// Copyright (C) 2016 Pietro Albini
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

use chan;
use nickel;
use nickel::{Nickel, HttpRouter, Options};
use nickel::status::StatusCode;
use hyper::method::Method;
use hyper::uri::RequestUri;
use url::form_urlencoded;
use rustc_serialize::json::{Json, ToJson};

use hooks::Hook;
use processor::{HealthDetails, Request, Job, ProcessorInput, SenderChan};


enum JsonResponse {
    NotFound,
    Forbidden,
    Ok,
    HealthStatus(HealthDetails),
}

impl ToJson for JsonResponse {

    fn to_json(&self) -> Json {
        let mut map = BTreeMap::new();

        map.insert("status".to_string(), match *self {
            JsonResponse::NotFound => "not_found",
            JsonResponse::Forbidden => "forbidden",
            JsonResponse::Ok => "ok",
            JsonResponse::HealthStatus(..) => "ok"
        }.to_string().to_json());

        if let JsonResponse::HealthStatus(ref details) = *self {
            map.insert("result".to_string(), details.to_json());
        }

        Json::Object(map)
    }
}


pub struct WebAPI {
    stop_chan: Option<chan::Sender<()>>,
    sender_chan: Option<SenderChan>,
    stop: bool,
    hooks: HashMap<String, Hook>,
}

impl WebAPI {

    pub fn new(hooks: HashMap<String, Hook>) -> WebAPI {
        WebAPI {
            stop_chan: None,
            sender_chan: None,
            stop: false,
            hooks: hooks,
        }
    }

    pub fn listen(&mut self, bind: &str, enable_health: bool,
                  sender: SenderChan) -> Result<SocketAddr, String> {
        // Store the sender channel
        self.sender_chan = Some(sender);

        // This is to fix lifetime issues with the thread below
        let bind = bind.to_string();

        let app = self.configure_nickel(enable_health);

        // This channel is used so it's possible to stop the listener
        let (send_stop, recv_stop) = chan::sync(0);
        self.stop_chan = Some(send_stop);

        // This channel is used to receive the result from the thread, which
        // will be returned
        let (return_send, return_recv) = chan::async();

        ::std::thread::spawn(move || {
            let bind: &str = &bind;
            match app.listen(bind) {
                Ok(listener) => {
                    // Send the socket address to the main thread
                    let sock = listener.socket();
                    return_send.send(Ok(sock));

                    // This blocks until someone sends something to
                    // self.stop_chan
                    recv_stop.recv();

                    // Detach the webserver from the current thread, allowing
                    // the process to exit
                    listener.detach();
                },
                Err(error) => {
                    // Send the error
                    return_send.send(Err(format!("{}", error)));
                }
            }
        });

        // Return what the thread sends
        return_recv.recv().unwrap()
    }

    pub fn stop(&mut self) -> bool {
        // Don't try to stop twice
        if self.stop {
            return true;
        }

        match self.stop_chan {
            Some(ref stop_chan) => {
                // Tell the thread to stop
                stop_chan.send(());

                self.stop = true;
                true
            },
            None => false,
        }
    }

    fn configure_nickel(&self, enable_health: bool) -> Nickel {
        let mut app = Nickel::new();

        // Disable the default message nickel prints on stdout
        app.options = Options::default().output_on_listen(false);

        for method in &[Method::Get, Method::Post] {
            // Make the used things owned
            let method = method.clone();
            let sender = self.sender_chan.clone().unwrap();
            let hooks = self.hooks.clone();

            // This middleware processes incoming hooks
            app.add_route(method, "/hook/:hook", middleware! { |req, mut res|
                let hook_name = req.param("hook").unwrap().to_string();

                // Ignore requests without a valid hook
                if hook_name == "".to_string() {
                    return res.next_middleware();
                }

                // Ignore requests with non-existent hooks
                let hook: Hook;
                if let Some(found) = hooks.get(&hook_name) {
                    hook = found.clone();
                } else {
                    return res.next_middleware();
                }

                let request = convert_request(&req);

                if let Some(job_hook) = hook.validate(&request.clone()) {
                    // If the hook is valid, create a new job and queue it
                    let job = Job::new(job_hook, request);
                    sender.send(ProcessorInput::Job(job));

                    JsonResponse::Ok.to_json()
                } else {
                    // Else send a great 403 Forbidden
                    res.set(StatusCode::Forbidden);
                    JsonResponse::Forbidden.to_json()
                }
            });
        }


        // Health reporting can be disabled by the user
        if enable_health {
            let sender = self.sender_chan.clone().unwrap();

            app.get("/health", middleware! {
                let (details_send, details_recv) = chan::async();

                // Get the details from the processor
                sender.send(ProcessorInput::HealthStatus(details_send));
                let details = details_recv.recv().unwrap();

                JsonResponse::HealthStatus(details).to_json()
            });
        } else {
            app.get("/health", middleware! { |_req, mut res|
                res.set(StatusCode::Forbidden);

                JsonResponse::Forbidden.to_json()
            });
        }


        // This middleware provides a basic Not found page
        app.utilize(middleware! { |_req, mut res|
            res.set(StatusCode::NotFound);

            JsonResponse::NotFound.to_json()
        });

        app
    }

}


fn convert_request(req: &nickel::Request) -> Request {
    let source = req.origin.remote_addr.clone();

    // Convert headers from the hyper representation to strings
    let mut headers = HashMap::new();
    for header in req.origin.headers.iter() {
        headers.insert(header.name().to_string(), header.value_string());
    }

    let params = params_from_request(req);

    Request {
        source: source,
        headers: headers,
        params: params,
    }
}


fn params_from_request(req: &nickel::Request) -> HashMap<String, String> {
    let ref uri = req.origin.uri;

    let query_string = match *uri {
        RequestUri::AbsoluteUri(ref url) => Some(url.query()),
        RequestUri::AbsolutePath(ref s) => Some(s.splitn(2, '?').nth(1)),
        _ => None,
    };

    match query_string {
        Some(path) => {
            // Don't do anything if there is no query string
            if path.is_none() {
                return HashMap::new();
            }
            let path = path.unwrap();

            let mut hashmap = HashMap::new();
            for (a, b) in form_urlencoded::parse(path.as_bytes()).into_owned() {
                hashmap.insert(a, b);
            }
            hashmap
        },
        None => HashMap::new(),
    }
}


#[cfg(test)]
mod tests {

    mod json_responses {
        use rustc_serialize::json::ToJson;

        use processor::HealthDetails;
        use super::super::JsonResponse;


        #[test]
        fn test_not_found() {
            let response = JsonResponse::NotFound.to_json();

            // The result must be an object
            let obj = response.as_object().unwrap();

            // The status must be "not_found"
            assert_eq!(
                obj.get("status").unwrap().as_string().unwrap(),
                "not_found".to_string()
            );
        }


        #[test]
        fn test_forbidden() {
            let response = JsonResponse::Forbidden.to_json();

            // The result must be an object
            let obj = response.as_object().unwrap();

            // The status must be "forbidden"
            assert_eq!(
                obj.get("status").unwrap().as_string().unwrap(),
                "forbidden".to_string()
            );
        }


        #[test]
        fn test_ok() {
            let response = JsonResponse::Ok.to_json();

            // The result must be an object
            let obj = response.as_object().unwrap();

            // The status must be "ok"
            assert_eq!(
                obj.get("status").unwrap().as_string().unwrap(),
                "ok".to_string()
            );
        }


        #[test]
        fn test_health_status() {
            let response = JsonResponse::HealthStatus(HealthDetails {
                active_jobs: 1,
                queue_size: 2,
            }).to_json();

            // The result must be an object
            let obj = response.as_object().unwrap();

            // The status must be "ok"
            assert_eq!(
                obj.get("status").unwrap().as_string().unwrap(),
                "ok".to_string()
            );

            // It must have an object called "result"
            let result = obj.get("result").unwrap().as_object().unwrap();

            // The result must contain "active_jobs" and "queue_size"
            assert_eq!(
                result.get("active_jobs").unwrap().as_u64().unwrap(),
                1 as u64
            );
            assert_eq!(
                result.get("queue_size").unwrap().as_u64().unwrap(),
                2 as u64
            );
        }

    }
}

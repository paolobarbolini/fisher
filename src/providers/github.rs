// Copyright (C) 2016-2017 Pietro Albini
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

use hmac::{Hmac, Mac};
use serde_derive::Deserialize;

use crate::common::prelude::*;
use crate::providers::prelude::*;
use crate::utils;

const GITHUB_EVENTS: &[&'static str] = &[
    "commit_comment",
    "create",
    "delete",
    "deployment",
    "deployment_status",
    "fork",
    "gollum",
    "issue_comment",
    "issues",
    "label",
    "member",
    "membership",
    "milestone",
    "organization",
    "page_build",
    "project_card",
    "project_column",
    "project",
    "public",
    "pull_reques_review_comment",
    "pull_request_review",
    "pull_request",
    "push",
    "repository",
    "release",
    "status",
    "team",
    "team_add",
    "watch",
];
const GITHUB_HEADERS: &[&'static str] =
    &["X-GitHub-Event", "X-Hub-Signature", "X-GitHub-Delivery"];

#[derive(Deserialize)]
struct PushEvent<'src> {
    #[serde(rename = "ref")]
    git_ref: &'src str,
    head_commit: PushCommit<'src>,
}

#[derive(Deserialize)]
struct PushCommit<'src> {
    id: &'src str,
}

#[derive(Debug, Deserialize)]
pub struct GitHubProvider {
    secret: Option<String>,
    events: Option<Vec<String>>,
}

impl ProviderTrait for GitHubProvider {
    fn new(input: &str) -> Result<GitHubProvider> {
        let inst: GitHubProvider = serde_json::from_str(input)?;

        if let Some(ref events) = inst.events {
            // Check if the events exists
            for event in events {
                if !GITHUB_EVENTS.contains(&event.as_ref()) {
                    // Return an error if the event doesn't exist
                    return Err(ErrorKind::ProviderGitHubInvalidEventName(
                        event.clone(),
                    )
                    .into());
                }
            }
        }

        Ok(inst)
    }

    fn validate(&self, request: &Request) -> RequestType {
        let req;
        if let Request::Web(ref inner) = *request {
            req = inner;
        } else {
            return RequestType::Invalid;
        }

        // Check if the correct headers are present
        for header in GITHUB_HEADERS.iter() {
            if !req.headers.contains_key(*header) {
                return RequestType::Invalid;
            }
        }

        // Check the signature only if a secret key was provided
        if let Some(ref secret) = self.secret {
            // Check if the signature is valid
            let signature = &req.headers["X-Hub-Signature"];
            if !verify_signature(secret, &req.body, signature) {
                return RequestType::Invalid;
            }
        }

        // Check if the event is valid
        let event = &req.headers["X-GitHub-Event"];
        if !(GITHUB_EVENTS.contains(&event.as_ref()) || *event == "ping") {
            return RequestType::Invalid;
        }

        // Check if the event should be accepted
        if let Some(ref events) = self.events {
            if !(events.contains(event) || *event == "ping") {
                return RequestType::Invalid;
            }
        }

        // Check if the JSON in the body is valid
        if serde_json::from_str::<serde_json::Value>(&req.body).is_err() {
            return RequestType::Invalid;
        }

        // The "ping" event is a ping (doh!)
        if event == "ping" {
            return RequestType::Ping;
        }

        // Process the hook in the other cases
        RequestType::ExecuteHook
    }

    fn build_env(&self, r: &Request, b: &mut EnvBuilder) -> Result<()> {
        let req;
        if let Request::Web(ref inner) = *r {
            req = inner;
        } else {
            return Ok(());
        }

        b.add_env("EVENT", &req.headers["X-GitHub-Event"]);
        b.add_env("DELIVERY_ID", &req.headers["X-GitHub-Delivery"]);

        // Add specific environment variables for the `push` event
        let event = &req.headers["X-GitHub-Event"];
        if self.events.as_ref().map_or(false, |e| e.contains(event))
            && *event == "push"
        {
            let parsed: PushEvent = serde_json::from_str(&req.body)?;
            b.add_env("PUSH_REF", parsed.git_ref);
            b.add_env("PUSH_HEAD", parsed.head_commit.id);
        }

        Ok(())
    }
}

fn verify_signature(secret: &str, payload: &str, raw_signature: &str) -> bool {
    type HmacSha1 = Hmac<sha1::Sha1>;

    // The signature must have a =
    if !raw_signature.contains('=') {
        return false;
    }

    // Split the raw signature to get the algorithm and the signature
    let splitted: Vec<&str> = raw_signature.split('=').collect();
    let algorithm = &splitted[0];
    let hex_signature = splitted
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<&str>>()
        .join("=");

    // Convert the signature from hex
    let signature = if let Ok(converted) = utils::from_hex(&hex_signature) {
        converted
    } else {
        // This is not hex
        return false;
    };

    // Only SHA-1 is supported
    if *algorithm != "sha1" {
        return false;
    }

    // Verify the HMAC signature
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::providers::ProviderTrait;
    use crate::requests::RequestType;
    use crate::scripts::EnvBuilder;
    use crate::utils::testing::*;
    use crate::web::WebRequest;

    use super::{verify_signature, GitHubProvider, GITHUB_EVENTS};

    #[test]
    fn test_new() {
        // Check for right configurations
        for right in &[
            r#"{}"#,
            r#"{"secret": "abcde"}"#,
            r#"{"events": ["push", "fork"]}"#,
            r#"{"secret": "abcde", "events": ["push", "fork"]}"#,
        ] {
            assert!(GitHubProvider::new(right).is_ok(), "{}", right);
        }

        // Checks for wrong configurations
        for wrong in &[
            // Wrong types
            r#"{"secret": 12345}"#,
            r#"{"secret": true}"#,
            r#"{"events": 12345}"#,
            r#"{"events": true}"#,
            r#"{"events": {}}"#,
            r#"{"events": [12345]}"#,
            r#"{"events": [true]}"#,
            r#"{"events": ["invalid_event"]}"#,
        ] {
            assert!(GitHubProvider::new(wrong).is_err(), "{}", wrong);
        }
    }

    #[test]
    fn test_request_type() {
        let provider = GitHubProvider::new("{}").unwrap();

        // This helper gets the request type of an event
        macro_rules! assert_req_type {
            ($provider:expr, $event:expr, $expected:expr) => {
                let mut request = dummy_web_request();
                let _ = request
                    .headers
                    .insert("X-GitHub-Event".into(), $event.to_string());
                let _ = request
                    .headers
                    .insert("X-GitHub-Delivery".into(), "12345".into());
                let _ = request
                    .headers
                    .insert("X-Hub-Signature".into(), "invalid".into());
                request.body = "{}".into();

                assert_eq!($provider.validate(&request.into()), $expected);
            };
        }

        assert_req_type!(provider, "ping", RequestType::Ping);
        for event in GITHUB_EVENTS.iter() {
            assert_req_type!(provider, event, RequestType::ExecuteHook);
        }
    }

    #[test]
    fn test_build_env() {
        let mut req = dummy_web_request();
        req.headers.insert("X-GitHub-Event".into(), "ping".into());
        req.headers
            .insert("X-GitHub-Delivery".into(), "12345".into());

        let provider = GitHubProvider::new("{}").unwrap();
        let mut b = EnvBuilder::dummy();
        provider.build_env(&req.into(), &mut b).unwrap();

        assert_eq!(
            b.dummy_data().env,
            hashmap! {
                "EVENT".into() => "ping".into(),
                "DELIVERY_ID".into() => "12345".into(),
            }
        );
        assert_eq!(b.dummy_data().files, hashmap!());
    }

    fn dummy_push_event_request(event: &str) -> WebRequest {
        let mut req = dummy_web_request();

        req.headers
            .insert("X-GitHub-Delivery".into(), "12345".into());
        req.headers.insert("X-GitHub-Event".into(), event.into());
        req.body = ::serde_json::to_string(&json!({
            "ref": "refs/heads/master",
            "head_commit": json!({
                "id": "deadbeef",
            }),
        }))
        .unwrap();

        req
    }

    #[test]
    fn test_build_env_event_push_wrong_event() {
        let req = dummy_push_event_request("ping");
        let provider =
            GitHubProvider::new(r#"{"events": ["create", "push"]}"#).unwrap();

        let mut b = EnvBuilder::dummy();
        provider.build_env(&req.into(), &mut b).unwrap();

        assert_eq!(b.dummy_data().env.get("PUSH_REF"), None);
        assert_eq!(b.dummy_data().env.get("PUSH_HEAD"), None);
    }

    #[test]
    fn test_build_env_event_push_no_whitelist() {
        let req = dummy_push_event_request("push");
        let provider = GitHubProvider::new("{}").unwrap();

        let mut b = EnvBuilder::dummy();
        provider.build_env(&req.into(), &mut b).unwrap();

        assert_eq!(b.dummy_data().env.get("PUSH_REF"), None);
        assert_eq!(b.dummy_data().env.get("PUSH_HEAD"), None);
    }

    #[test]
    fn test_build_env_event_push_correct() {
        let req = dummy_push_event_request("push");
        let provider = GitHubProvider::new(r#"{"events": ["push"]}"#).unwrap();

        let mut b = EnvBuilder::dummy();
        provider.build_env(&req.into(), &mut b).unwrap();

        assert_eq!(
            b.dummy_data().env.get("PUSH_REF"),
            Some(&"refs/heads/master".into())
        );
        assert_eq!(
            b.dummy_data().env.get("PUSH_HEAD"),
            Some(&"deadbeef".into())
        );
    }

    #[test]
    fn test_verify_signature() {
        // Check if the function allows invalid signatures
        for signature in &[
            "invalid",         // No algorithm
            "invalid=invalid", // Invalid algorithm
            "sha1=g",          // The signature is not hex
            // Invalid signature (the first "e" should be "f")
            "sha1=e75efc0f29bf50c23f99b30b86f7c78fdaf5f11d",
        ] {
            assert!(
                !verify_signature("secret", "payload", signature),
                "{}",
                signature
            );
        }

        // This is known to be right
        assert!(verify_signature(
            "secret",
            "payload",
            "sha1=f75efc0f29bf50c23f99b30b86f7c78fdaf5f11d"
        ));
    }
}

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::server::{Outcome, Status};

pub struct Client {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(base: &str, token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .build();
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    pub fn post(&self, path: &str, body: &impl Serialize) -> anyhow::Result<Outcome> {
        let request = self.authorize(self.agent.post(&format!("{}{path}", self.base)));
        self.read(request.send_json(body))
    }

    pub fn status(&self) -> anyhow::Result<Status> {
        let request = self.authorize(self.agent.get(&format!("{}/v1/status", self.base)));
        self.read(request.call())
    }

    fn authorize(&self, request: ureq::Request) -> ureq::Request {
        match &self.token {
            Some(token) => request.set("Authorization", &format!("Bearer {token}")),
            None => request,
        }
    }

    fn read<T: DeserializeOwned>(
        &self,
        response: Result<ureq::Response, ureq::Error>,
    ) -> anyhow::Result<T> {
        match response {
            Ok(response) => response
                .into_json()
                .context("cannot read the server response"),
            Err(ureq::Error::Status(code, response)) => {
                let message = response
                    .into_json::<serde_json::Value>()
                    .ok()
                    .and_then(|body| body["error"].as_str().map(str::to_string))
                    .unwrap_or_default();
                Err(anyhow!(
                    "the server refused the request (HTTP {code}): {message}"
                ))
            }
            Err(error) => Err(anyhow!("cannot reach the server at {}: {error}", self.base)),
        }
    }
}

//! Blocking HTTP client for the sync server (`sync-server/`).
//!
//! The calls are blocking (`ureq`); [`run_blocking`] moves them off the async
//! runtime's worker threads, so the CLI and TUI can call them from async code.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::db::Id;

use super::types::{RemoteEvent, SyncEvent};

/// Where `wrangler dev` serves the backend locally.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";

/// The server address: `IM_SYNC_URL` when set, else the local default.
pub fn server_url() -> String {
    std::env::var("IM_SYNC_URL").unwrap_or_else(|_| DEFAULT_SERVER.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub user_id: String,
    pub email: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountStatus {
    pub user_id: String,
    pub email: String,
}

#[derive(Debug, Serialize)]
struct SyncRequest {
    device_id: Id,
    since_version: i64,
    client_events: Vec<SyncEvent>,
}

#[derive(Debug, Deserialize)]
pub struct SyncOutcome {
    pub new_server_version: i64,
    /// The server had more events than the page it just returned.
    pub has_more: bool,
    pub remote_events: Vec<RemoteEvent>,
}

/// A client for one server, optionally carrying a bearer token.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    agent: ureq::Agent,
    token: Option<String>,
}

impl Client {
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self::with_timeout(base, token, Duration::from_secs(10))
    }

    pub fn with_timeout(base: &str, token: Option<String>, timeout: Duration) -> Self {
        // Status codes are read by hand so the server's `{ error }` message
        // reaches the user instead of a bare "http status: 409".
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build();
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent: ureq::Agent::new_with_config(config),
            token,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn register(&self, email: &str, password: &str) -> Result<Account> {
        let body = serde_json::json!({ "email": email, "password": password });
        self.send("POST", "/api/v1/auth/register", Some(&body), false)
    }

    pub fn login(&self, email: &str, password: &str) -> Result<Account> {
        let body = serde_json::json!({ "email": email, "password": password });
        self.send("POST", "/api/v1/auth/login", Some(&body), false)
    }

    pub fn status(&self) -> Result<AccountStatus> {
        self.send("GET", "/api/v1/auth/status", None, true)
    }

    /// Push the outbox and pull one page of what this device has not seen yet.
    pub fn sync(
        &self,
        device_id: Id,
        since_version: i64,
        events: &[SyncEvent],
    ) -> Result<SyncOutcome> {
        let request = SyncRequest {
            device_id,
            since_version,
            client_events: events.to_vec(),
        };
        let body = serde_json::to_value(&request).context("Failed to serialize the push")?;
        self.send("POST", "/api/v1/sync", Some(&body), true)
    }

    fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        authorized: bool,
    ) -> Result<T> {
        let url = format!("{}{path}", self.base);
        let authorization = self
            .token
            .as_deref()
            .filter(|_| authorized)
            .map(|token| format!("Bearer {token}"));
        if authorized && authorization.is_none() {
            bail!("not logged in — run `im :auth login`");
        };

        // Status codes are read by hand so the server's `{ error }` message
        // reaches the user instead of a bare "http status: 409".
        let mut response = match method {
            "GET" => {
                let mut request = self.agent.get(&url);
                if let Some(value) = &authorization {
                    request = request.header("authorization", value);
                }
                request.call()
            }
            _ => {
                let mut request = self.agent.post(&url);
                if let Some(value) = &authorization {
                    request = request.header("authorization", value);
                }
                match body {
                    Some(body) => request.send_json(body),
                    None => request.send_empty(),
                }
            }
        }
        .with_context(|| format!("Failed to reach the sync server at {}", self.base))?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .context("Failed to read the sync server's response")?;
        if !(200..300).contains(&status) {
            bail!("{}", server_error_message(&text, status));
        }
        serde_json::from_str(&text).context("The sync server returned an unexpected response")
    }
}

/// The server's `{ error }` message, or a plain status description.
fn server_error_message(body: &str, status: u16) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("The sync server replied with HTTP {status}"))
}

/// Run a blocking client call away from the async runtime's worker threads.
pub async fn run_blocking<T, F>(call: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(call)
        .await
        .context("The sync task panicked")?
}

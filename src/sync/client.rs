//! Blocking HTTP client for the sync server (`sync-server/`).
//!
//! The calls are blocking (`ureq`); [`run_blocking`] moves them off the async
//! runtime's worker threads, so the CLI and TUI can call them from async code.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Config;

use super::types::{SyncRequest, SyncResponse};

/// Where `wrangler dev` serves the backend locally.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";

/// The server address: `IM_SYNC_URL` when set (and non-empty), else `[sync]
/// server` from the config, else the local default.
pub fn server_url(config: &Config) -> String {
    let from_env = std::env::var("IM_SYNC_URL").unwrap_or_default();
    let from_env = from_env.trim();
    if !from_env.is_empty() {
        return from_env.to_string();
    }
    config
        .sync
        .server
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_SERVER)
        .to_string()
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

    /// Push the outbox and pull every cell above the cursor.
    ///
    /// A refused database is an error, not a response: the caller cannot merge
    /// a log it never agreed to read, and the user has to choose what to do
    /// about it. See [`Refusal`].
    pub fn sync(&self, request: &SyncRequest) -> Result<SyncResponse> {
        let body = serde_json::to_value(request).context("Failed to serialize the push")?;
        let (status, text) = self.exchange("POST", "/api/sync", Some(&body), true)?;
        if let Some(refusal) = Refusal::parse(status, &text, self.base(), request) {
            return Err(refusal.into());
        }
        if !(200..300).contains(&status) {
            bail!("{}", server_error_message(&text, status));
        }
        serde_json::from_str(&text).context("The sync server returned an unexpected response")
    }

    fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        authorized: bool,
    ) -> Result<T> {
        let (status, text) = self.exchange(method, path, body, authorized)?;
        if !(200..300).contains(&status) {
            bail!("{}", server_error_message(&text, status));
        }
        serde_json::from_str(&text).context("The sync server returned an unexpected response")
    }

    /// One request, status and body read by hand so the server's `{ error }`
    /// message reaches the user instead of a bare "http status: 409".
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        authorized: bool,
    ) -> Result<(u16, String)> {
        let url = format!("{}{path}", self.base);
        let authorization = self
            .token
            .as_deref()
            .filter(|_| authorized)
            .map(|token| format!("Bearer {token}"));
        if authorized && authorization.is_none() {
            bail!("not logged in — run `im :sync login`");
        };

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
        Ok((status, text))
    }
}

/// Why a server refused this database outright.
///
/// These are not merge decisions: the round trip carried no cells in either
/// direction. A cursor only means something inside the log that issued it, so a
/// database pointed at another log, at a rewound one, or at a build that speaks
/// another protocol has nothing it can safely do without the user deciding.
#[derive(Debug, Clone)]
pub enum Refusal {
    /// The cursor names a log this server does not have.
    LogMismatch {
        base: String,
        /// The log this device's cursor belongs to.
        local: String,
        server_log: String,
        version: i64,
        head: Option<i64>,
    },
    /// The cursor is past the end of the server's log: the same log, rewound by
    /// a restore, so the versions this device consumed mean other cells now.
    CursorAhead {
        base: String,
        server_log: String,
        version: i64,
        head: Option<i64>,
    },
    /// The two builds disagree on the envelope or on the cell vocabulary.
    Version {
        protocol: u32,
        schema: u32,
        server_protocol: u32,
        server_schema: u32,
    },
}

impl Refusal {
    fn parse(status: u16, body: &str, base: &str, request: &SyncRequest) -> Option<Self> {
        if status != 409 {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        let code = value.get("code")?.as_str()?;
        let number = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .map(|number| number as u32)
        };
        let log = || {
            value
                .get("log_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let head = || value.get("head").and_then(serde_json::Value::as_i64);
        match code {
            "log_mismatch" => {
                // Only a named cursor can belong to another log.
                let cursor = request.cursor.as_ref()?;
                Some(Self::LogMismatch {
                    base: base.to_string(),
                    local: cursor.log.clone(),
                    server_log: log()?,
                    version: cursor.version,
                    head: head(),
                })
            }
            "cursor_ahead" => {
                let cursor = request.cursor.as_ref()?;
                Some(Self::CursorAhead {
                    base: base.to_string(),
                    server_log: log()?,
                    version: cursor.version,
                    head: head(),
                })
            }
            "version_mismatch" => Some(Self::Version {
                protocol: super::schema::SCHEMA.protocol,
                schema: super::schema::SCHEMA.version,
                server_protocol: number("protocol")?,
                server_schema: number("schema")?,
            }),
            _ => None,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = |log: &str| log.chars().take(8).collect::<String>();
        match self {
            Self::LogMismatch {
                base,
                local,
                server_log,
                version,
                head,
            } => {
                let head = head.map_or_else(|| "unknown".to_string(), |head| head.to_string());
                write!(
                    f,
                    "This database last synced to a different server log.\n  \
                     this device: cursor {version} in log {}\n  \
                     {base}: log {} (head {head})\n\
                     Nothing was sent or applied. `im :sync --reset` rebuilds this database from that server; \
                     pointing `[sync] server` back at the old one keeps this data.",
                    short(local),
                    short(server_log),
                )
            }
            Self::CursorAhead {
                base,
                server_log,
                version,
                head,
            } => {
                let head = head.map_or_else(|| "an unknown version".to_string(), |head| head.to_string());
                write!(
                    f,
                    "This database has read to version {version} of the server's log, but {base} ends at \
                     {head} (log {}): the server's log was reset or restored from an older copy.\n\
                     Nothing was sent or applied. `im :sync --reset` rebuilds this database from that server.",
                    short(server_log),
                )
            }
            Self::Version {
                protocol,
                schema,
                server_protocol,
                server_schema,
            } => write!(
                f,
                "The server speaks sync protocol {server_protocol}, schema {server_schema}; this build of \
                 im speaks protocol {protocol}, schema {schema}.\n\
                 Nothing was sent or applied. Upgrade im (or the server) until they agree.",
            ),
        }
    }
}

impl std::error::Error for Refusal {}

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

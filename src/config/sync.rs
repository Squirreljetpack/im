use serde::{Deserialize, Serialize};

/// `[sync]` section — where this device reaches the sync server.
///
/// This is the base URL for every server call: `im :auth` appends
/// `/api/v1/auth/...` and `im :sync` appends `/api/sync`. The URL is not part
/// of the account: signing in does not record it, so a config shared by
/// several machines can point them all at the same server.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Base URL of the sync server, e.g. `https://im-sync.example.workers.dev`.
    /// Unset means the local development server (`http://127.0.0.1:8787`);
    /// `IM_SYNC_URL` overrides this (for `:auth` and `:sync`) per command.
    pub server: Option<String>,
}

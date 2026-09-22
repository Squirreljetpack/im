//! Offline-first multi-device sync (`@@SYNC.md`).
//!
//! The client half of the event stream: [`types`] is the wire format,
//! [`state`] holds the device identity/clocks/LWW watermark, [`events`]
//! appends the outbox rows that mutations produce, and [`apply`] replays
//! incoming events (with the promptable text and parent conflicts of §4.2).

pub mod apply;
pub mod client;
pub mod events;
pub mod session;
pub mod state;
pub mod types;

#[cfg(test)]
mod tests;

pub use apply::{Applied, Conflict, ConflictKind, PageOutcome, Resolution};
pub use client::{Account, AccountStatus, Client, DEFAULT_SERVER, server_url};
pub use session::{SyncReport, sync_once};
pub use state::{
    KEY_AUTH_TOKEN, KEY_DEVICE_ID, KEY_LAST_SERVER_VERSION, KEY_USER_ID, next_event_timestamp,
    now_ms,
};
pub use types::{
    Change, CompletionData, EntityPayload, Event, MoodCreateData, MoodUpdate, MoodUpdateData,
    RemoteEvent, SyncEvent, TaskCreateData, TaskUpdate, TaskUpdateData, TrackerData, TrackerScore,
    TrackerUpdate, TrackerUpdateData, is_note_field, is_text_field, task_field,
};

//! Offline-first multi-device sync (`@@SYNC.md`).
//!
//! The client half of the event stream: [`types`] is the wire format,
//! [`state`] holds the device identity/clocks/LWW watermark, [`events`]
//! appends the outbox rows that mutations produce, and [`apply`] replays
//! incoming events (with the two promptable delete conflicts of §4.3).

pub mod apply;
pub mod events;
pub mod state;
pub mod types;

#[cfg(test)]
mod tests;

pub use apply::{ApplyOutcome, Conflict, ConflictKind, RemoteEvent, Resolution};
pub use events::to_json;
pub use state::{
    KEY_AUTH_TOKEN, KEY_DEVICE_ID, KEY_LAST_SERVER_VERSION, KEY_USER_ID, next_event_timestamp,
    now_ms,
};
pub use types::{
    CompletionData, EntityPayload, MoodData, SyncEvent, TaskData, TrackerData, TrackerScore,
};

//! Offline-first multi-device sync (`@@SYNC_PLAN.md`).
//!
//! One event writes one cell; the server decides which write owns each cell and
//! hands back the winners, so the merge here is a projection rather than an
//! arbitration. [`types`] is the wire format, [`schema`] the shared rules
//! table, [`state`] the device id and cursor, [`events`] the outbox, [`apply`]
//! the write path and [`session`] the round trip.

pub mod apply;
pub mod client;
pub mod events;
pub mod schema;
pub mod session;
pub mod state;
pub mod types;

#[cfg(test)]
mod tests;

pub use client::{Account, AccountStatus, Client, DEFAULT_SERVER, server_url};
pub use session::{Pull, SyncReport, sync_once};
pub use state::{KEY_DEVICE_ID, next_stamp, now_ms};
pub use types::{
    CellEvent, CellValue, LogRow, MoodUpdate, Outcome, SyncRequest, SyncResponse, TaskUpdate,
    TrackerUpdate, Verdict, completion_field, mood_field, task_field, tracker_field,
};

//! Wire types of the sync event stream (`@@SYNC.md` §3).
//!
//! One event describes exactly one entity: `payload: Some(..)` is an upsert
//! carrying a full snapshot, `payload: None` is a delete. Ids serialize as
//! hyphenated uuid strings — [`crate::db::Id`] is `serde(transparent)` over
//! `uuid::Uuid`, so the JSON matches the spec's `Uuid` fields.

use serde::{Deserialize, Serialize};

use crate::db::{EventId, Id, TrackerValue};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncEvent {
    /// Globally unique per mutation.
    pub event_id: EventId,
    /// The entity's stable id.
    pub entity_id: Id,
    /// The device that authored the event.
    pub device_id: Id,
    /// Unix epoch **milliseconds**, strictly increasing per device (see
    /// [`super::state::next_event_timestamp`]).
    pub timestamp: i64,
    /// `Some` = upsert snapshot, `None` = delete.
    pub payload: Option<EntityPayload>,
}

/// One row of the server's log: an [`SyncEvent`] plus its arrival `version`,
/// which orders the pull (`@@SYNC.md` §4.5, §5.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteEvent {
    pub version: i64,
    #[serde(flatten)]
    pub event: SyncEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum EntityPayload {
    Mood(MoodData),
    Tracker(TrackerData),
    Task(TaskData),
    Completion(CompletionData),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoodData {
    pub mood: String,
    pub body: String,
    pub time: i64,
    /// Cached emotional saliency; recomputed locally when absent.
    pub score: Option<f32>,
    pub duration: Option<i64>,
    pub todo_id: Option<Id>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackerData {
    pub tracker_type: String,
    pub score: TrackerScore,
    pub time: i64,
    pub mood_id: Option<Id>,
}

/// A tracker value with its storage class preserved (the `tracker.score`
/// column is dynamically typed — see [`crate::db::TrackerValue`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value")]
pub enum TrackerScore {
    Integer(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskData {
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: Option<i64>,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`].
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: bool,
    pub end_time: Option<i64>,
    pub parent_id: Option<Id>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletionData {
    pub todo_id: Id,
    pub time: i64,
    pub count: i32,
}

impl From<&TrackerValue> for TrackerScore {
    fn from(value: &TrackerValue) -> Self {
        match value {
            TrackerValue::Text(text) => TrackerScore::Text(text.clone()),
            TrackerValue::Integer(number) => TrackerScore::Integer(*number),
            TrackerValue::Float(float) => TrackerScore::Float(*float),
        }
    }
}

impl TrackerScore {
    pub fn to_value(&self) -> TrackerValue {
        match self {
            TrackerScore::Text(text) => TrackerValue::Text(text.clone()),
            TrackerScore::Integer(number) => TrackerValue::Integer(*number),
            TrackerScore::Float(float) => TrackerValue::Float(*float),
        }
    }

    /// The entity kind of the payload, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            TrackerScore::Text(_) => "text",
            TrackerScore::Integer(_) => "integer",
            TrackerScore::Float(_) => "float",
        }
    }
}

impl EntityPayload {
    /// The entity kind, for reports and conflict messages.
    pub fn kind(&self) -> &'static str {
        match self {
            EntityPayload::Mood(_) => "mood",
            EntityPayload::Tracker(_) => "tracker",
            EntityPayload::Task(_) => "task",
            EntityPayload::Completion(_) => "completion",
        }
    }
}

impl SyncEvent {
    pub fn is_delete(&self) -> bool {
        self.payload.is_none()
    }
}

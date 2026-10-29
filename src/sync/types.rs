//! Wire types of the sync protocol.
//!
//! One event writes one **cell** — one field of one entity. A create is the
//! entity's existence cell plus a burst of field cells; a delete is the
//! existence cell written `deleted`. The author's `timestamp` decides which
//! write owns a cell; the server's append-order `version` decides delivery.

use serde::{Deserialize, Serialize};

use crate::db::{EventId, Id, TrackerValue};

/// The column names of `todos` that a cell can carry.
pub mod task_field {
    pub const NAME: &str = "name";
    pub const BODY: &str = "body";
    pub const PRIORITY: &str = "priority";
    pub const START_TIME: &str = "start_time";
    pub const AVAILABLE_DURATION_SECS: &str = "available_duration_secs";
    pub const INTERVAL_SECS: &str = "interval_secs";
    pub const TARGET_COUNT: &str = "target_count";
    pub const OPTIONAL: &str = "optional";
    pub const END_TIME: &str = "end_time";
    pub const PARENT_ID: &str = "parent_id";
}

/// The column names of `mood` that a cell can carry.
pub mod mood_field {
    pub const MOOD: &str = "mood";
    pub const BODY: &str = "body";
    pub const TIME: &str = "time";
    pub const DURATION: &str = "duration";
    pub const TODO_ID: &str = "todo_id";
}

/// The column names of `tracker` that a cell can carry.
pub mod tracker_field {
    pub const TRACKER_TYPE: &str = "type";
    pub const SCORE: &str = "score";
    pub const TIME: &str = "time";
    pub const MOOD_ID: &str = "mood_id";
}

/// The column names of `todo_completions` that a cell can carry.
pub mod completion_field {
    pub const TODO_ID: &str = "todo_id";
    pub const TIME: &str = "time";
    pub const COUNT: &str = "count";
}

/// The reserved field that carries an entity's existence (`live` / `deleted`).
/// No real column can be named `""`, and `NULL` would break the uniqueness of
/// `(entity_id, field)`.
pub const EXISTENCE: &str = "";

pub const LIVE: &str = "live";
pub const DELETED: &str = "deleted";

/// One cell value with its storage class kept explicit: a tracker score of `3`
/// and `3.0` are different rows in a dynamically typed column, so a bare JSON
/// number would lose the distinction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellValue {
    Int(i64),
    Real(f64),
    Text(String),
}

impl CellValue {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub fn int(value: impl Into<i64>) -> Self {
        Self::Int(value.into())
    }

    pub fn real(value: f64) -> Self {
        Self::Real(value)
    }

    /// The storage class, for messages.
    pub fn class(&self) -> &'static str {
        match self {
            CellValue::Int(_) => "integer",
            CellValue::Real(_) => "float",
            CellValue::Text(_) => "text",
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            CellValue::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            CellValue::Int(number) => Some(*number),
            _ => None,
        }
    }
}

impl From<&TrackerValue> for CellValue {
    fn from(value: &TrackerValue) -> Self {
        match value {
            TrackerValue::Text(text) => CellValue::Text(text.clone()),
            TrackerValue::Integer(number) => CellValue::Int(*number),
            TrackerValue::Float(float) => CellValue::Real(*float),
        }
    }
}

/// The existence cell's value for a live entity.
pub fn live() -> Option<CellValue> {
    Some(CellValue::text(LIVE))
}

/// The existence cell's value for a dead one.
pub fn deleted() -> Option<CellValue> {
    Some(CellValue::text(DELETED))
}

/// One write to one cell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellEvent {
    /// Globally unique per event; `UNIQUE` on the server makes a retry a no-op.
    pub event_id: EventId,
    /// The device that authored the event.
    pub device_id: Id,
    /// The entity the cell belongs to.
    pub entity_id: Id,
    /// `task`, `mood`, `tracker` or `completion`.
    pub kind: String,
    /// The field, or [`EXISTENCE`].
    pub field: String,
    /// `None` is a null cell (a cleared field).
    pub value: Option<CellValue>,
    /// The author's stamp in milliseconds: the cell's LWW key.
    pub timestamp: i64,
}

/// A cell as the server holds it: the event plus the delivery position.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogRow {
    /// Append order. Decides delivery only, never who wins a cell.
    pub version: i64,
    #[serde(flatten)]
    pub cell: CellEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub device_id: Id,
    /// The highest `version` this device has merged.
    pub cursor: i64,
    pub events: Vec<CellEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncResponse {
    pub cursor: i64,
    /// The server's clock, for naming a `clock_ahead` offset.
    pub now: i64,
    pub has_more: bool,
    pub events: Vec<LogRow>,
    pub verdicts: Vec<Verdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Superseded,
    Rejected,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    pub event_id: EventId,
    pub outcome: Outcome,
    /// Why a push was refused, when it was.
    #[serde(default)]
    pub reason: Option<String>,
    /// The cell's winning event, when the push did not become it.
    #[serde(default)]
    pub winner: Option<LogRow>,
    /// The existence cell, when the entity is gone.
    #[serde(default)]
    pub tombstone: Option<LogRow>,
}

/// An edit of one task field.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskUpdate {
    Name(String),
    Body(String),
    Priority(i32),
    StartTime(Option<i64>),
    AvailableDurationSecs(Option<i64>),
    IntervalSecs(Option<i64>),
    TargetCount(i32),
    Optional(bool),
    EndTime(Option<i64>),
    ParentId(Option<Id>),
}

impl TaskUpdate {
    /// The cell this edit writes.
    pub fn cell(&self) -> (&'static str, Option<CellValue>) {
        match self {
            Self::Name(name) => (task_field::NAME, Some(CellValue::text(name))),
            Self::Body(body) => (task_field::BODY, Some(CellValue::text(body))),
            Self::Priority(priority) => (task_field::PRIORITY, Some(CellValue::int(*priority))),
            Self::StartTime(time) => (task_field::START_TIME, time.map(CellValue::Int)),
            Self::AvailableDurationSecs(secs) => {
                (task_field::AVAILABLE_DURATION_SECS, secs.map(CellValue::Int))
            }
            Self::IntervalSecs(secs) => (task_field::INTERVAL_SECS, secs.map(CellValue::Int)),
            Self::TargetCount(count) => (task_field::TARGET_COUNT, Some(CellValue::int(*count))),
            Self::Optional(optional) => (
                task_field::OPTIONAL,
                Some(CellValue::int(i64::from(*optional))),
            ),
            Self::EndTime(time) => (task_field::END_TIME, time.map(CellValue::Int)),
            Self::ParentId(parent) => (
                task_field::PARENT_ID,
                parent.map(|id| CellValue::text(id.to_string())),
            ),
        }
    }
}

/// An edit of one mood field.
#[derive(Debug, Clone, PartialEq)]
pub enum MoodUpdate {
    Mood(String),
    Body(String),
    Duration(Option<i64>),
    TodoId(Option<Id>),
}

impl MoodUpdate {
    pub fn cell(&self) -> (&'static str, Option<CellValue>) {
        match self {
            Self::Mood(mood) => (mood_field::MOOD, Some(CellValue::text(mood))),
            Self::Body(body) => (mood_field::BODY, Some(CellValue::text(body))),
            Self::Duration(duration) => (mood_field::DURATION, duration.map(CellValue::Int)),
            Self::TodoId(todo) => (
                mood_field::TODO_ID,
                todo.map(|id| CellValue::text(id.to_string())),
            ),
        }
    }
}

/// An edit of one tracker entry. `type` and `time` are create-only: a row that
/// has to move to another slot is replaced, not rewritten.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackerUpdate {
    Score(CellValue),
    MoodId(Option<Id>),
}

impl TrackerUpdate {
    pub fn cell(&self) -> (&'static str, Option<CellValue>) {
        match self {
            Self::Score(score) => (tracker_field::SCORE, Some(score.clone())),
            Self::MoodId(mood) => (
                tracker_field::MOOD_ID,
                mood.map(|id| CellValue::text(id.to_string())),
            ),
        }
    }
}

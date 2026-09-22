//! Wire types of the sync event stream (`@@SYNC.md` §3).
//!
//! A mutation carries a **field-level diff**: a creation snapshot for a new
//! entity, or only the fields an edit touched. `payload: None` is a delete.
//! Ids serialize as hyphenated uuid strings — [`crate::db::Id`] is
//! `serde(transparent)` over `uuid::Uuid`, so the JSON matches the spec.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{EventId, Id, TrackerValue};

/// The column names of `todos` that a diff can carry.
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

/// The column names of `mood` that a diff can carry.
pub mod mood_field {
    pub const MOOD: &str = "mood";
    pub const BODY: &str = "body";
    pub const TIME: &str = "time";
    pub const SCORE: &str = "score";
    pub const DURATION: &str = "duration";
    pub const TODO_ID: &str = "todo_id";
}

/// The column names of `tracker` that a diff can carry.
pub mod tracker_field {
    pub const TRACKER_TYPE: &str = "type";
    pub const SCORE: &str = "score";
    pub const TIME: &str = "time";
    pub const MOOD_ID: &str = "mood_id";
}

/// The column names of `todo_completions` that a diff can carry.
pub mod completion_field {
    pub const TODO_ID: &str = "todo_id";
    pub const TIME: &str = "time";
    pub const COUNT: &str = "count";
}

/// One field of a mutation: `None` clears the column (`NULL`).
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub field: &'static str,
    pub value: Option<Value>,
}

impl Change {
    pub fn set(field: &'static str, value: impl Serialize) -> Self {
        Self {
            field,
            value: Some(serde_json::to_value(value).expect("a field value serializes")),
        }
    }

    pub fn clear(field: &'static str) -> Self {
        Self { field, value: None }
    }
}

/// One event: a creation snapshot, an update diff, or (with `None`) a delete.
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
    /// `Some` = mutation, `None` = delete.
    pub payload: Option<EntityPayload>,
}

/// One row of the server's log: an [`SyncEvent`] plus its arrival `version`,
/// which orders the pull (`@@SYNC.md` §4.3, §5.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteEvent {
    pub version: i64,
    #[serde(flatten)]
    pub event: SyncEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum EntityPayload {
    TaskCreate(TaskCreateData),
    TaskUpdate(TaskUpdate),
    MoodCreate(MoodCreateData),
    MoodUpdate(MoodUpdate),
    TrackerCreate(TrackerData),
    TrackerUpdate(TrackerUpdate),
    Completion(CompletionData),
}

/// Alias for `EntityPayload` as an event variant.
pub type Event = EntityPayload;

/// Backward-compatible type aliases.
pub type TaskUpdateData = TaskUpdate;
pub type MoodUpdateData = MoodUpdate;
pub type TrackerUpdateData = TrackerUpdate;

/// A new task: every column a peer needs to materialize the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskCreateData {
    pub name: String,
    #[serde(default)]
    pub body: String,
    pub priority: i32,
    #[serde(default)]
    pub start_time: Option<i64>,
    #[serde(default)]
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`] (`None` for a
    /// oneshot or scheduled task).
    #[serde(default)]
    pub interval_secs: Option<i64>,
    #[serde(default)]
    pub target_count: i32,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub end_time: Option<i64>,
    #[serde(default)]
    pub parent_id: Option<Id>,
}

/// An update to a single field of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
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
    pub fn change(&self) -> Change {
        match self {
            TaskUpdate::Name(name) => Change::set(task_field::NAME, name),
            TaskUpdate::Body(body) => Change::set(task_field::BODY, body),
            TaskUpdate::Priority(priority) => Change::set(task_field::PRIORITY, priority),
            TaskUpdate::StartTime(Some(time)) => Change::set(task_field::START_TIME, time),
            TaskUpdate::StartTime(None) => Change::clear(task_field::START_TIME),
            TaskUpdate::AvailableDurationSecs(Some(secs)) => {
                Change::set(task_field::AVAILABLE_DURATION_SECS, secs)
            }
            TaskUpdate::AvailableDurationSecs(None) => {
                Change::clear(task_field::AVAILABLE_DURATION_SECS)
            }
            TaskUpdate::IntervalSecs(Some(secs)) => Change::set(task_field::INTERVAL_SECS, secs),
            TaskUpdate::IntervalSecs(None) => Change::clear(task_field::INTERVAL_SECS),
            TaskUpdate::TargetCount(count) => Change::set(task_field::TARGET_COUNT, count),
            TaskUpdate::Optional(optional) => Change::set(task_field::OPTIONAL, optional),
            TaskUpdate::EndTime(Some(time)) => Change::set(task_field::END_TIME, time),
            TaskUpdate::EndTime(None) => Change::clear(task_field::END_TIME),
            TaskUpdate::ParentId(Some(parent)) => Change::set(task_field::PARENT_ID, parent),
            TaskUpdate::ParentId(None) => Change::clear(task_field::PARENT_ID),
        }
    }
}

/// A new mood entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoodCreateData {
    pub mood: String,
    #[serde(default)]
    pub body: String,
    pub time: i64,
    #[serde(default)]
    pub score: Option<f32>,
    #[serde(default)]
    pub duration: Option<i64>,
    pub todo_id: Option<Id>,
}

/// An update to a single field of a mood entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum MoodUpdate {
    Mood(String),
    Body(String),
    Score(Option<f32>),
    Duration(Option<i64>),
    TodoId(Option<Id>),
}

impl MoodUpdate {
    pub fn change(&self) -> Change {
        match self {
            MoodUpdate::Mood(mood) => Change::set(mood_field::MOOD, mood),
            MoodUpdate::Body(body) => Change::set(mood_field::BODY, body),
            MoodUpdate::Score(Some(score)) => Change::set(mood_field::SCORE, score),
            MoodUpdate::Score(None) => Change::clear(mood_field::SCORE),
            MoodUpdate::Duration(Some(dur)) => Change::set(mood_field::DURATION, dur),
            MoodUpdate::Duration(None) => Change::clear(mood_field::DURATION),
            MoodUpdate::TodoId(Some(todo)) => Change::set(mood_field::TODO_ID, todo),
            MoodUpdate::TodoId(None) => Change::clear(mood_field::TODO_ID),
        }
    }
}

/// A new tracker entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackerData {
    pub tracker_type: String,
    pub score: TrackerScore,
    pub time: i64,
    #[serde(default)]
    pub mood_id: Option<Id>,
}

/// An update to a single field of a tracker entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum TrackerUpdate {
    Score(TrackerScore),
    Time(i64),
    MoodId(Option<Id>),
}

impl TrackerUpdate {
    pub fn change(&self) -> Change {
        match self {
            TrackerUpdate::Score(score) => Change::set(tracker_field::SCORE, score),
            TrackerUpdate::Time(time) => Change::set(tracker_field::TIME, time),
            TrackerUpdate::MoodId(Some(mood)) => Change::set(tracker_field::MOOD_ID, mood),
            TrackerUpdate::MoodId(None) => Change::clear(tracker_field::MOOD_ID),
        }
    }
}

/// Completions are append-only and immutable: editing a count is not
/// supported, a correction deletes the row and logs a new one (§3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletionData {
    pub todo_id: Id,
    pub time: i64,
    #[serde(default = "default_count")]
    pub count: i32,
}

fn default_count() -> i32 {
    1
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

    /// The storage class of the value, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            TrackerScore::Text(_) => "text",
            TrackerScore::Integer(_) => "integer",
            TrackerScore::Float(_) => "float",
        }
    }
}

impl EntityPayload {
    /// The entity kind, as `_sync_entities.kind` spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            EntityPayload::TaskCreate(_) | EntityPayload::TaskUpdate(_) => "task",
            EntityPayload::MoodCreate(_) | EntityPayload::MoodUpdate(_) => "mood",
            EntityPayload::TrackerCreate(_) | EntityPayload::TrackerUpdate(_) => "tracker",
            EntityPayload::Completion(_) => "completion",
        }
    }

    /// The fields this mutation writes. A creation carries every field of the
    /// entity; an update carries only what changed.
    pub fn changes(&self) -> Vec<Change> {
        match self {
            EntityPayload::TaskCreate(data) => vec![
                Change::set(task_field::NAME, &data.name),
                Change::set(task_field::BODY, &data.body),
                Change::set(task_field::PRIORITY, data.priority),
                Change::set(task_field::START_TIME, data.start_time),
                Change::set(
                    task_field::AVAILABLE_DURATION_SECS,
                    data.available_duration_secs,
                ),
                Change::set(task_field::INTERVAL_SECS, data.interval_secs),
                Change::set(task_field::TARGET_COUNT, data.target_count),
                Change::set(task_field::OPTIONAL, data.optional),
                Change::set(task_field::END_TIME, data.end_time),
                Change::set(task_field::PARENT_ID, data.parent_id),
            ],
            EntityPayload::TaskUpdate(data) => vec![data.change()],
            EntityPayload::MoodCreate(data) => vec![
                Change::set(mood_field::MOOD, &data.mood),
                Change::set(mood_field::BODY, &data.body),
                Change::set(mood_field::TIME, data.time),
                Change::set(mood_field::SCORE, data.score),
                Change::set(mood_field::DURATION, data.duration),
                Change::set(mood_field::TODO_ID, data.todo_id),
            ],
            EntityPayload::MoodUpdate(data) => vec![data.change()],
            EntityPayload::TrackerCreate(data) => vec![
                Change::set(tracker_field::TRACKER_TYPE, &data.tracker_type),
                Change::set(tracker_field::SCORE, &data.score),
                Change::set(tracker_field::TIME, data.time),
                Change::set(tracker_field::MOOD_ID, data.mood_id),
            ],
            EntityPayload::TrackerUpdate(data) => vec![data.change()],
            EntityPayload::Completion(data) => vec![
                Change::set(completion_field::TODO_ID, data.todo_id),
                Change::set(completion_field::TIME, data.time),
                Change::set(completion_field::COUNT, data.count),
            ],
        }
    }

    /// Whether a mutation carries anything at all (an empty edit emits nothing).
    pub fn is_empty(&self) -> bool {
        self.changes().is_empty()
    }
}

/// An update payload carrying exactly one field: what a resolution publishes
/// after the user picked a side (`@@SYNC.md` §4.2).
pub fn payload_of_change(kind: &str, change: &Change) -> anyhow::Result<EntityPayload> {
    let value = change.value.clone();
    match (kind, change.field) {
        ("task", task_field::NAME) => Ok(EntityPayload::TaskUpdate(TaskUpdate::Name(
            value
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        ))),
        ("task", task_field::BODY) => Ok(EntityPayload::TaskUpdate(TaskUpdate::Body(
            value
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        ))),
        ("task", task_field::PARENT_ID) => Ok(EntityPayload::TaskUpdate(TaskUpdate::ParentId(
            value
                .as_ref()
                .and_then(|value| value.as_str())
                .and_then(|text| Id::parse(text).ok()),
        ))),
        ("task", task_field::AVAILABLE_DURATION_SECS) => {
            Ok(EntityPayload::TaskUpdate(TaskUpdate::AvailableDurationSecs(
                value.as_ref().and_then(Value::as_i64),
            )))
        }
        ("mood", mood_field::MOOD) => Ok(EntityPayload::MoodUpdate(MoodUpdate::Mood(
            value
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        ))),
        ("mood", mood_field::BODY) => Ok(EntityPayload::MoodUpdate(MoodUpdate::Body(
            value
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        ))),
        ("mood", mood_field::TODO_ID) => Ok(EntityPayload::MoodUpdate(MoodUpdate::TodoId(
            value
                .as_ref()
                .and_then(|value| value.as_str())
                .and_then(|text| Id::parse(text).ok()),
        ))),
        ("tracker", tracker_field::SCORE) => Ok(EntityPayload::TrackerUpdate(TrackerUpdate::Score(
            value
                .map(|value| serde_json::from_value(value).context("Corrupt tracker score"))
                .transpose()?
                .context("Tracker score cannot be null")?,
        ))),
        ("tracker", tracker_field::TIME) => Ok(EntityPayload::TrackerUpdate(TrackerUpdate::Time(
            value.as_ref().and_then(Value::as_i64).unwrap_or_default(),
        ))),
        ("tracker", tracker_field::MOOD_ID) => {
            Ok(EntityPayload::TrackerUpdate(TrackerUpdate::MoodId(
                value
                    .as_ref()
                    .and_then(|value| value.as_str())
                    .and_then(|text| Id::parse(text).ok()),
            )))
        }
        (kind, field) => anyhow::bail!("cannot publish field '{field}' of a {kind}"),
    }
}

/// Whether concurrent replacements of this field need the user's decision
/// (`@@SYNC.md` §4.2.1) instead of a silent field-level LWW.
pub fn is_text_field(kind: &str, field: &str) -> bool {
    matches!(
        (kind, field),
        ("task", task_field::NAME)
            | ("task", task_field::BODY)
            | ("mood", mood_field::MOOD)
            | ("mood", mood_field::BODY)
    )
}

/// Whether a text field holds free-form notes. A replacement conflict on a
/// note can also be settled by keeping both texts; a name or a mood label can
/// only be one value or the other (`@@SYNC.md` §4.2.1). The task and the mood
/// body are the note columns, and both are called `body`.
pub fn is_note_field(field: &str) -> bool {
    field == task_field::BODY
}

impl SyncEvent {
    pub fn is_delete(&self) -> bool {
        self.payload.is_none()
    }
}

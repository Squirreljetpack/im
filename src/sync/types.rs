//! Wire types of the sync event stream (`@@SYNC.md` §3).
//!
//! A mutation carries a **field-level diff**: a creation snapshot for a new
//! entity, or only the fields an edit touched. `payload: None` is a delete.
//! Ids serialize as hyphenated uuid strings — [`crate::db::Id`] is
//! `serde(transparent)` over `uuid::Uuid`, so the JSON matches the spec.

use anyhow::Context;
use serde::{Deserialize, Deserializer, Serialize};
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
    TaskUpdate(TaskUpdateData),
    MoodCreate(MoodCreateData),
    MoodUpdate(MoodUpdateData),
    TrackerCreate(TrackerData),
    TrackerUpdate(TrackerUpdateData),
    Completion(CompletionData),
}

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

/// An edited task: only the fields the edit touched.
///
/// `None` means *unchanged*; a nullable field wrapped twice
/// ([`double_option`]) also distinguishes a **clear** (`Some(None)` → `NULL`)
/// from an absent field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskUpdateData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<i64>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub available_duration_secs: Option<Option<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_count: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<i64>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_id: Option<Option<Id>>,
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
    #[serde(default)]
    pub todo_id: Option<Id>,
}

/// An edited mood entry: only the fields the edit touched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MoodUpdateData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub todo_id: Option<Option<Id>>,
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

/// An edited tracker entry: only the fields the edit touched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TrackerUpdateData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<TrackerScore>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub mood_id: Option<Option<Id>>,
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

/// Deserialize a nullable field so an explicit `null` (a clear) differs from
/// an absent one (unchanged).
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
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
            EntityPayload::TaskUpdate(data) => {
                let mut changes = Vec::new();
                if let Some(name) = &data.name {
                    changes.push(Change::set(task_field::NAME, name));
                }
                if let Some(body) = &data.body {
                    changes.push(Change::set(task_field::BODY, body));
                }
                if let Some(priority) = data.priority {
                    changes.push(Change::set(task_field::PRIORITY, priority));
                }
                if let Some(start_time) = data.start_time {
                    changes.push(Change::set(task_field::START_TIME, start_time));
                }
                match data.available_duration_secs {
                    Some(Some(secs)) => {
                        changes.push(Change::set(task_field::AVAILABLE_DURATION_SECS, secs))
                    }
                    Some(None) => changes.push(Change::clear(task_field::AVAILABLE_DURATION_SECS)),
                    None => {}
                }
                if let Some(interval_secs) = data.interval_secs {
                    changes.push(Change::set(task_field::INTERVAL_SECS, interval_secs));
                }
                if let Some(target_count) = data.target_count {
                    changes.push(Change::set(task_field::TARGET_COUNT, target_count));
                }
                if let Some(optional) = data.optional {
                    changes.push(Change::set(task_field::OPTIONAL, optional));
                }
                if let Some(end_time) = data.end_time {
                    changes.push(Change::set(task_field::END_TIME, end_time));
                }
                match data.parent_id {
                    Some(Some(parent)) => changes.push(Change::set(task_field::PARENT_ID, parent)),
                    Some(None) => changes.push(Change::clear(task_field::PARENT_ID)),
                    None => {}
                }
                changes
            }
            EntityPayload::MoodCreate(data) => vec![
                Change::set(mood_field::MOOD, &data.mood),
                Change::set(mood_field::BODY, &data.body),
                Change::set(mood_field::TIME, data.time),
                Change::set(mood_field::SCORE, data.score),
                Change::set(mood_field::DURATION, data.duration),
                Change::set(mood_field::TODO_ID, data.todo_id),
            ],
            EntityPayload::MoodUpdate(data) => {
                let mut changes = Vec::new();
                if let Some(mood) = &data.mood {
                    changes.push(Change::set(mood_field::MOOD, mood));
                }
                if let Some(body) = &data.body {
                    changes.push(Change::set(mood_field::BODY, body));
                }
                if let Some(score) = data.score {
                    changes.push(Change::set(mood_field::SCORE, score));
                }
                if let Some(duration) = data.duration {
                    changes.push(Change::set(mood_field::DURATION, duration));
                }
                match data.todo_id {
                    Some(Some(todo)) => changes.push(Change::set(mood_field::TODO_ID, todo)),
                    Some(None) => changes.push(Change::clear(mood_field::TODO_ID)),
                    None => {}
                }
                changes
            }
            EntityPayload::TrackerCreate(data) => vec![
                Change::set(tracker_field::TRACKER_TYPE, &data.tracker_type),
                Change::set(tracker_field::SCORE, &data.score),
                Change::set(tracker_field::TIME, data.time),
                Change::set(tracker_field::MOOD_ID, data.mood_id),
            ],
            EntityPayload::TrackerUpdate(data) => {
                let mut changes = Vec::new();
                if let Some(score) = &data.score {
                    changes.push(Change::set(tracker_field::SCORE, score));
                }
                if let Some(time) = data.time {
                    changes.push(Change::set(tracker_field::TIME, time));
                }
                match data.mood_id {
                    Some(Some(mood)) => changes.push(Change::set(tracker_field::MOOD_ID, mood)),
                    Some(None) => changes.push(Change::clear(tracker_field::MOOD_ID)),
                    None => {}
                }
                changes
            }
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
        ("task", task_field::NAME) => Ok(EntityPayload::TaskUpdate(TaskUpdateData {
            name: Some(
                value
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
            ..TaskUpdateData::default()
        })),
        ("task", task_field::BODY) => Ok(EntityPayload::TaskUpdate(TaskUpdateData {
            body: Some(
                value
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
            ..TaskUpdateData::default()
        })),
        ("task", task_field::PARENT_ID) => Ok(EntityPayload::TaskUpdate(TaskUpdateData {
            parent_id: Some(
                value
                    .as_ref()
                    .and_then(|value| value.as_str())
                    .and_then(|text| Id::parse(text).ok()),
            ),
            ..TaskUpdateData::default()
        })),
        ("task", task_field::AVAILABLE_DURATION_SECS) => {
            Ok(EntityPayload::TaskUpdate(TaskUpdateData {
                available_duration_secs: Some(value.as_ref().and_then(Value::as_i64)),
                ..TaskUpdateData::default()
            }))
        }
        ("mood", mood_field::MOOD) => Ok(EntityPayload::MoodUpdate(MoodUpdateData {
            mood: Some(
                value
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
            ..MoodUpdateData::default()
        })),
        ("mood", mood_field::BODY) => Ok(EntityPayload::MoodUpdate(MoodUpdateData {
            body: Some(
                value
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
            ..MoodUpdateData::default()
        })),
        ("mood", mood_field::TODO_ID) => Ok(EntityPayload::MoodUpdate(MoodUpdateData {
            todo_id: Some(
                value
                    .as_ref()
                    .and_then(|value| value.as_str())
                    .and_then(|text| Id::parse(text).ok()),
            ),
            ..MoodUpdateData::default()
        })),
        ("tracker", tracker_field::SCORE) => Ok(EntityPayload::TrackerUpdate(TrackerUpdateData {
            score: value
                .map(|value| serde_json::from_value(value).context("Corrupt tracker score"))
                .transpose()?,
            ..TrackerUpdateData::default()
        })),
        ("tracker", tracker_field::TIME) => Ok(EntityPayload::TrackerUpdate(TrackerUpdateData {
            time: value.as_ref().and_then(Value::as_i64),
            ..TrackerUpdateData::default()
        })),
        ("tracker", tracker_field::MOOD_ID) => {
            Ok(EntityPayload::TrackerUpdate(TrackerUpdateData {
                mood_id: Some(
                    value
                        .as_ref()
                        .and_then(|value| value.as_str())
                        .and_then(|text| Id::parse(text).ok()),
                ),
                ..TrackerUpdateData::default()
            }))
        }
        (kind, field) => anyhow::bail!("cannot publish field '{field}' of a {kind}"),
    }
}

/// Every field a kind carries: the set a delete wins (and a resurrection
/// starts from).
pub fn fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "task" => &[
            task_field::NAME,
            task_field::BODY,
            task_field::PRIORITY,
            task_field::START_TIME,
            task_field::AVAILABLE_DURATION_SECS,
            task_field::INTERVAL_SECS,
            task_field::TARGET_COUNT,
            task_field::OPTIONAL,
            task_field::END_TIME,
            task_field::PARENT_ID,
        ],
        "mood" => &[
            mood_field::MOOD,
            mood_field::BODY,
            mood_field::TIME,
            mood_field::SCORE,
            mood_field::DURATION,
            mood_field::TODO_ID,
        ],
        "tracker" => &[
            tracker_field::TRACKER_TYPE,
            tracker_field::SCORE,
            tracker_field::TIME,
            tracker_field::MOOD_ID,
        ],
        "completion" => &[
            completion_field::TODO_ID,
            completion_field::TIME,
            completion_field::COUNT,
        ],
        _ => &[],
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

impl SyncEvent {
    pub fn is_delete(&self) -> bool {
        self.payload.is_none()
    }
}

impl TaskUpdateData {
    /// Whether the edit changed anything at all.
    pub fn is_empty(&self) -> bool {
        *self == TaskUpdateData::default()
    }
}

impl MoodUpdateData {
    /// Whether the edit changed anything at all.
    pub fn is_empty(&self) -> bool {
        *self == MoodUpdateData::default()
    }
}

impl TrackerUpdateData {
    /// Whether the edit changed anything at all.
    pub fn is_empty(&self) -> bool {
        *self == TrackerUpdateData::default()
    }
}

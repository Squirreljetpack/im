//! The schema table (`sync_schema.json`): kinds, their fields, storage
//! classes, links, cascades and kind-derivation rules.
//!
//! It is a data file, not generated code, so the worker and this client read
//! the same source. Drift is caught by [`tests::matches_the_field_inventory`].

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

const SCHEMA_JSON: &str = include_str!("../../sync_schema.json");

#[derive(Debug, Deserialize)]
pub struct Schema {
    pub kinds: BTreeMap<String, Kind>,
}

#[derive(Debug, Deserialize)]
pub struct Kind {
    /// The domain table the kind lives in.
    pub table: String,
    pub fields: BTreeMap<String, Field>,
    /// Cells a delete of this kind forces on its neighbours.
    #[serde(default)]
    pub cascade: Vec<Cascade>,
    /// How a subkind (a task's kind) is derived from which fields are set.
    #[serde(default)]
    pub subkind: Option<Subkind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Text,
    Int,
    Real,
    /// Any of the three, decided by the kind's own configuration (tracker
    /// scores: text for a text tracker, integer for integer and null, real for
    /// float and duration).
    Scalar,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub class: Class,
    /// Must be present when the entity is created.
    #[serde(default)]
    pub required: bool,
    /// Written once, at create.
    #[serde(default)]
    pub immutable: bool,
    /// The kind the value must point at.
    #[serde(default)]
    pub link: Option<String>,
    /// The domain column, when it differs from the field name.
    #[serde(default)]
    pub column: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cascade {
    pub kind: String,
    #[serde(default)]
    pub field: Option<String>,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    SetNull,
    Delete,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subkind {
    pub default: String,
    pub from: Vec<(String, String)>,
}

pub static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    serde_json::from_str(SCHEMA_JSON).expect("sync_schema.json is a valid schema")
});

/// The kinds in the order a page must materialise them: a referrer's row can
/// only be written once the row it points at exists.
pub const KIND_ORDER: [&str; 4] = ["task", "mood", "tracker", "completion"];

pub fn kind(name: &str) -> Option<&'static Kind> {
    SCHEMA.kinds.get(name)
}

pub fn field(kind_name: &str, field_name: &str) -> Option<&'static Field> {
    kind(kind_name)?.fields.get(field_name)
}

/// The domain column a field writes.
pub fn column<'a>(kind_name: &'a str, field_name: &'a str) -> Option<&'a str> {
    let spec = field(kind_name, field_name)?;
    Some(spec.column.as_deref().unwrap_or(field_name))
}

pub fn table(kind_name: &str) -> Option<&'static str> {
    Some(kind(kind_name)?.table.as_str())
}

pub fn class(kind_name: &str, field_name: &str) -> Option<Class> {
    Some(field(kind_name, field_name)?.class)
}

/// Whether a field is a link, and to which kind.
pub fn link(kind_name: &str, field_name: &str) -> Option<&'static str> {
    field(kind_name, field_name)?.link.as_deref()
}

/// The subkind a set of populated fields implies, e.g. a task's kind.
pub fn subkind_of(kind_name: &str, present: impl Fn(&str) -> bool) -> Option<String> {
    let rule = kind(kind_name)?.subkind.as_ref()?;
    for (field, subkind) in &rule.from {
        if present(field) {
            return Some(subkind.clone());
        }
    }
    Some(rule.default.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::types::{completion_field, mood_field, task_field, tracker_field};

    /// Every field the client can emit is in the schema, and every schema field
    /// is one the client emits. `mood.score` is deliberately absent: it is a
    /// cache of a function of `mood.body`, derived on read.
    #[test]
    fn matches_the_field_inventory() {
        let task = [
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
        ];
        let mood = [
            mood_field::MOOD,
            mood_field::BODY,
            mood_field::TIME,
            mood_field::DURATION,
            mood_field::TODO_ID,
        ];
        let tracker = [
            tracker_field::TRACKER_TYPE,
            tracker_field::SCORE,
            tracker_field::TIME,
            tracker_field::MOOD_ID,
        ];
        let completion = [
            completion_field::TODO_ID,
            completion_field::TIME,
            completion_field::COUNT,
        ];

        for (kind_name, expected) in [
            ("task", task.as_slice()),
            ("mood", mood.as_slice()),
            ("tracker", tracker.as_slice()),
            ("completion", completion.as_slice()),
        ] {
            let spec = kind(kind_name).expect("the kind is in the schema");
            let mut actual: Vec<&str> = spec.fields.keys().map(String::as_str).collect();
            let mut expected = expected.to_vec();
            actual.sort_unstable();
            expected.sort_unstable();
            assert_eq!(actual, expected, "{kind_name} fields drifted");
        }
        assert_eq!(SCHEMA.kinds.len(), KIND_ORDER.len());
    }

    #[test]
    fn keeps_the_type_traps_mapped() {
        // The wire names and the domain columns differ for both link fields.
        assert_eq!(column("task", task_field::PARENT_ID), Some("parent"));
        assert_eq!(column("tracker", tracker_field::MOOD_ID), Some("mood"));
        assert_eq!(class("tracker", tracker_field::SCORE), Some(Class::Scalar));
        assert_eq!(link("completion", completion_field::TODO_ID), Some("task"));
        assert_eq!(
            subkind_of("task", |field| field == task_field::INTERVAL_SECS).as_deref(),
            Some("recurring")
        );
    }
}

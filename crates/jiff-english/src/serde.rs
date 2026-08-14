//! Serde support for `jiff-english` types.
//!
//! [`jiff::civil::Weekday`] has no serde impl, so this module carries a
//! serde-friendly [`Weekday`] wrapper (re-exported from the ungated
//! [`crate::types`]): it serializes as `"Monday"` … `"Sunday"`
//! (PascalCase) and deserializes case-insensitively from short or full
//! names. Convert to the jiff type with `jiff::civil::Weekday::from`.

use serde::{Deserialize, Serialize};

pub use crate::types::Weekday;

impl Serialize for Weekday {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        })
    }
}

impl<'de> Deserialize<'de> for Weekday {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Weekday::from_name(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown weekday '{}' (expected Mon..Sun or Monday..Sunday)",
                s
            ))
        })
    }
}

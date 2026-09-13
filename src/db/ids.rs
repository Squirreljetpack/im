//! Primary-key types for synced entities.
//!
//! Every synced row is keyed by a UUIDv7 stored as SQLite `TEXT` (hyphenated
//! form): ids are unique across devices and sort in creation order. The
//! `serde(transparent)` wire form is identical to `uuid::Uuid` — a hyphenated
//! string — so sync payloads carry plain uuid strings.

use std::fmt;
use std::str::FromStr;

use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::sqlite::{SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Encode, Sqlite, Type};
use uuid::Uuid;

/// Defines a UUIDv7 `TEXT`-stored id type: `Display`/`FromStr` in hyphenated
/// form, sqlx `Encode`/`Decode` for `TEXT` columns, and `serde` transparent
/// with `uuid::Uuid`.
macro_rules! text_uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh UUIDv7 (its high bits are a millisecond timestamp).
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            pub fn into_uuid(self) -> Uuid {
                self.0
            }

            /// Parse the hyphenated form.
            pub fn parse(text: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(text).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(text).map(Self)
            }
        }

        impl Type<Sqlite> for $name {
            fn type_info() -> SqliteTypeInfo {
                <String as Type<Sqlite>>::type_info()
            }

            fn compatible(ty: &SqliteTypeInfo) -> bool {
                <String as Type<Sqlite>>::compatible(ty)
            }
        }

        impl Encode<'_, Sqlite> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut SqliteArgumentsBuffer,
            ) -> Result<IsNull, BoxDynError> {
                Encode::<Sqlite>::encode(self.0.hyphenated().to_string(), buf)
            }
        }

        impl<'r> Decode<'r, Sqlite> for $name {
            fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
                let text = <String as Decode<'r, Sqlite>>::decode(value)?;
                Self::parse(&text).map_err(Into::into)
            }
        }
    };
}

text_uuid_id! {
    /// The stable row id of a synced entity (`mood`, `tracker`, `todos`,
    /// `todo_completions`).
    Id
}

text_uuid_id! {
    /// The id of one event in the sync stream; `event_id` in the local
    /// outbox and in the server's append-only log, where it is unique and
    /// makes retried pushes idempotent.
    EventId
}

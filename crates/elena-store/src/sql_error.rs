//! Map `sqlx` errors to Elena's serializable [`StoreError`].
//!
//! The goal is to preserve enough structure that callers can distinguish
//! "row not found" from "integrity violation" from "connection broke," while
//! stripping non-serializable `sqlx` internals at the crate boundary.

use elena_types::StoreError;

/// Classify a [`sqlx::Error`] into a [`StoreError`] variant.
pub(crate) fn classify_sqlx(err: sqlx::Error) -> StoreError {
    match err {
        sqlx::Error::RowNotFound => {
            // Generic — the specific NotFound variant is selected by the
            // caller (ThreadStore etc.) which knows which row was expected.
            StoreError::Database("row not found".to_owned())
        }
        sqlx::Error::Database(db_err) => {
            // Unique violations, FK failures, etc. come through here.
            // We surface these as `Conflict` so callers can retry without
            // reclassifying the underlying driver message.
            if matches!(
                db_err.kind(),
                sqlx::error::ErrorKind::UniqueViolation
                    | sqlx::error::ErrorKind::ForeignKeyViolation
                    | sqlx::error::ErrorKind::CheckViolation
                    | sqlx::error::ErrorKind::NotNullViolation
            ) {
                StoreError::Conflict(db_err.to_string())
            } else {
                StoreError::Database(db_err.to_string())
            }
        }
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => {
            StoreError::Database(format!("pool error: {err}"))
        }
        other => StoreError::Database(other.to_string()),
    }
}

/// Map a `serde_json::Error` that occurs at the store boundary.
pub(crate) fn classify_serde(err: &serde_json::Error) -> StoreError {
    StoreError::Serialization(err.to_string())
}

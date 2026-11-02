use crate::queue::EffectId;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    /// The file was written by a newer build than this one.
    Schema {
        found: u32,
        known: u32,
    },
    /// The file system refused write-ahead logging.
    Journal(String),
    NoEffect(EffectId),
    /// The event log fails verification at this row.
    Chain {
        seq: u64,
        reason: &'static str,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "sqlite: {err}"),
            Self::Json(err) => write!(f, "json: {err}"),
            Self::Schema { found, known } => write!(
                f,
                "schema version {found} is newer than this build knows ({known})"
            ),
            Self::Journal(mode) => write!(f, "journal mode is {mode}, not wal"),
            Self::NoEffect(id) => write!(f, "no effect {id}"),
            Self::Chain { seq, reason } => write!(f, "event {seq} {reason}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(err) => Some(err),
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<StoreError> for solos_agent_core::Error {
    fn from(err: StoreError) -> Self {
        Self::Store(err.to_string())
    }
}

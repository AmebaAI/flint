use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, TransactionBehavior, params};
use thiserror::Error;

use crate::session::SessionKey;

const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredSessionState {
    Ready,
    Stopped,
}

impl StoredSessionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Stopped => "stopped",
        }
    }

    fn parse(value: &str) -> Result<Self, SessionStoreError> {
        match value {
            "ready" => Ok(Self::Ready),
            "stopped" => Ok(Self::Stopped),
            _ => Err(SessionStoreError::InvalidState(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionRecord {
    pub(crate) key: SessionKey,
    pub(crate) catalog_generation: String,
    pub(crate) image: String,
    pub(crate) image_id: String,
    pub(crate) volume_name: String,
    pub(crate) created_at_unix_millis: i64,
    pub(crate) last_activity_at_unix_millis: i64,
    pub(crate) compute_started_at_unix_millis: Option<i64>,
    pub(crate) state: StoredSessionState,
    pub(crate) last_error: Option<String>,
    pub(crate) last_stop_token: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SessionStore {
    path: Arc<PathBuf>,
    connection: Arc<Mutex<Connection>>,
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self, SessionStoreError> {
        let mut connection = Connection::open_in_memory()?;
        configure_connection(&mut connection)?;
        Ok(Self {
            path: Arc::new(PathBuf::from(":memory:")),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let path = path.as_ref();
        let parent = path.parent().ok_or_else(|| {
            SessionStoreError::InvalidPath(format!("{} has no parent directory", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut connection = Connection::open(path)?;
        configure_connection(&mut connection)?;
        Ok(Self {
            path: Arc::new(path.to_path_buf()),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub(crate) fn upsert(&self, record: &SessionRecord) -> Result<(), SessionStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            r#"
            INSERT INTO sessions (
                runtime_arn,
                qualifier,
                runtime_session_id,
                catalog_generation,
                image,
                image_id,
                volume_name,
                created_at_unix_millis,
                last_activity_at_unix_millis,
                compute_started_at_unix_millis,
                state,
                last_error,
                last_stop_token
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(runtime_arn, qualifier, runtime_session_id) DO UPDATE SET
                catalog_generation = excluded.catalog_generation,
                image = excluded.image,
                image_id = excluded.image_id,
                volume_name = excluded.volume_name,
                last_activity_at_unix_millis = excluded.last_activity_at_unix_millis,
                compute_started_at_unix_millis = excluded.compute_started_at_unix_millis,
                state = excluded.state,
                last_error = excluded.last_error,
                last_stop_token = excluded.last_stop_token
            "#,
            params![
                record.key.runtime_arn,
                record.key.qualifier,
                record.key.runtime_session_id,
                record.catalog_generation,
                record.image,
                record.image_id,
                record.volume_name,
                record.created_at_unix_millis,
                record.last_activity_at_unix_millis,
                record.compute_started_at_unix_millis,
                record.state.as_str(),
                record.last_error,
                record.last_stop_token,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_ready_writes_for_test(&self) -> Result<(), SessionStoreError> {
        self.connection()?.execute_batch(
            "CREATE TEMP TRIGGER fail_ready_write
             BEFORE UPDATE OF state ON sessions
             WHEN NEW.state = 'ready'
             BEGIN
                 SELECT RAISE(FAIL, 'fixture mark_ready failure');
             END;",
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, key: &SessionKey) -> Result<Option<SessionRecord>, SessionStoreError> {
        let connection = self.connection()?;
        let stored = connection
            .query_row(
                "SELECT runtime_arn, qualifier, runtime_session_id, catalog_generation, image, image_id, volume_name, created_at_unix_millis, last_activity_at_unix_millis, compute_started_at_unix_millis, state, last_error, last_stop_token FROM sessions WHERE runtime_arn = ?1 AND qualifier = ?2 AND runtime_session_id = ?3",
                params![key.runtime_arn, key.qualifier, key.runtime_session_id],
                raw_record,
            )
            .optional()?;
        stored.map(parse_record).transpose()
    }

    pub(crate) fn load_all(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT runtime_arn, qualifier, runtime_session_id, catalog_generation, image, image_id, volume_name, created_at_unix_millis, last_activity_at_unix_millis, compute_started_at_unix_millis, state, last_error, last_stop_token FROM sessions ORDER BY runtime_arn, qualifier, runtime_session_id",
        )?;
        let rows = statement.query_map([], raw_record)?;
        rows.map(|row| row.map_err(SessionStoreError::from).and_then(parse_record))
            .collect()
    }

    pub(crate) fn mark_ready(
        &self,
        key: &SessionKey,
        compute_started_at_unix_millis: i64,
        last_activity_at_unix_millis: i64,
    ) -> Result<(), SessionStoreError> {
        self.update_state(
            key,
            StoredSessionState::Ready,
            Some(compute_started_at_unix_millis),
            last_activity_at_unix_millis,
            None,
            None,
        )
    }

    pub(crate) fn mark_stopped(
        &self,
        key: &SessionKey,
        last_activity_at_unix_millis: i64,
        last_error: Option<&str>,
        last_stop_token: Option<&str>,
    ) -> Result<(), SessionStoreError> {
        self.update_state(
            key,
            StoredSessionState::Stopped,
            None,
            last_activity_at_unix_millis,
            last_error,
            last_stop_token,
        )
    }

    pub(crate) fn touch(
        &self,
        key: &SessionKey,
        last_activity_at_unix_millis: i64,
    ) -> Result<(), SessionStoreError> {
        let connection = self.connection()?;
        let changed = connection.execute(
            "UPDATE sessions SET last_activity_at_unix_millis = ?4 WHERE runtime_arn = ?1 AND qualifier = ?2 AND runtime_session_id = ?3",
            params![
                key.runtime_arn,
                key.qualifier,
                key.runtime_session_id,
                last_activity_at_unix_millis
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(SessionStoreError::MissingSession(
                key.runtime_session_id.clone(),
            ))
        }
    }

    fn update_state(
        &self,
        key: &SessionKey,
        state: StoredSessionState,
        compute_started_at_unix_millis: Option<i64>,
        last_activity_at_unix_millis: i64,
        last_error: Option<&str>,
        last_stop_token: Option<&str>,
    ) -> Result<(), SessionStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE sessions SET state = ?4, compute_started_at_unix_millis = ?5, last_activity_at_unix_millis = ?6, last_error = ?7, last_stop_token = ?8 WHERE runtime_arn = ?1 AND qualifier = ?2 AND runtime_session_id = ?3",
            params![
                key.runtime_arn,
                key.qualifier,
                key.runtime_session_id,
                state.as_str(),
                compute_started_at_unix_millis,
                last_activity_at_unix_millis,
                last_error,
                last_stop_token,
            ],
        )?;
        if changed != 1 {
            return Err(SessionStoreError::MissingSession(
                key.runtime_session_id.clone(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, SessionStoreError> {
        self.connection
            .lock()
            .map_err(|_| SessionStoreError::LockPoisoned)
    }
}

fn configure_connection(connection: &mut Connection) -> Result<(), SessionStoreError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    migrate(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), SessionStoreError> {
    let version =
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
    match version {
        0 => {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                r#"
                CREATE TABLE sessions (
                    runtime_arn TEXT NOT NULL,
                    qualifier TEXT NOT NULL,
                    runtime_session_id TEXT NOT NULL,
                    catalog_generation TEXT NOT NULL,
                    image TEXT NOT NULL,
                    image_id TEXT NOT NULL,
                    volume_name TEXT NOT NULL,
                    created_at_unix_millis INTEGER NOT NULL,
                    last_activity_at_unix_millis INTEGER NOT NULL,
                    compute_started_at_unix_millis INTEGER,
                    state TEXT NOT NULL CHECK (state IN ('ready', 'stopped')),
                    last_error TEXT,
                    last_stop_token TEXT,
                    PRIMARY KEY (runtime_arn, qualifier, runtime_session_id)
                );
                CREATE INDEX sessions_state_idx ON sessions(state);
                "#,
            )?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        other => Err(SessionStoreError::UnsupportedSchema(other)),
    }
}

type RawRecord = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
);

fn raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn parse_record(raw: RawRecord) -> Result<SessionRecord, SessionStoreError> {
    Ok(SessionRecord {
        key: SessionKey {
            runtime_arn: raw.0,
            qualifier: raw.1,
            runtime_session_id: raw.2,
        },
        catalog_generation: raw.3,
        image: raw.4,
        image_id: raw.5,
        volume_name: raw.6,
        created_at_unix_millis: raw.7,
        last_activity_at_unix_millis: raw.8,
        compute_started_at_unix_millis: raw.9,
        state: StoredSessionState::parse(&raw.10)?,
        last_error: raw.11,
        last_stop_token: raw.12,
    })
}

#[derive(Debug, Error)]
pub(crate) enum SessionStoreError {
    #[error("failed to create session state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid session state path: {0}")]
    InvalidPath(String),
    #[error("session state lock was poisoned")]
    LockPoisoned,
    #[error("session {0} is not stored")]
    MissingSession(String),
    #[error("session state contains unsupported state {0}")]
    InvalidState(String),
    #[error("session state schema version {0} is newer than this Flint build supports")]
    UnsupportedSchema(i64),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

#[cfg(test)]
mod tests {
    use super::{SessionRecord, SessionStore, SessionStoreError, StoredSessionState};
    use crate::{catalog::RuntimeCatalog, session::SessionKey};

    fn temporary_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "flint-session-store-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn remove_database(path: &std::path::Path) {
        for suffix in ["", "-shm", "-wal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn record() -> SessionRecord {
        let runtime = RuntimeCatalog::test_catalog().default_snapshot();
        SessionRecord {
            key: SessionKey {
                runtime_arn: runtime.runtime_arn.clone(),
                qualifier: runtime.qualifier.clone(),
                runtime_session_id: "20000000-0000-0000-0000-000000000001".to_owned(),
            },
            catalog_generation: runtime.catalog_generation.clone(),
            image: runtime.image.clone(),
            image_id: runtime.image_id.clone(),
            volume_name: "flint-session-volume".to_owned(),
            created_at_unix_millis: 1_000,
            last_activity_at_unix_millis: 1_000,
            compute_started_at_unix_millis: None,
            state: StoredSessionState::Stopped,
            last_error: None,
            last_stop_token: None,
        }
    }

    #[test]
    fn schema_contains_metadata_but_no_runtime_secrets() {
        let store = SessionStore::in_memory().expect("session store");
        let connection = store.connection().expect("store connection");
        let mut statement = connection
            .prepare("PRAGMA table_info(sessions)")
            .expect("inspect schema");
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query schema")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema columns");

        assert!(columns.contains(&"runtime_session_id".to_owned()));
        assert!(columns.contains(&"image_id".to_owned()));
        assert!(columns.contains(&"volume_name".to_owned()));
        assert!(!columns.iter().any(|column| {
            column.contains("environment")
                || column.contains("credential")
                || column.contains("secret")
                || column.contains("payload")
        }));
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let path = temporary_path();
        let connection = rusqlite::Connection::open(&path).expect("create future database");
        connection
            .pragma_update(None, "user_version", 99)
            .expect("set future schema version");
        drop(connection);

        assert!(matches!(
            SessionStore::open(&path),
            Err(SessionStoreError::UnsupportedSchema(99))
        ));
        remove_database(&path);
    }

    #[test]
    fn session_records_survive_reopen_and_state_transitions() {
        let path = temporary_path();
        let expected = record();
        {
            let store = SessionStore::open(&path).expect("open session store");
            assert_eq!(store.path(), path);
            store.upsert(&expected).expect("store session");
            store
                .mark_ready(&expected.key, 2_000, 2_100)
                .expect("mark ready");
            store.touch(&expected.key, 2_200).expect("touch session");
        }
        {
            let store = SessionStore::open(&path).expect("reopen session store");
            let ready = store
                .get(&expected.key)
                .expect("read session")
                .expect("stored session");
            assert_eq!(ready.state, StoredSessionState::Ready);
            assert_eq!(ready.compute_started_at_unix_millis, Some(2_000));
            assert_eq!(ready.last_activity_at_unix_millis, 2_200);
            store
                .mark_stopped(&expected.key, 2_300, Some("unhealthy"), Some("stop-token"))
                .expect("mark stopped");
            let stopped = store.load_all().expect("load sessions");
            assert_eq!(stopped.len(), 1);
            assert_eq!(stopped[0].state, StoredSessionState::Stopped);
            assert_eq!(stopped[0].last_error.as_deref(), Some("unhealthy"));
            assert_eq!(stopped[0].last_stop_token.as_deref(), Some("stop-token"));
        }
        remove_database(&path);
    }
}

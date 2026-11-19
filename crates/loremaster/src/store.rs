//! SQLite store for memories, queued turns and session state.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use loremaster_core::{Hlc, Kind, Record, RecordType, Scope};
use rusqlite::{
    CachedStatement, Connection, ErrorCode, OptionalExtension, Row, Transaction,
    TransactionBehavior, params,
};
use serde::de::DeserializeOwned;
use thiserror::Error;

const SCHEMA_VERSION: i64 = 3;
const SCHEMA_V1: &str = "
    CREATE TABLE memories (
        memory_id TEXT PRIMARY KEY,
        type TEXT,
        kind TEXT,
        scope TEXT,
        project_id TEXT,
        text TEXT,
        created_at INTEGER,
        hlc TEXT NOT NULL,
        writer TEXT NOT NULL,
        superseded_by TEXT,
        deleted INTEGER NOT NULL DEFAULT 0,
        seq INTEGER,
        vector BLOB,
        model_id TEXT
    );
    CREATE INDEX memories_scope_project ON memories (scope, project_id);
    CREATE TABLE turn_queue (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL,
        agent TEXT NOT NULL,
        project_id TEXT NOT NULL,
        turn TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
    );
    CREATE TABLE sessions (
        session_id TEXT PRIMARY KEY,
        agent TEXT NOT NULL,
        project_id TEXT NOT NULL,
        transcript_path TEXT,
        transcript_offset INTEGER NOT NULL DEFAULT 0,
        last_turn_at INTEGER,
        ended INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE outbox (memory_id TEXT PRIMARY KEY);
    CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);
    CREATE TABLE injected (
        session_id TEXT NOT NULL,
        memory_id TEXT NOT NULL,
        PRIMARY KEY (session_id, memory_id)
    );
";
/// Counts the failed tries of each queued turn, so one that always fails stops being tried.
const SCHEMA_V2: &str = "ALTER TABLE turn_queue ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;";
/// Counts the failed episode tries of each session, so one that always fails stops being tried.
const SCHEMA_V3: &str =
    "ALTER TABLE sessions ADD COLUMN episode_attempts INTEGER NOT NULL DEFAULT 0;";
const MEMORY_COLUMNS: &str = "memory_id, type, kind, scope, project_id, text, created_at, hlc, \
    writer, superseded_by, deleted, seq, vector, model_id";
const MEMORY_VALUES: &str = "?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14";
const CLOCK_KEY: &str = "hlc";
const CURSOR_KEY: &str = "cursor";
const TOTAL_KEY: &str = "total";

/// Why the store could not be opened or used.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store.db is from a newer Loremaster (schema {found})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("store.db is not a database")]
    NotADatabase,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot lock store.db for setup: {0}")]
    SetupLock(#[from] std::io::Error),
}

/// A memory as kept on the device, with its locally computed vector.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMemory {
    pub record: Record,
    pub vector: Option<Vec<f32>>,
    pub model_id: Option<String>,
}

/// How much of the user's memory a device holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Live memories with a vector, so recall can find them.
    pub recallable: i64,
    /// The service's count of the user's live memories at the last pull. `None` before one.
    pub total: Option<i64>,
    /// Memories waiting to be pushed.
    pub outbox: i64,
}

/// Capture progress for one agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub agent: String,
    pub project_id: String,
    pub transcript_path: Option<String>,
    pub transcript_offset: u64,
    pub last_turn_at: Option<i64>,
    pub ended: bool,
}

/// One queued prepared turn and the session state that advances with it.
#[derive(Debug, Clone, Copy)]
pub struct QueuedTurn<'a> {
    pub session_id: &'a str,
    pub agent: &'a str,
    pub project_id: &'a str,
    pub turn: &'a str,
    pub next_offset: u64,
    pub now_ms: i64,
}

/// A queued turn that still needs extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTurn {
    pub id: i64,
    pub session_id: String,
    pub project_id: String,
    pub turn: String,
}

/// A session whose queued turns are ready to become an episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueEpisode {
    pub session_id: String,
    pub project_id: String,
    /// Queue ids and prepared turns, oldest first.
    pub turns: Vec<(i64, String)>,
}

impl DueEpisode {
    pub fn turn_ids(&self) -> Vec<i64> {
        self.turns.iter().map(|(id, _)| *id).collect()
    }
}

/// Where a queued turn is in extraction. Rows stay queued in every status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Pending,
    Extracted,
    /// It cannot be extracted and is not tried again.
    Failed,
}

impl TurnStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Extracted => "extracted",
            Self::Failed => "failed",
        }
    }
}

/// The device store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the store, migrating it to the current schema. A store it cannot use is
    /// left untouched.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        // Setup must run one process at a time: SQLite reports busy at once, ignoring its timeout,
        // when two connections switch a new database to WAL together.
        let setup_lock = File::options()
            .create(true)
            .append(true)
            .open(path.with_added_extension("lock"))?;
        setup_lock.lock()?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| match e.sqlite_error_code() {
                Some(ErrorCode::NotADatabase) => StoreError::NotADatabase,
                _ => StoreError::Sqlite(e),
            })?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        if version < 1 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;"
            ))?;
        }
        if version < 2 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA_V2} PRAGMA user_version = 2; COMMIT;"
            ))?;
        }
        if version < 3 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA_V3} PRAGMA user_version = 3; COMMIT;"
            ))?;
        }
        Ok(Self { conn })
    }

    /// Replaces a memory.
    pub fn put_memory(&self, memory: &StoredMemory) -> Result<(), StoreError> {
        self.put_memories(std::slice::from_ref(memory))
    }

    /// Replaces many memories in one transaction.
    pub fn put_memories(&self, memories: &[StoredMemory]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached(&format!(
                "INSERT OR REPLACE INTO memories ({MEMORY_COLUMNS}) VALUES ({MEMORY_VALUES})"
            ))?;
            for memory in memories {
                execute_with_memory(&mut insert, memory)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores memories written on this device and queues them for push, in one transaction. Each
    /// gets `writer`, and an empty `hlc` gets the next stamp of the device clock. A given `hlc` is
    /// kept, and the clock moves past it.
    pub fn write_local(
        &self,
        memories: &mut [StoredMemory],
        writer: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let tx = self.clock_transaction()?;
        write_local_in(&tx, memories, writer, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Stores the facts a queued turn gave as a local write and marks the turn extracted, in one
    /// transaction, so a turn is never half written. Each `(old, new)` in `supersedes` marks
    /// `old` as replaced by `new` if `old` is still live when the transaction runs; a marked
    /// row is added to `memories`.
    pub fn finish_turn(
        &self,
        turn_id: i64,
        memories: &mut Vec<StoredMemory>,
        supersedes: &[(String, String)],
        writer: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let tx = self.clock_transaction()?;
        // Read inside the transaction: a pull may have deleted or replaced the old fact since
        // the turn was decided, and writing an older copy would undo that.
        for (old_id, new_id) in supersedes {
            if let Some(mut old) = self.get_memory(old_id)?
                && !old.record.deleted
                && old.record.superseded_by.is_none()
            {
                old.record.superseded_by = Some(new_id.clone());
                old.record.hlc = String::new();
                memories.push(old);
            }
        }
        write_local_in(&tx, memories, writer, now_ms)?;
        tx.execute(
            "UPDATE turn_queue SET status = ?2 WHERE id = ?1",
            params![turn_id, TurnStatus::Extracted.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A transaction that holds the write lock from its start.
    fn clock_transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        // Immediate: the clock is read, then written, and another process may write it too.
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    /// Stores each pulled memory whose `hlc` is newer than the local copy, the cursor after the
    /// page, the service's count of live memories, and the device clock moved past every stamp in
    /// the page, in one transaction. Returns the memories it stored.
    pub fn apply_pulled(
        &self,
        memories: Vec<StoredMemory>,
        cursor: i64,
        total: i64,
        writer: &str,
    ) -> Result<Vec<StoredMemory>, StoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut clock = read_clock(&tx, writer)?;
        let mut stored = Vec::with_capacity(memories.len());
        {
            let mut upsert = tx.prepare_cached(&format!(
                "INSERT INTO memories ({MEMORY_COLUMNS}) VALUES ({MEMORY_VALUES})
                 ON CONFLICT (memory_id) DO UPDATE SET
                     type = excluded.type, kind = excluded.kind, scope = excluded.scope,
                     project_id = excluded.project_id, text = excluded.text,
                     created_at = excluded.created_at, hlc = excluded.hlc,
                     writer = excluded.writer, superseded_by = excluded.superseded_by,
                     deleted = excluded.deleted, seq = excluded.seq, vector = excluded.vector,
                     model_id = excluded.model_id
                 WHERE excluded.hlc > memories.hlc"
            ))?;
            // A local change that lost to a newer one has nothing left to push.
            let mut unqueue = tx.prepare_cached("DELETE FROM outbox WHERE memory_id = ?1")?;
            for memory in memories {
                observe(&mut clock, &memory.record.hlc);
                if execute_with_memory(&mut upsert, &memory)? == 1 {
                    unqueue.execute([&memory.record.memory_id])?;
                    stored.push(memory);
                }
            }
        }
        save_state(&tx, CURSOR_KEY, &cursor.to_string())?;
        save_state(&tx, TOTAL_KEY, &total.to_string())?;
        save_state(&tx, CLOCK_KEY, &clock.to_string())?;
        tx.commit()?;
        Ok(stored)
    }

    /// Reads the `hlc` of a memory, if the device has it.
    pub fn hlc_of(&self, memory_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT hlc FROM memories WHERE memory_id = ?1",
                [memory_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Sets the vector of a memory, unless it has changed since `hlc`. True when it was set.
    pub fn set_vector(
        &self,
        memory_id: &str,
        hlc: &str,
        vector: &[f32],
        model_id: &str,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE memories SET vector = ?3, model_id = ?4 WHERE memory_id = ?1 AND hlc = ?2",
            params![memory_id, hlc, vector_bytes(vector), model_id],
        )?;
        Ok(changed == 1)
    }

    /// The `next_cursor` of the last pulled page, or 0 before the first pull.
    pub fn sync_cursor(&self) -> Result<i64, StoreError> {
        Ok(self.state_number(CURSOR_KEY)?.unwrap_or(0))
    }

    /// How much of the user's memory this device can recall, embedded with `model_id`.
    pub fn progress(&self, model_id: &str) -> Result<Progress, StoreError> {
        let (recallable, outbox) = self.conn.query_row(
            "SELECT
                 (SELECT count(*) FROM memories WHERE deleted = 0 AND model_id = ?1),
                 (SELECT count(*) FROM outbox)",
            [model_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Progress {
            recallable,
            total: self.state_number(TOTAL_KEY)?,
            outbox,
        })
    }

    fn state_number(&self, key: &str) -> Result<Option<i64>, StoreError> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// Number of memories waiting to be pushed.
    pub fn outbox_count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM outbox", [], |row| row.get(0))?)
    }

    /// Reads up to `limit` memories waiting to be pushed, oldest first. Queued ids with no memory
    /// are removed.
    pub fn outbox_batch(&self, limit: usize) -> Result<Vec<StoredMemory>, StoreError> {
        self.conn.execute(
            "DELETE FROM outbox WHERE memory_id NOT IN (SELECT memory_id FROM memories)",
            [],
        )?;
        let mut query = self.conn.prepare_cached(&format!(
            "SELECT {MEMORY_COLUMNS} FROM outbox JOIN memories USING (memory_id)
             ORDER BY outbox.rowid LIMIT ?1"
        ))?;
        let rows = query.query_map([limit as i64], memory_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Takes pushed memories out of the outbox. A memory changed since its push keeps its row, so
    /// the change is pushed next time.
    pub fn remove_pushed(&self, pushed: &[(&str, &str)]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut delete = tx.prepare_cached(
                "DELETE FROM outbox WHERE memory_id = ?1
                     AND EXISTS (SELECT 1 FROM memories WHERE memory_id = ?1 AND hlc = ?2)",
            )?;
            for (memory_id, hlc) in pushed {
                delete.execute([memory_id, hlc])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Takes a memory out of the outbox whatever its state, such as one the service refuses.
    pub fn remove_from_outbox(&self, memory_id: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM outbox WHERE memory_id = ?1", [memory_id])?;
        Ok(())
    }

    /// Reads one memory.
    pub fn get_memory(&self, memory_id: &str) -> Result<Option<StoredMemory>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE memory_id = ?1"),
                [memory_id],
                memory_from_row,
            )
            .optional()?)
    }

    /// Visits every memory.
    pub fn for_each_memory(&self, mut visit: impl FnMut(StoredMemory)) -> Result<(), StoreError> {
        let mut query = self
            .conn
            .prepare_cached(&format!("SELECT {MEMORY_COLUMNS} FROM memories"))?;
        for memory in query.query_map([], memory_from_row)? {
            // One unreadable row must not take the rest of memory down with it.
            match memory {
                Ok(memory) => visit(memory),
                Err(e) => tracing::error!(error = %e, "skipped unreadable memory"),
            }
        }
        Ok(())
    }

    /// Reads current facts eligible for a project, without their vectors.
    pub fn live_facts(&self, project_id: &str) -> Result<Vec<Record>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT memory_id, type, kind, scope, project_id, text, created_at, hlc, writer, \
                 superseded_by, deleted, seq, NULL, NULL
             FROM memories
             WHERE type = 'fact' AND deleted = 0 AND superseded_by IS NULL
                 AND (scope = 'user' OR (scope = 'project' AND project_id = ?1))",
        )?;
        let rows = query.query_map([project_id], memory_from_row)?;
        Ok(rows
            .filter_map(|row| match row {
                Ok(memory) => Some(memory.record),
                Err(e) => {
                    tracing::error!(error = %e, "skipped unreadable memory");
                    None
                }
            })
            .collect())
    }

    /// Creates a session, or points an existing one at its current transcript and reopens it.
    /// Capture progress is kept.
    pub fn upsert_session(
        &self,
        session_id: &str,
        agent: &str,
        project_id: &str,
        transcript_path: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO sessions (session_id, agent, project_id, transcript_path)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id) DO UPDATE SET
                 transcript_path = COALESCE(excluded.transcript_path, transcript_path),
                 ended = 0",
            params![session_id, agent, project_id, transcript_path],
        )?;
        Ok(())
    }

    /// Reads every session that has not ended.
    pub fn open_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT session_id, agent, project_id, transcript_path, transcript_offset,
                 last_turn_at, ended
             FROM sessions WHERE ended = 0",
        )?;
        let sessions = query.query_map([], session_from_row)?;
        Ok(sessions.collect::<Result<_, _>>()?)
    }

    /// Reads one session.
    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id, agent, project_id, transcript_path, transcript_offset,
                     last_turn_at, ended
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                session_from_row,
            )
            .optional()?)
    }

    /// Moves a session's transcript position.
    pub fn set_offset(&self, session_id: &str, offset: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET transcript_offset = ?2 WHERE session_id = ?1",
            params![session_id, offset as i64],
        )?;
        Ok(())
    }

    /// Queues a prepared turn and advances the transcript position atomically, so a crash can
    /// neither lose the turn nor queue it twice.
    pub fn commit_turn(&self, turn: QueuedTurn<'_>) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO turn_queue (session_id, agent, project_id, turn, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                turn.session_id,
                turn.agent,
                turn.project_id,
                turn.turn,
                turn.now_ms
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET transcript_offset = ?2, last_turn_at = ?3, ended = 0
             WHERE session_id = ?1",
            params![turn.session_id, turn.next_offset as i64, turn.now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The oldest queued turn that still needs extraction.
    pub fn next_pending_turn(&self) -> Result<Option<PendingTurn>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, session_id, project_id, turn FROM turn_queue
                 WHERE status = 'pending' ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok(PendingTurn {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        project_id: row.get(2)?,
                        turn: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_turn_status(&self, id: i64, status: TurnStatus) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE turn_queue SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    /// Counts one more failed try of a queued turn. Returns the count so far.
    pub fn count_failed_attempt(&self, id: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "UPDATE turn_queue SET attempts = attempts + 1 WHERE id = ?1 RETURNING attempts",
            [id],
            |row| row.get(0),
        )?)
    }

    /// The session with the oldest queued row whose rows are all past extraction, if it has ended
    /// or had no turn for `idle_ms`. Its rows come oldest first.
    pub fn due_episode(&self, now_ms: i64, idle_ms: i64) -> Result<Option<DueEpisode>, StoreError> {
        // One statement, so a row queued meanwhile cannot slip in unextracted.
        let mut query = self.conn.prepare_cached(
            "WITH due AS (
                 SELECT session_id, sessions.project_id FROM turn_queue
                 JOIN sessions USING (session_id)
                 GROUP BY session_id
                 HAVING sum(status = 'pending') = 0
                     AND (sessions.ended = 1 OR sessions.last_turn_at <= ?1 - ?2)
                 ORDER BY min(turn_queue.id) LIMIT 1
             )
             SELECT due.session_id, due.project_id, turn_queue.id, turn_queue.turn
             FROM due JOIN turn_queue USING (session_id) ORDER BY turn_queue.id",
        )?;
        let mut rows = query.query(params![now_ms, idle_ms])?;
        let mut due: Option<DueEpisode> = None;
        while let Some(row) = rows.next()? {
            let turn = (row.get(2)?, row.get(3)?);
            match &mut due {
                Some(due) => due.turns.push(turn),
                None => {
                    due = Some(DueEpisode {
                        session_id: row.get(0)?,
                        project_id: row.get(1)?,
                        turns: vec![turn],
                    })
                }
            }
        }
        Ok(due)
    }

    /// The earliest time an open session with queued rows becomes due by `idle_ms`.
    pub fn next_episode_at(&self, idle_ms: i64) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT min(last_turn_at) + ?1 FROM sessions
             WHERE ended = 0
                 AND EXISTS (SELECT 1 FROM turn_queue WHERE turn_queue.session_id = sessions.session_id)",
            [idle_ms],
            |row| row.get(0),
        )?)
    }

    /// Counts one more failed episode try of a session. Returns the count so far.
    pub fn count_episode_failure(&self, session_id: &str) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "UPDATE sessions SET episode_attempts = episode_attempts + 1 WHERE session_id = ?1
             RETURNING episode_attempts",
            [session_id],
            |row| row.get(0),
        )?)
    }

    /// Stores a session's episode, if any, as a local write, deletes the queued rows it was made
    /// from and resets the session's failure count, in one transaction.
    pub fn finish_episode(
        &self,
        session_id: &str,
        turn_ids: &[i64],
        episode: Option<&mut StoredMemory>,
        writer: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let tx = self.clock_transaction()?;
        write_local_in(
            &tx,
            episode.map_or(&mut [], std::slice::from_mut),
            writer,
            now_ms,
        )?;
        {
            let mut delete = tx.prepare_cached("DELETE FROM turn_queue WHERE id = ?1")?;
            for id in turn_ids {
                delete.execute([id])?;
            }
        }
        tx.execute(
            "UPDATE sessions SET episode_attempts = 0 WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Marks a session as ended.
    pub fn end_session(&self, session_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET ended = 1 WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    /// Records that a memory was injected in a session.
    pub fn mark_injected(&self, session_id: &str, memory_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO injected (session_id, memory_id) VALUES (?1, ?2)",
            [session_id, memory_id],
        )?;
        Ok(())
    }

    /// Forgets what a session was given, after its context has lost those memories.
    pub fn clear_injected(&self, session_id: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM injected WHERE session_id = ?1", [session_id])?;
        Ok(())
    }

    /// True when a memory was already injected in a session.
    pub fn was_injected(&self, session_id: &str, memory_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM injected WHERE session_id = ?1 AND memory_id = ?2)",
            [session_id, memory_id],
            |row| row.get(0),
        )?)
    }

    /// Counter that changes when another process commits to the database.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }
}

/// The body of a local write, inside the caller's transaction.
fn write_local_in(
    tx: &Transaction<'_>,
    memories: &mut [StoredMemory],
    writer: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    let mut clock = read_clock(tx, writer)?;
    let mut insert = tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO memories ({MEMORY_COLUMNS}) VALUES ({MEMORY_VALUES})"
    ))?;
    // A row already queued keeps its place, so the oldest change is pushed first.
    let mut queue = tx.prepare_cached("INSERT OR IGNORE INTO outbox (memory_id) VALUES (?1)")?;
    for memory in memories.iter_mut() {
        let record = &mut memory.record;
        if record.hlc.is_empty() {
            record.hlc = clock.tick(now_ms.max(0) as u64);
        } else {
            observe(&mut clock, &record.hlc);
        }
        record.writer = writer.to_owned();
        execute_with_memory(&mut insert, memory)?;
        queue.execute([&memory.record.memory_id])?;
    }
    save_state(tx, CLOCK_KEY, &clock.to_string())?;
    Ok(())
}

fn execute_with_memory(
    statement: &mut CachedStatement<'_>,
    memory: &StoredMemory,
) -> rusqlite::Result<usize> {
    let StoredMemory {
        record,
        vector,
        model_id,
    } = memory;
    statement.execute(params![
        record.memory_id,
        record.record_type.map(RecordType::as_str),
        record.kind.map(Kind::as_str),
        record.scope.map(Scope::as_str),
        record.project_id,
        record.text,
        record.created_at,
        record.hlc,
        record.writer,
        record.superseded_by,
        record.deleted,
        record.seq,
        vector.as_deref().map(vector_bytes),
        model_id,
    ])
}

/// The device clock for `writer`, moved past the last stamp this device saved.
fn read_clock(conn: &Connection, writer: &str) -> Result<Hlc, StoreError> {
    let mut clock = Hlc::new(writer);
    let saved: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = ?1",
            [CLOCK_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(saved) = saved {
        observe(&mut clock, &saved);
    }
    Ok(clock)
}

/// Moves the clock past a stamp. A malformed one cannot order anything, so it is skipped.
fn observe(clock: &mut Hlc, stamp: &str) {
    if let Err(e) = clock.observe(stamp) {
        tracing::warn!(error = %e, "clock skipped a stamp");
    }
}

fn save_state(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sync_state (key, value) VALUES (?1, ?2)",
        [key, value],
    )?;
    Ok(())
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        session_id: row.get(0)?,
        agent: row.get(1)?,
        project_id: row.get(2)?,
        transcript_path: row.get(3)?,
        transcript_offset: row.get::<_, i64>(4)? as u64,
        last_turn_at: row.get(5)?,
        ended: row.get(6)?,
    })
}

fn enum_from_text<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(index)?;
    text.map(|t| {
        serde_json::from_value(serde_json::Value::String(t)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    })
    .transpose()
}

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn vector_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

fn memory_from_row(row: &Row<'_>) -> rusqlite::Result<StoredMemory> {
    let record_type: Option<RecordType> = enum_from_text(row, 1)?;
    let kind: Option<Kind> = enum_from_text(row, 2)?;
    let scope: Option<Scope> = enum_from_text(row, 3)?;
    let vector: Option<Vec<u8>> = row.get(12)?;
    Ok(StoredMemory {
        record: Record {
            user_id: None,
            memory_id: row.get(0)?,
            seq: row.get(11)?,
            hlc: row.get(7)?,
            writer: row.get(8)?,
            record_type,
            kind,
            scope,
            project_id: row.get(4)?,
            text: row.get(5)?,
            created_at: row.get(6)?,
            superseded_by: row.get(9)?,
            deleted: row.get(10)?,
            evict_key: None,
        },
        vector: vector.as_deref().map(vector_from_bytes),
        model_id: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        (dir, store)
    }

    fn fact(id: &str) -> StoredMemory {
        StoredMemory {
            record: Record {
                user_id: None,
                memory_id: id.into(),
                seq: Some(4),
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: Some(RecordType::Fact),
                kind: Some(Kind::Preference),
                scope: Some(Scope::User),
                project_id: None,
                text: Some("Prefers pnpm over npm".into()),
                created_at: Some(1761750000123),
                superseded_by: None,
                deleted: false,
                evict_key: None,
            },
            vector: Some(vec![0.25, -1.5, 3.0]),
            model_id: Some("m".into()),
        }
    }

    #[test]
    fn fresh_store_has_the_schema() {
        let (_dir, store) = open_temp();
        let conn = &store.conn;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        for table in [
            "memories",
            "turn_queue",
            "sessions",
            "outbox",
            "sync_state",
            "injected",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{table}");
        }
    }

    #[test]
    fn reopening_keeps_the_schema_and_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        Store::open(&path).unwrap().put_memory(&fact("m1")).unwrap();
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        assert!(store.get_memory("m1").unwrap().is_some());
    }

    #[test]
    fn first_schema_store_gains_attempts_and_keeps_its_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA_V1}
                 INSERT INTO turn_queue (session_id, agent, project_id, turn, created_at)
                 VALUES ('s1', 'claude-code', 'p', 'first', 1), ('s1', 'claude-code', 'p', 'second', 2);
                 PRAGMA user_version = 1;"
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let rows: Vec<(String, String, i64)> = store
            .conn
            .prepare("SELECT turn, status, attempts FROM turn_queue ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("first".to_owned(), "pending".to_owned(), 0),
                ("second".to_owned(), "pending".to_owned(), 0),
            ]
        );
    }

    #[test]
    fn memory_round_trips_with_its_vector() {
        let (_dir, store) = open_temp();
        let memory = fact("m1");
        store.put_memory(&memory).unwrap();
        assert_eq!(store.get_memory("m1").unwrap().unwrap(), memory);
    }

    #[test]
    fn tombstone_round_trips_with_no_content() {
        let (_dir, store) = open_temp();
        let tombstone = StoredMemory {
            record: Record {
                user_id: None,
                memory_id: "m2".into(),
                seq: None,
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: None,
                kind: None,
                scope: None,
                project_id: None,
                text: None,
                created_at: None,
                superseded_by: None,
                deleted: true,
                evict_key: None,
            },
            vector: None,
            model_id: None,
        };
        store.put_memory(&tombstone).unwrap();
        assert_eq!(store.get_memory("m2").unwrap().unwrap(), tombstone);
    }

    #[test]
    fn unknown_ids_read_as_none() {
        let (_dir, store) = open_temp();
        assert!(store.get_memory("unknown").unwrap().is_none());
        assert!(store.get_session("unknown").unwrap().is_none());
    }

    #[test]
    fn offset_persists_on_the_session() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", Some("/t"))
            .unwrap();
        store.set_offset("s1", 4096).unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 4096);
        assert_eq!(session.transcript_path.as_deref(), Some("/t"));
        assert_eq!(session.agent, "claude-code");
        assert_eq!(session.project_id, "p");
    }

    #[test]
    fn upserting_keeps_progress_and_reopens_the_session() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", Some("/t"))
            .unwrap();
        store.set_offset("s1", 10).unwrap();
        store.end_session("s1").unwrap();
        assert!(store.get_session("s1").unwrap().unwrap().ended);
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 10);
        assert_eq!(session.transcript_path.as_deref(), Some("/t"));
        assert!(!session.ended);
    }

    #[test]
    fn injected_is_recorded_once_per_session() {
        let (_dir, store) = open_temp();
        assert!(!store.was_injected("s1", "m1").unwrap());
        store.mark_injected("s1", "m1").unwrap();
        store.mark_injected("s1", "m1").unwrap();
        assert!(store.was_injected("s1", "m1").unwrap());
        assert!(!store.was_injected("s2", "m1").unwrap());
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM injected", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        store.clear_injected("s1").unwrap();
        assert!(!store.was_injected("s1", "m1").unwrap());
    }

    #[test]
    fn newer_schema_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t (a); PRAGMA user_version = 4;")
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::SchemaTooNew {
                found: 4,
                supported: 3
            })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn non_database_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        std::fs::write(&path, "not a database").unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::NotADatabase)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a database");
    }

    #[test]
    fn setup_lock_that_cannot_be_created_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        std::fs::create_dir(path.with_added_extension("lock")).unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::SetupLock(_))));
    }

    #[test]
    fn opening_an_unusable_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Store::open(dir.path()),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn commit_turn_queues_and_advances_together() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        let turn = QueuedTurn {
            session_id: "s1",
            agent: "claude-code",
            project_id: "p",
            turn: "{}",
            next_offset: 99,
            now_ms: 5,
        };
        store.commit_turn(turn).unwrap();
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.transcript_offset, 99);
        assert_eq!(session.last_turn_at, Some(5));
        let status: String = store
            .conn
            .query_row("SELECT status FROM turn_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn finishing_a_turn_stores_its_facts_and_marks_it_extracted_together() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store
            .commit_turn(QueuedTurn {
                session_id: "s1",
                agent: "claude-code",
                project_id: "p",
                turn: "{}",
                next_offset: 1,
                now_ms: 5,
            })
            .unwrap();
        let turn = store.next_pending_turn().unwrap().unwrap();
        let mut facts = vec![fact("A"), fact("B")];
        store
            .finish_turn(turn.id, &mut facts, &[], "dev_laptop", 10)
            .unwrap();
        assert!(store.next_pending_turn().unwrap().is_none());
        assert_eq!(store.outbox_count().unwrap(), 2);
        let stored = store.get_memory("A").unwrap().unwrap();
        assert_eq!(stored.record.writer, "dev_laptop");
        assert!(!stored.record.hlc.is_empty());
        assert_eq!(
            facts[1].record.hlc,
            store.get_memory("B").unwrap().unwrap().record.hlc
        );
    }

    #[test]
    fn finishing_a_turn_marks_only_old_facts_that_are_still_live() {
        let (_dir, store) = open_temp();
        let mut gone = fact("GONE");
        gone.record.deleted = true;
        let mut replaced = fact("REPLACED");
        replaced.record.superseded_by = Some("OTHER".to_owned());
        store
            .put_memories(&[fact("LIVE"), gone.clone(), replaced.clone()])
            .unwrap();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store
            .commit_turn(QueuedTurn {
                session_id: "s1",
                agent: "claude-code",
                project_id: "p",
                turn: "{}",
                next_offset: 1,
                now_ms: 5,
            })
            .unwrap();
        let turn = store.next_pending_turn().unwrap().unwrap();
        let mut facts = vec![fact("N1"), fact("N2"), fact("N3")];
        let supersedes = [
            ("LIVE".to_owned(), "N1".to_owned()),
            ("GONE".to_owned(), "N2".to_owned()),
            ("REPLACED".to_owned(), "N3".to_owned()),
            ("MISSING".to_owned(), "N3".to_owned()),
        ];
        store
            .finish_turn(turn.id, &mut facts, &supersedes, "dev_laptop", 10)
            .unwrap();
        let live = store.get_memory("LIVE").unwrap().unwrap();
        assert_eq!(live.record.superseded_by.as_deref(), Some("N1"));
        assert_eq!(live.record.writer, "dev_laptop");
        assert_eq!(
            store.get_memory("GONE").unwrap().unwrap().record,
            gone.record
        );
        assert_eq!(
            store.get_memory("REPLACED").unwrap().unwrap().record,
            replaced.record
        );
        assert!(store.get_memory("MISSING").unwrap().is_none());
        assert_eq!(facts.len(), 4);
        assert_eq!(store.outbox_count().unwrap(), 4);
    }

    #[test]
    fn finishing_a_turn_with_no_facts_marks_it_extracted() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store
            .commit_turn(QueuedTurn {
                session_id: "s1",
                agent: "claude-code",
                project_id: "p",
                turn: "{}",
                next_offset: 1,
                now_ms: 5,
            })
            .unwrap();
        let turn = store.next_pending_turn().unwrap().unwrap();
        store
            .finish_turn(turn.id, &mut Vec::new(), &[], "dev_laptop", 10)
            .unwrap();
        assert!(store.next_pending_turn().unwrap().is_none());
        assert_eq!(store.outbox_count().unwrap(), 0);
    }

    #[test]
    fn pending_turns_come_oldest_first_until_their_status_changes() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        for (turn, next_offset) in [("first", 1), ("second", 2), ("third", 3)] {
            store
                .commit_turn(QueuedTurn {
                    session_id: "s1",
                    agent: "claude-code",
                    project_id: "p",
                    turn,
                    next_offset,
                    now_ms: 5,
                })
                .unwrap();
        }
        let first = store.next_pending_turn().unwrap().unwrap();
        assert_eq!(
            (
                first.session_id.as_str(),
                first.project_id.as_str(),
                first.turn.as_str()
            ),
            ("s1", "p", "first")
        );
        assert_eq!(store.count_failed_attempt(first.id).unwrap(), 1);
        assert_eq!(store.count_failed_attempt(first.id).unwrap(), 2);
        assert_eq!(store.count_failed_attempt(first.id).unwrap(), 3);
        assert_eq!(store.next_pending_turn().unwrap().unwrap().id, first.id);

        store
            .set_turn_status(first.id, TurnStatus::Extracted)
            .unwrap();
        let second = store.next_pending_turn().unwrap().unwrap();
        assert_eq!(second.turn, "second");
        store
            .set_turn_status(second.id, TurnStatus::Failed)
            .unwrap();
        let third = store.next_pending_turn().unwrap().unwrap();
        assert_eq!(third.turn, "third");
        store
            .set_turn_status(third.id, TurnStatus::Extracted)
            .unwrap();
        assert_eq!(store.next_pending_turn().unwrap(), None);

        store
            .set_turn_status(second.id, TurnStatus::Pending)
            .unwrap();
        assert_eq!(store.next_pending_turn().unwrap().unwrap().id, second.id);
        let statuses: Vec<String> = store
            .conn
            .prepare("SELECT status FROM turn_queue ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(statuses, ["extracted", "pending", "extracted"]);
    }

    #[test]
    fn failed_commit_rolls_back_the_queue_insert() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON sessions
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let turn = QueuedTurn {
            session_id: "s1",
            agent: "claude-code",
            project_id: "p",
            turn: "{}",
            next_offset: 99,
            now_ms: 5,
        };
        assert!(store.commit_turn(turn).is_err());
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM turn_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        store
            .conn
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        assert_eq!(
            store.get_session("s1").unwrap().unwrap().transcript_offset,
            0
        );
    }

    #[test]
    fn live_facts_skip_other_projects_episodes_and_dead_rows() {
        let (_dir, store) = open_temp();
        let mut project = fact("project");
        project.record.scope = Some(Scope::Project);
        project.record.project_id = Some("x".into());
        let mut other = project.clone();
        other.record.memory_id = "other".into();
        other.record.project_id = Some("y".into());
        let mut episode = fact("episode");
        episode.record.record_type = Some(RecordType::Episode);
        let mut superseded = fact("superseded");
        superseded.record.superseded_by = Some("user".into());
        let mut deleted = fact("deleted");
        deleted.record.deleted = true;
        store
            .put_memories(&[fact("user"), project, other, episode, superseded, deleted])
            .unwrap();
        let mut ids: Vec<String> = store
            .live_facts("x")
            .unwrap()
            .into_iter()
            .map(|r| r.memory_id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["project", "user"]);
    }

    #[test]
    fn for_each_visits_all_memories() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        let mut seen = 0;
        store.for_each_memory(|_| seen += 1).unwrap();
        assert_eq!(seen, 2);
    }

    #[test]
    fn listing_skips_an_unreadable_row() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense' WHERE memory_id = 'a'")
            .unwrap();
        let mut seen = Vec::new();
        store
            .for_each_memory(|m| seen.push(m.record.memory_id))
            .unwrap();
        assert_eq!(seen, ["b"]);
    }

    #[test]
    fn live_facts_skip_an_unreadable_row() {
        let (_dir, store) = open_temp();
        store.put_memories(&[fact("a"), fact("b")]).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense' WHERE memory_id = 'a'")
            .unwrap();
        let facts = store.live_facts("p").unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].memory_id, "b");
    }

    #[test]
    fn open_sessions_leave_out_ended_ones() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("open", "claude-code", "p", None)
            .unwrap();
        store
            .upsert_session("done", "claude-code", "p", None)
            .unwrap();
        store.end_session("done").unwrap();
        let open: Vec<String> = store
            .open_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(open, ["open"]);
    }

    #[test]
    fn concurrent_opens_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open(&path).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    }

    #[test]
    fn corrupt_enum_text_is_a_read_error() {
        let (_dir, store) = open_temp();
        store.put_memory(&fact("m1")).unwrap();
        store
            .conn
            .execute_batch("UPDATE memories SET kind = 'nonsense'")
            .unwrap();
        assert!(store.get_memory("m1").is_err());
    }

    #[test]
    fn operations_report_sql_errors() {
        let (_dir, store) = open_temp();
        store
            .conn
            .execute_batch("DROP TABLE memories; DROP TABLE sessions; DROP TABLE injected; DROP TABLE turn_queue;")
            .unwrap();
        assert!(store.put_memory(&fact("m1")).is_err());
        assert!(store.get_memory("m1").is_err());
        assert!(store.for_each_memory(|_| ()).is_err());
        assert!(store.live_facts("p").is_err());
        assert!(store.upsert_session("s", "a", "p", None).is_err());
        assert!(store.get_session("s").is_err());
        assert!(store.open_sessions().is_err());
        assert!(store.set_offset("s", 1).is_err());
        assert!(store.end_session("s").is_err());
        assert!(store.mark_injected("s", "m").is_err());
        assert!(store.was_injected("s", "m").is_err());
        assert!(store.clear_injected("s").is_err());
        let turn = QueuedTurn {
            session_id: "s",
            agent: "a",
            project_id: "p",
            turn: "{}",
            next_offset: 1,
            now_ms: 1,
        };
        assert!(store.commit_turn(turn).is_err());
        assert!(store.write_local(&mut [fact("m1")], "w", 1).is_err());
        assert!(store.apply_pulled(vec![fact("m1")], 1, 1, "w").is_err());
        assert!(store.hlc_of("m1").is_err());
        assert!(store.set_vector("m1", "h", &[1.0], "m").is_err());
        assert!(store.outbox_batch(1).is_err());
        assert!(store.remove_pushed(&[("m1", "h")]).is_err());
        assert!(store.due_episode(1, 1).is_err());
        assert!(store.next_episode_at(1).is_err());
        assert!(store.count_episode_failure("s").is_err());
        assert!(store.finish_episode("s", &[1], None, "w", 1).is_err());
        assert!(store.finish_turn(1, &mut Vec::new(), &[], "w", 1).is_err());

        store
            .conn
            .execute_batch("DROP TABLE outbox; DROP TABLE sync_state;")
            .unwrap();
        assert!(store.sync_cursor().is_err());
        assert!(store.outbox_count().is_err());
        assert!(store.remove_from_outbox("m1").is_err());
        assert!(store.progress("m").is_err());
    }

    #[test]
    fn database_locked_by_another_process_is_a_sqlite_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("CREATE TABLE t (a); BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::Sqlite(_))));
    }

    #[test]
    fn data_version_changes_after_another_connection_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = Store::open(&path).unwrap();
        let before = store.data_version().unwrap();
        Store::open(&path).unwrap().put_memory(&fact("m1")).unwrap();
        assert_ne!(store.data_version().unwrap(), before);
    }

    fn unstamped(id: &str) -> StoredMemory {
        let mut memory = fact(id);
        memory.record.hlc = String::new();
        memory.record.writer = "local".into();
        memory
    }

    fn outbox(store: &Store) -> Vec<String> {
        let mut query = store
            .conn
            .prepare("SELECT memory_id FROM outbox ORDER BY rowid")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn saved_clock(store: &Store) -> String {
        store
            .conn
            .query_row("SELECT value FROM sync_state WHERE key = 'hlc'", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn local_writes_get_rising_stamps_and_are_queued() {
        let (_dir, store) = open_temp();
        let mut first = [unstamped("m1")];
        store
            .write_local(&mut first, "dev_a", 1761750000123)
            .unwrap();
        let mut second = [unstamped("m2")];
        store
            .write_local(&mut second, "dev_a", 1761750000123)
            .unwrap();

        assert_eq!(first[0].record.hlc, "1761750000123-00000-dev_a");
        assert_eq!(second[0].record.hlc, "1761750000123-00001-dev_a");
        let stored = store.get_memory("m2").unwrap().unwrap();
        assert_eq!(stored.record.writer, "dev_a");
        assert_eq!(stored.record.hlc, second[0].record.hlc);
        assert_eq!(outbox(&store), ["m1", "m2"]);
        assert_eq!(saved_clock(&store), "1761750000123-00001-dev_a");
    }

    #[test]
    fn a_given_stamp_is_kept_and_later_stamps_sort_after_it() {
        let (_dir, store) = open_temp();
        let mut given = [fact("m1")];
        given[0].record.hlc = "1761750009999-00007-dev_a".into();
        store
            .write_local(&mut given, "dev_a", 1761750000123)
            .unwrap();
        let mut next = [unstamped("m2")];
        store
            .write_local(&mut next, "dev_a", 1761750000123)
            .unwrap();

        assert_eq!(
            store.get_memory("m1").unwrap().unwrap().record.hlc,
            "1761750009999-00007-dev_a"
        );
        assert_eq!(next[0].record.hlc, "1761750009999-00008-dev_a");
    }

    #[test]
    fn the_saved_clock_holds_across_processes_and_writers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        Store::open(&path)
            .unwrap()
            .write_local(&mut [unstamped("m1")], "dev_a", 1761750000500)
            .unwrap();
        let mut later = [unstamped("m2")];
        Store::open(&path)
            .unwrap()
            .write_local(&mut later, "dev_b", 1761750000100)
            .unwrap();
        assert_eq!(later[0].record.hlc, "1761750000500-00001-dev_b");
    }

    #[test]
    fn a_rewrite_keeps_the_queue_place() {
        let (_dir, store) = open_temp();
        for id in ["m1", "m2", "m1"] {
            store.write_local(&mut [unstamped(id)], "dev_a", 1).unwrap();
        }
        assert_eq!(outbox(&store), ["m1", "m2"]);
    }

    #[test]
    fn outbox_batch_is_oldest_first_and_drops_rows_with_no_memory() {
        let (_dir, store) = open_temp();
        for id in ["m3", "m1", "m2"] {
            store.write_local(&mut [unstamped(id)], "dev_a", 1).unwrap();
        }
        store
            .conn
            .execute("DELETE FROM memories WHERE memory_id = 'm1'", [])
            .unwrap();

        assert_eq!(store.outbox_count().unwrap(), 3);
        let batch = store.outbox_batch(1).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].record.memory_id, "m3");
        assert_eq!(batch[0].vector, fact("m3").vector);
        let ids: Vec<String> = store
            .outbox_batch(10)
            .unwrap()
            .into_iter()
            .map(|m| m.record.memory_id)
            .collect();
        assert_eq!(ids, ["m3", "m2"]);
        assert_eq!(store.outbox_count().unwrap(), 2);
    }

    #[test]
    fn a_memory_changed_since_its_push_stays_queued() {
        let (_dir, store) = open_temp();
        let mut pushed = [unstamped("m1"), unstamped("m2")];
        store.write_local(&mut pushed, "dev_a", 1).unwrap();
        store
            .write_local(&mut [unstamped("m2")], "dev_a", 2)
            .unwrap();

        let sent: Vec<(&str, &str)> = pushed
            .iter()
            .map(|m| (m.record.memory_id.as_str(), m.record.hlc.as_str()))
            .collect();
        store.remove_pushed(&sent).unwrap();
        assert_eq!(outbox(&store), ["m2"]);

        store.remove_from_outbox("m2").unwrap();
        assert!(outbox(&store).is_empty());
    }

    fn pulled(id: &str, hlc: &str, text: &str) -> StoredMemory {
        let mut memory = fact(id);
        memory.record.hlc = hlc.into();
        memory.record.text = Some(text.into());
        memory.record.seq = Some(9);
        memory
    }

    #[test]
    fn pulled_memories_replace_only_older_copies() {
        let (_dir, store) = open_temp();
        store
            .put_memories(&[
                pulled("older", "1761750000100-00000-x", "old"),
                pulled("same", "1761750000200-00000-x", "mine"),
                pulled("newer", "1761750000900-00000-x", "mine"),
            ])
            .unwrap();
        let page = vec![
            pulled("older", "1761750000500-00000-y", "new"),
            pulled("same", "1761750000200-00000-x", "theirs"),
            pulled("newer", "1761750000500-00000-y", "theirs"),
            pulled("absent", "1761750000700-00000-y", "new"),
        ];

        let stored = store.apply_pulled(page, 42, 3, "dev_a").unwrap();
        let ids: Vec<&str> = stored.iter().map(|m| m.record.memory_id.as_str()).collect();
        assert_eq!(ids, ["older", "absent"]);
        let text = |id| store.get_memory(id).unwrap().unwrap().record.text.unwrap();
        assert_eq!(
            [text("older"), text("same"), text("newer"), text("absent")],
            ["new", "mine", "mine", "new"]
        );
        assert_eq!(store.sync_cursor().unwrap(), 42);
        assert_eq!(saved_clock(&store), "1761750000700-00000-dev_a");
        assert!(outbox(&store).is_empty());
    }

    #[test]
    fn a_newer_pulled_change_takes_the_local_one_out_of_the_outbox() {
        let (_dir, store) = open_temp();
        let mut local = [unstamped("lost"), unstamped("kept")];
        store
            .write_local(&mut local, "dev_a", 1761750000100)
            .unwrap();
        let page = vec![
            pulled("lost", "1761750000900-00000-dev_b", "theirs"),
            pulled("kept", "1761750000050-00000-dev_b", "older"),
        ];
        store.apply_pulled(page, 2, 2, "dev_a").unwrap();
        assert_eq!(outbox(&store), ["kept"]);
    }

    #[test]
    fn a_malformed_pulled_stamp_is_stored_but_does_not_move_the_clock() {
        let (_dir, store) = open_temp();
        store
            .apply_pulled(vec![pulled("m1", "garbage", "x")], 1, 1, "dev_a")
            .unwrap();
        assert!(store.get_memory("m1").unwrap().is_some());
        assert_eq!(saved_clock(&store), "0000000000000-00000-dev_a");
    }

    #[test]
    fn each_pulled_page_saves_the_service_total() {
        let (_dir, store) = open_temp();
        assert_eq!(store.progress("m").unwrap().total, None);
        store
            .apply_pulled(vec![fact("m1")], 1, 40, "dev_a")
            .unwrap();
        assert_eq!(store.progress("m").unwrap().total, Some(40));
        store
            .apply_pulled(vec![fact("m2")], 2, 41, "dev_a")
            .unwrap();
        assert_eq!(store.progress("m").unwrap().total, Some(41));
    }

    #[test]
    fn progress_counts_live_rows_with_a_current_vector_and_the_outbox() {
        let (_dir, store) = open_temp();
        let mut gone = fact("gone");
        gone.record.deleted = true;
        let mut unembedded = fact("unembedded");
        (unembedded.vector, unembedded.model_id) = (None, None);
        let mut old_model = fact("old_model");
        old_model.model_id = Some("other".into());
        store
            .put_memories(&[fact("a"), fact("b"), gone, unembedded, old_model])
            .unwrap();
        store
            .write_local(&mut [unstamped("c")], "dev_a", 1)
            .unwrap();

        assert_eq!(
            store.progress("m").unwrap(),
            Progress {
                recallable: 3,
                total: None,
                outbox: 1
            }
        );
    }

    #[test]
    fn cursor_starts_at_zero() {
        let (_dir, store) = open_temp();
        assert_eq!(store.sync_cursor().unwrap(), 0);
    }

    #[test]
    fn a_vector_is_set_only_on_the_version_it_was_made_for() {
        let (_dir, store) = open_temp();
        store.put_memory(&fact("m1")).unwrap();
        let hlc = fact("m1").record.hlc;
        assert!(
            !store
                .set_vector("m1", "1761750000999-00000-w", &[1.0], "m2")
                .unwrap()
        );
        assert!(store.set_vector("m1", &hlc, &[1.0], "m2").unwrap());
        let stored = store.get_memory("m1").unwrap().unwrap();
        assert_eq!(
            (stored.vector, stored.model_id),
            (Some(vec![1.0]), Some("m2".into()))
        );
        assert_eq!(store.hlc_of("m1").unwrap(), Some(hlc));
        assert_eq!(store.hlc_of("m9").unwrap(), None);
    }

    #[test]
    fn second_schema_store_gains_episode_attempts_and_keeps_its_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA_V1} {SCHEMA_V2}
                 INSERT INTO sessions (session_id, agent, project_id, last_turn_at, ended)
                 VALUES ('s1', 'claude-code', 'p', 7, 1);
                 PRAGMA user_version = 2;"
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let (version, attempts): (i64, i64) = store
            .conn
            .query_row(
                "SELECT (SELECT user_version FROM pragma_user_version), episode_attempts FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((version, attempts), (3, 0));
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(
            (
                session.project_id.as_str(),
                session.last_turn_at,
                session.ended
            ),
            ("p", Some(7), true)
        );
    }

    fn queue(store: &Store, session_id: &str, turn: &str, now_ms: i64) {
        store
            .commit_turn(QueuedTurn {
                session_id,
                agent: "claude-code",
                project_id: "ignored",
                turn,
                next_offset: 1,
                now_ms,
            })
            .unwrap();
    }

    fn mark_all(store: &Store, status: TurnStatus) {
        store
            .conn
            .execute("UPDATE turn_queue SET status = ?1", [status.as_str()])
            .unwrap();
    }

    fn queued_turns(store: &Store) -> Vec<String> {
        store
            .conn
            .prepare("SELECT turn FROM turn_queue ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_turn_reopens_an_ended_session() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store.end_session("s1").unwrap();
        queue(&store, "s1", "after", 5);
        assert!(!store.get_session("s1").unwrap().unwrap().ended);
    }

    const IDLE: i64 = 1_000;

    #[test]
    fn no_queued_rows_means_no_episode() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        store.end_session("s1").unwrap();
        assert_eq!(store.due_episode(i64::MAX / 2, IDLE).unwrap(), None);
        assert_eq!(store.next_episode_at(IDLE).unwrap(), None);
    }

    #[test]
    fn a_pending_row_holds_the_session_back() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        queue(&store, "s1", "first", 5);
        mark_all(&store, TurnStatus::Extracted);
        queue(&store, "s1", "second", 6);
        store.end_session("s1").unwrap();
        assert_eq!(store.due_episode(6 + IDLE, IDLE).unwrap(), None);
    }

    #[test]
    fn an_ended_session_is_due_at_once_with_its_rows_in_order() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        for (turn, at) in [("first", 5), ("second", 6), ("third", 7)] {
            queue(&store, "s1", turn, at);
        }
        mark_all(&store, TurnStatus::Extracted);
        store
            .conn
            .execute(
                "UPDATE turn_queue SET status = 'failed' WHERE turn = 'second'",
                [],
            )
            .unwrap();
        store.end_session("s1").unwrap();

        let due = store.due_episode(7, IDLE).unwrap().unwrap();
        assert_eq!(due.session_id, "s1");
        assert_eq!(due.project_id, "p");
        let turns: Vec<&str> = due.turns.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(turns, ["first", "second", "third"]);
        assert!(due.turns.is_sorted_by_key(|(id, _)| *id));
    }

    #[test]
    fn an_open_session_is_due_after_the_idle_time_from_its_last_turn() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        queue(&store, "s1", "first", 5);
        queue(&store, "s1", "second", 100);
        mark_all(&store, TurnStatus::Extracted);
        assert_eq!(store.next_episode_at(IDLE).unwrap(), Some(100 + IDLE));
        assert_eq!(store.due_episode(100 + IDLE - 1, IDLE).unwrap(), None);
        let due = store.due_episode(100 + IDLE, IDLE).unwrap().unwrap();
        assert_eq!(due.session_id, "s1");
        assert_eq!(due.turns.len(), 2);
    }

    #[test]
    fn the_session_with_the_oldest_row_is_due_first() {
        let (_dir, store) = open_temp();
        for session in ["late", "early"] {
            store
                .upsert_session(session, "claude-code", session, None)
                .unwrap();
        }
        queue(&store, "early", "a", 5);
        queue(&store, "late", "b", 6);
        mark_all(&store, TurnStatus::Extracted);
        store.end_session("late").unwrap();
        store.end_session("early").unwrap();
        assert_eq!(
            store.due_episode(6, IDLE).unwrap().unwrap().project_id,
            "early"
        );
    }

    #[test]
    fn next_episode_at_skips_ended_sessions_and_sessions_with_no_rows() {
        let (_dir, store) = open_temp();
        for session in ["ended", "empty", "open"] {
            store
                .upsert_session(session, "claude-code", "p", None)
                .unwrap();
        }
        queue(&store, "ended", "a", 5);
        queue(&store, "empty", "b", 6);
        queue(&store, "open", "c", 300);
        store.end_session("ended").unwrap();
        store
            .conn
            .execute("DELETE FROM turn_queue WHERE session_id = 'empty'", [])
            .unwrap();
        assert_eq!(store.next_episode_at(IDLE).unwrap(), Some(300 + IDLE));
    }

    #[test]
    fn episode_failures_are_counted_per_session_and_reset_when_finished() {
        let (_dir, store) = open_temp();
        for session in ["s1", "s2"] {
            store
                .upsert_session(session, "claude-code", "p", None)
                .unwrap();
        }
        assert_eq!(store.count_episode_failure("s1").unwrap(), 1);
        assert_eq!(store.count_episode_failure("s1").unwrap(), 2);
        assert_eq!(store.count_episode_failure("s1").unwrap(), 3);
        assert_eq!(store.count_episode_failure("s2").unwrap(), 1);
        store.finish_episode("s1", &[], None, "dev_a", 1).unwrap();
        assert_eq!(store.count_episode_failure("s1").unwrap(), 1);
    }

    #[test]
    fn finishing_an_episode_stores_it_and_deletes_only_the_sent_rows() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        queue(&store, "s1", "sent", 5);
        queue(&store, "s1", "later", 6);
        let sent = store.next_pending_turn().unwrap().unwrap().id;
        let mut episode = unstamped("e1");
        episode.record.record_type = Some(RecordType::Episode);

        store
            .finish_episode("s1", &[sent], Some(&mut episode), "dev_a", 1761750000123)
            .unwrap();

        assert_eq!(episode.record.hlc, "1761750000123-00000-dev_a");
        assert_eq!(store.get_memory("e1").unwrap().unwrap(), episode);
        assert_eq!(outbox(&store), ["e1"]);
        assert_eq!(queued_turns(&store), ["later"]);
    }

    #[test]
    fn finishing_with_no_episode_writes_no_memory() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        queue(&store, "s1", "sent", 5);
        let sent = store.next_pending_turn().unwrap().unwrap().id;
        store
            .finish_episode("s1", &[sent], None, "dev_a", 1)
            .unwrap();
        let mut memories = 0;
        store.for_each_memory(|_| memories += 1).unwrap();
        assert_eq!(memories, 0);
        assert!(outbox(&store).is_empty());
        assert!(queued_turns(&store).is_empty());
    }

    #[test]
    fn a_failed_finish_keeps_the_rows_and_writes_nothing() {
        let (_dir, store) = open_temp();
        store
            .upsert_session("s1", "claude-code", "p", None)
            .unwrap();
        queue(&store, "s1", "sent", 5);
        let sent = store.next_pending_turn().unwrap().unwrap().id;
        store.count_episode_failure("s1").unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON sessions
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let mut episode = unstamped("e1");
        assert!(
            store
                .finish_episode("s1", &[sent], Some(&mut episode), "dev_a", 1)
                .is_err()
        );
        store
            .conn
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        assert_eq!(store.get_memory("e1").unwrap(), None);
        assert!(outbox(&store).is_empty());
        assert_eq!(queued_turns(&store), ["sent"]);
        assert_eq!(store.count_episode_failure("s1").unwrap(), 2);
    }
}

//! B1 transactional managed-agents store substrate.
//!
//! `managed-agents.json` / `teams.json` stay the canonical user-visible files.
//! `store-journal.sqlite` (beside them, at the **store-family anchor**) holds
//! shared recovery facts: operation records, per-key generation / tombstone
//! metadata, immutable inbox/outbox rows.
//!
//! **Anchor**: canonical dev `agents/` dir for shared worktrees
//! (`BUZZ_SHARE_IDENTITY=1`); the bundle's own `agents/` dir for standalone.
//! Lock identity is never derived from a possibly-absent file.
//!
//! **Lock sequence**: in-process `AppState::managed_agents_store_lock` →
//! anchored OS advisory lock (`flock`/named-mutex) → fresh decode → mutation
//! closure → atomic fsync write → release.  Network I/O and keyring access
//! stay outside every critical section.
//!
//! **Posture**: protects against crashes and concurrent cooperating processes
//! on one machine only.  Does not defend against adversarial same-user
//! tampering, cross-machine duplication, supply-chain attack, mixed-version
//! writers, sign-out/reset races, or concurrent bundles.

use std::{
    path::{Path, PathBuf},
    sync::MutexGuard,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};
use tauri::{AppHandle, Manager};

use crate::managed_agents::{ManagedAgentRecord, TeamRecord};
use crate::migration::is_dev_data_dir_name;

// ── Constants ────────────────────────────────────────────────────────────────

const CANONICAL_DEV_IDENTIFIER: &str = "xyz.block.buzz.app.dev";

/// Journal filename, beside `managed-agents.json`.
const JOURNAL_FILENAME: &str = "store-journal.sqlite";

/// Advisory lockfile name, beside `managed-agents.json`.
const ADVISORY_LOCK_FILENAME: &str = "store-journal.lock";

// ── Store-family anchor ──────────────────────────────────────────────────────

/// Resolve the store-family anchor directory.
///
/// For shared dev worktrees (`BUZZ_SHARE_IDENTITY=1`): the canonical dev
/// `agents/` dir, returned **unconditionally** regardless of whether it
/// exists yet.  Lock acquisition calls `create_dir_all`, so absent-on-first-
/// boot is not a reason to fall back.  Falling back on absence would let two
/// simultaneous first-boot processes each choose their own local dir, giving
/// them different lock/journal authorities and making shared-state recovery
/// impossible (v34.1 §1).
///
/// For standalone: `app_data_dir()/agents`.  Never derived from
/// `managed-agents.json` — an absent file must never determine lock identity.
pub fn store_anchor_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let local_agents = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?
        .join("agents");

    // Only redirect to the canonical dev anchor when identity-sharing is active.
    let is_shared = std::env::var("BUZZ_SHARE_IDENTITY")
        .map(|v| v == "1")
        .unwrap_or(false);

    if is_shared {
        if let Some(anchor) = canonical_dev_anchor(&local_agents) {
            // Return the canonical dev path UNCONDITIONALLY — do not branch on
            // anchor.exists().  Lock acquisition will create the directory.
            return Ok(anchor);
        }
    }

    Ok(local_agents)
}

/// Compute the canonical dev anchor from a local `agents/` path.
/// Returns `None` when the path structure is unexpected.
/// Exposed as `canonical_dev_anchor_pub` for tests.
#[cfg(test)]
pub fn canonical_dev_anchor_pub(local_agents: &Path) -> Option<PathBuf> {
    canonical_dev_anchor(local_agents)
}

fn canonical_dev_anchor(local_agents: &Path) -> Option<PathBuf> {
    // local_agents = <AppDataDir>/agents
    // AppDataDir   = <parent>/<identifier>
    // canonical    = <parent>/<CANONICAL_DEV_IDENTIFIER>/agents
    let app_data_dir = local_agents.parent()?;
    let data_parent = app_data_dir.parent()?;
    let name = app_data_dir.file_name()?.to_str()?;

    if !is_dev_data_dir_name(name) {
        return None;
    }

    Some(data_parent.join(CANONICAL_DEV_IDENTIFIER).join("agents"))
}

// ── Advisory lock ────────────────────────────────────────────────────────────

/// RAII guard for the interprocess advisory lock.
/// Unix: `flock(2)`.  Windows: named mutex.  Other: no-op.
pub struct JournalLockGuard {
    #[cfg(unix)]
    #[allow(dead_code)]
    file: std::fs::File,
    #[cfg(windows)]
    mutex_handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(not(any(unix, windows)))]
    _phantom: (),
}

impl JournalLockGuard {
    /// Acquire the exclusive advisory lock for `anchor_dir`, blocking until
    /// the lock is available.
    pub fn acquire(anchor_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(anchor_dir)
            .map_err(|e| format!("create anchor dir {}: {e}", anchor_dir.display()))?;
        let lock_path = anchor_dir.join(ADVISORY_LOCK_FILENAME);

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .map_err(|e| format!("open journal lock {}: {e}", lock_path.display()))?;
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(format!("journal flock {}: {err}", lock_path.display()));
            }
            Ok(JournalLockGuard { file })
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            // Derive a unique mutex name from the lock file path.
            let path_str = lock_path
                .to_str()
                .unwrap_or("buzz-store-journal")
                .replace(['\\', '/', ':'], "-");
            let name: Vec<u16> = std::ffi::OsStr::new(&format!("Global\\{path_str}"))
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let handle = unsafe {
                windows_sys::Win32::System::Threading::CreateMutexW(
                    std::ptr::null_mut(),
                    0,
                    name.as_ptr(),
                )
            };
            if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return Err(format!("CreateMutexW failed for journal lock"));
            }
            let wait = unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    handle,
                    windows_sys::Win32::System::Threading::INFINITE,
                )
            };
            if wait != 0 {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
                return Err(format!("WaitForSingleObject failed: {wait}"));
            }
            return Ok(JournalLockGuard {
                mutex_handle: handle,
            });
        }

        #[cfg(not(any(unix, windows)))]
        {
            Ok(JournalLockGuard { _phantom: () })
        }
    }
}

#[cfg(windows)]
impl Drop for JournalLockGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.mutex_handle);
            windows_sys::Win32::Foundation::CloseHandle(self.mutex_handle);
        }
    }
}

// ── Journal database ─────────────────────────────────────────────────────────

/// Open (or create) `store-journal.sqlite` at `anchor_dir` (WAL mode, 5 s
/// busy timeout) and apply the schema idempotently.
pub fn open_journal(anchor_dir: &Path) -> Result<Connection, String> {
    std::fs::create_dir_all(anchor_dir).map_err(|e| format!("create anchor dir: {e}"))?;
    let path = anchor_dir.join(JOURNAL_FILENAME);
    let conn = Connection::open(&path).map_err(|e| format!("open store-journal.sqlite: {e}"))?;

    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|e| format!("set busy_timeout: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("set WAL mode: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("enable foreign_keys: {e}"))?;

    apply_journal_schema(&conn)?;
    Ok(conn)
}

/// Apply all journal schema migrations idempotently.
/// Exposed as `apply_journal_schema_pub` for tests.
#[cfg(test)]
pub fn apply_journal_schema_pub(conn: &Connection) -> Result<(), String> {
    apply_journal_schema(conn)
}

fn apply_journal_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        -- Per-key generation / tombstone metadata.
        -- generation stored as TEXT to preserve full u64 range.
        -- is_tombstone=1: key deleted; generation kept forever (no GC).
        CREATE TABLE IF NOT EXISTS key_generations (
            key_id    TEXT NOT NULL PRIMARY KEY,
            generation TEXT NOT NULL,
            is_tombstone INTEGER NOT NULL DEFAULT 0,
            updated_at  INTEGER NOT NULL DEFAULT 0
        );

        -- Saga spine.  disposition: pending|committed|compensating|
        -- compensated|failed|uncertain|accepted.
        -- compensation_id/generation: phased claim fence (v10/v12).
        -- nonterminal_follow_up: 1 if uncertain/accepted needs a recheck.
        CREATE TABLE IF NOT EXISTS operations (
            operation_id TEXT NOT NULL PRIMARY KEY,
            kind         TEXT NOT NULL,
            key_id       TEXT NOT NULL,
            disposition  TEXT NOT NULL DEFAULT 'pending',
            generation   TEXT NOT NULL,
            compensation_id         TEXT,
            compensation_generation TEXT,
            nonterminal_follow_up   INTEGER NOT NULL DEFAULT 0,
            created_at   INTEGER NOT NULL,
            updated_at   INTEGER NOT NULL
        );

        -- Immutable outbox rows (written once; publication progress tracked
        -- via append-only operation/event phase transitions, not a mutable
        -- flag).
        -- published_state: 0=pending, 1=published, 2=uncertain, 3=accepted.
        -- Advancing published_state is a fenced CAS; see mark_outbox_published.
        CREATE TABLE IF NOT EXISTS outbox_events (
            event_id     TEXT NOT NULL PRIMARY KEY,
            operation_id TEXT NOT NULL
                REFERENCES operations(operation_id),
            payload      BLOB NOT NULL,
            published_state INTEGER NOT NULL DEFAULT 0,
            created_at   INTEGER NOT NULL
        );

        -- Immutable inbox rows (written once, never updated).
        CREATE TABLE IF NOT EXISTS inbox_events (
            event_id     TEXT NOT NULL PRIMARY KEY,
            operation_id TEXT NOT NULL
                REFERENCES operations(operation_id),
            payload      BLOB NOT NULL,
            received_at  INTEGER NOT NULL
        );
        ",
    )
    .map_err(|e| format!("apply journal schema: {e}"))?;

    // Set schema version via PRAGMA user_version (authoritative singleton,
    // never duplicated).
    conn.pragma_update(None, "user_version", 1)
        .map_err(|e| format!("set schema user_version: {e}"))?;

    Ok(())
}

// ── Generation CAS ────────────────────────────────────────────────────────────

/// Generation counter (u64 stored as TEXT to avoid SQLite's i64 ceiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

impl Generation {
    pub fn zero() -> Self {
        Generation(0)
    }
    pub fn next(self) -> Result<Self, String> {
        self.0
            .checked_add(1)
            .map(Generation)
            .ok_or_else(|| format!("generation overflow at {}: CAS cannot advance", self.0))
    }
    fn from_str(s: &str) -> Result<Self, String> {
        s.parse::<u64>()
            .map(Generation)
            .map_err(|e| format!("parse generation '{s}': {e}"))
    }
    fn to_db_str(self) -> String {
        self.0.to_string()
    }
}

/// Outcome of a generation CAS attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    /// CAS succeeded; `new_generation` is the committed value.
    Committed { new_generation: Generation },
    /// The stored generation did not match `expected`; the current value
    /// is returned so the caller can decide how to proceed.
    Conflict { current: Generation },
    /// The key has a tombstone; ABA rejected.
    Tombstoned { tombstone_generation: Generation },
}

/// Read the current generation for `key_id`.
/// Returns `(Generation::zero(), false)` when no row exists.
pub fn read_generation(conn: &Connection, key_id: &str) -> Result<(Generation, bool), String> {
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT generation, is_tombstone FROM key_generations WHERE key_id = ?1",
            params![key_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("read_generation({key_id}): {e}"))?;

    match row {
        None => Ok((Generation::zero(), false)),
        Some((gen_str, is_tombstone)) => Ok((Generation::from_str(&gen_str)?, is_tombstone)),
    }
}

/// Attempt a generation CAS on `key_id`.
///
/// If `expected` matches the stored generation (or key is absent and
/// `expected` is zero), advance to `expected.next()` and return
/// `CasOutcome::Committed`.  Tombstoned keys return `CasOutcome::Tombstoned`.
pub fn cas_generation(
    conn: &Connection,
    key_id: &str,
    expected: Generation,
) -> Result<CasOutcome, String> {
    let (current, is_tombstone) = read_generation(conn, key_id)?;

    if is_tombstone {
        return Ok(CasOutcome::Tombstoned {
            tombstone_generation: current,
        });
    }

    if current != expected {
        return Ok(CasOutcome::Conflict { current });
    }

    let new_gen = expected.next()?;
    let now = unix_now_secs();
    conn.execute(
        "INSERT INTO key_generations (key_id, generation, is_tombstone, updated_at)
         VALUES (?1, ?2, 0, ?3)
         ON CONFLICT(key_id) DO UPDATE SET
             generation  = excluded.generation,
             is_tombstone = 0,
             updated_at  = excluded.updated_at",
        params![key_id, new_gen.to_db_str(), now],
    )
    .map_err(|e| format!("cas_generation({key_id}): {e}"))?;

    Ok(CasOutcome::Committed {
        new_generation: new_gen,
    })
}

/// Write a tombstone for `key_id` at `expected` generation.
/// Kept forever; `cas_generation` respects it to prevent ABA.
pub fn tombstone_key(
    conn: &Connection,
    key_id: &str,
    expected: Generation,
) -> Result<CasOutcome, String> {
    let (current, is_tombstone) = read_generation(conn, key_id)?;

    if is_tombstone {
        return Ok(CasOutcome::Tombstoned {
            tombstone_generation: current,
        });
    }

    if current != expected {
        return Ok(CasOutcome::Conflict { current });
    }

    let tombstone_gen = expected.next()?;
    let now = unix_now_secs();
    conn.execute(
        "INSERT INTO key_generations (key_id, generation, is_tombstone, updated_at)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(key_id) DO UPDATE SET
             generation   = excluded.generation,
             is_tombstone = 1,
             updated_at   = excluded.updated_at",
        params![key_id, tombstone_gen.to_db_str(), now],
    )
    .map_err(|e| format!("tombstone_key({key_id}): {e}"))?;

    Ok(CasOutcome::Committed {
        new_generation: tombstone_gen,
    })
}

// ── Operations (saga spine) ───────────────────────────────────────────────────

/// Operation disposition values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Pending,
    Committed,
    Compensating,
    Compensated,
    Failed,
    /// Published to relay but outcome unknown (network outage).
    Uncertain,
    /// Accepted by relay; final state reached without full confirmation.
    Accepted,
}

impl Disposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Pending => "pending",
            Disposition::Committed => "committed",
            Disposition::Compensating => "compensating",
            Disposition::Compensated => "compensated",
            Disposition::Failed => "failed",
            Disposition::Uncertain => "uncertain",
            Disposition::Accepted => "accepted",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Disposition::Pending),
            "committed" => Some(Disposition::Committed),
            "compensating" => Some(Disposition::Compensating),
            "compensated" => Some(Disposition::Compensated),
            "failed" => Some(Disposition::Failed),
            "uncertain" => Some(Disposition::Uncertain),
            "accepted" => Some(Disposition::Accepted),
            _ => None,
        }
    }

    /// True when the operation is in a terminal state requiring no further
    /// progression.
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Disposition::Committed | Disposition::Compensated | Disposition::Failed
        )
    }

    /// True when the operation may require a nonterminal follow-up check
    /// (uncertain or accepted publication outcome).
    #[allow(dead_code)]
    pub fn requires_follow_up(&self) -> bool {
        matches!(self, Disposition::Uncertain | Disposition::Accepted)
    }
}

/// A record from the `operations` table.
#[derive(Debug, Clone)]
pub struct OperationRecord {
    pub operation_id: String,
    pub kind: String,
    pub key_id: String,
    pub disposition: Disposition,
    /// Current committed generation at operation record time.
    #[allow(dead_code)]
    pub generation: Generation,
    /// UUID of the active compensation event, if in `Compensating` state.
    #[allow(dead_code)]
    pub compensation_id: Option<String>,
    /// Generation snapshot at compensation start.
    #[allow(dead_code)]
    pub compensation_generation: Option<Generation>,
    /// Whether a nonterminal follow-up is pending (uncertain/accepted publication).
    #[allow(dead_code)]
    pub nonterminal_follow_up: bool,
}

/// Insert a new `pending` operation record.
pub fn insert_operation(
    conn: &Connection,
    operation_id: &str,
    kind: &str,
    key_id: &str,
    generation: Generation,
) -> Result<(), String> {
    let now = unix_now_secs();
    conn.execute(
        "INSERT INTO operations
             (operation_id, kind, key_id, disposition, generation, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?5)",
        params![operation_id, kind, key_id, generation.to_db_str(), now],
    )
    .map_err(|e| format!("insert_operation({operation_id}): {e}"))?;
    Ok(())
}

/// Read one operation record.  Returns `None` when not found.
#[allow(clippy::type_complexity)]
pub fn read_operation(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<OperationRecord>, String> {
    let row: Option<(
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        bool,
    )> = conn
        .query_row(
            "SELECT operation_id, kind, key_id, disposition, generation,
                    compensation_id, compensation_generation, nonterminal_follow_up
             FROM operations WHERE operation_id = ?1",
            params![operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("read_operation({operation_id}): {e}"))?;

    row.map(
        |(op_id, kind, key_id, disp_str, gen_str, comp_id, comp_gen_str, nf)| {
            let disposition = Disposition::from_str(&disp_str)
                .ok_or_else(|| format!("unknown disposition '{disp_str}'"))?;
            let generation = Generation::from_str(&gen_str)?;
            let compensation_generation =
                comp_gen_str.map(|s| Generation::from_str(&s)).transpose()?;
            Ok(OperationRecord {
                operation_id: op_id,
                kind,
                key_id,
                disposition,
                generation,
                compensation_id: comp_id,
                compensation_generation,
                nonterminal_follow_up: nf,
            })
        },
    )
    .transpose()
}

/// Outcome of a fenced disposition transition.
#[derive(Debug, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// Transition succeeded; operation is now in `new_disposition`.
    Advanced,
    /// The stored disposition did not match `expected_disposition`; the
    /// operation is in `actual_disposition`.
    Conflict { actual_disposition: String },
    /// The operation was not found.
    NotFound,
}

/// Advance an operation's disposition via SQL CAS on (operation_id,
/// expected_disposition).  Requires exactly one affected row and reports
/// `Conflict` or `NotFound` distinctly — never a silent no-op.
pub fn advance_disposition(
    conn: &Connection,
    operation_id: &str,
    expected_disposition: &Disposition,
    new_disposition: &Disposition,
) -> Result<TransitionOutcome, String> {
    let now = unix_now_secs();
    let affected = conn
        .execute(
            "UPDATE operations SET disposition = ?1, updated_at = ?2
             WHERE operation_id = ?3 AND disposition = ?4",
            params![
                new_disposition.as_str(),
                now,
                operation_id,
                expected_disposition.as_str(),
            ],
        )
        .map_err(|e| format!("advance_disposition({operation_id}): {e}"))?;

    if affected == 1 {
        return Ok(TransitionOutcome::Advanced);
    }

    // Distinguish NotFound from Conflict by reading the current row.
    match read_operation(conn, operation_id)? {
        None => Ok(TransitionOutcome::NotFound),
        Some(op) => Ok(TransitionOutcome::Conflict {
            actual_disposition: op.disposition.as_str().to_owned(),
        }),
    }
}

/// Pin the active compensation event for an operation (v10 claim fence).
/// Only allowed when disposition is exactly `Pending` — transitions to
/// `Compensating`.  Requires exactly one affected row.
#[allow(dead_code)]
pub fn pin_compensation(
    conn: &Connection,
    operation_id: &str,
    compensation_id: &str,
    compensation_generation: Generation,
) -> Result<TransitionOutcome, String> {
    let now = unix_now_secs();
    let affected = conn
        .execute(
            "UPDATE operations
             SET disposition            = 'compensating',
                 compensation_id        = ?1,
                 compensation_generation = ?2,
                 updated_at             = ?3
             WHERE operation_id = ?4 AND disposition = 'pending'",
            params![
                compensation_id,
                compensation_generation.to_db_str(),
                now,
                operation_id,
            ],
        )
        .map_err(|e| format!("pin_compensation({operation_id}): {e}"))?;

    if affected == 1 {
        return Ok(TransitionOutcome::Advanced);
    }

    match read_operation(conn, operation_id)? {
        None => Ok(TransitionOutcome::NotFound),
        Some(op) => Ok(TransitionOutcome::Conflict {
            actual_disposition: op.disposition.as_str().to_owned(),
        }),
    }
}

/// Mark that a nonterminal follow-up is required (uncertain/accepted
/// publication outcome — v12).  CAS on expected disposition.
#[allow(dead_code)]
pub fn set_nonterminal_follow_up(
    conn: &Connection,
    operation_id: &str,
    expected_disposition: &Disposition,
    required: bool,
) -> Result<TransitionOutcome, String> {
    let now = unix_now_secs();
    let affected = conn
        .execute(
            "UPDATE operations SET nonterminal_follow_up = ?1, updated_at = ?2
             WHERE operation_id = ?3 AND disposition = ?4",
            params![
                required as i64,
                now,
                operation_id,
                expected_disposition.as_str(),
            ],
        )
        .map_err(|e| format!("set_nonterminal_follow_up({operation_id}): {e}"))?;

    if affected == 1 {
        return Ok(TransitionOutcome::Advanced);
    }

    match read_operation(conn, operation_id)? {
        None => Ok(TransitionOutcome::NotFound),
        Some(op) => Ok(TransitionOutcome::Conflict {
            actual_disposition: op.disposition.as_str().to_owned(),
        }),
    }
}

/// Read all non-terminal operations for recovery.
pub fn read_nonterminal_operations(conn: &Connection) -> Result<Vec<OperationRecord>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, kind, key_id, disposition, generation,
                    compensation_id, compensation_generation, nonterminal_follow_up
             FROM operations
             WHERE disposition NOT IN ('committed', 'compensated', 'failed')",
        )
        .map_err(|e| format!("prepare nonterminal ops: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .map_err(|e| format!("query nonterminal ops: {e}"))?;

    let mut out = Vec::new();
    for row in rows {
        let (op_id, kind, key_id, disp_str, gen_str, comp_id, comp_gen_str, nf) =
            row.map_err(|e| format!("read nonterminal op row: {e}"))?;
        let disposition = Disposition::from_str(&disp_str)
            .ok_or_else(|| format!("unknown disposition '{disp_str}'"))?;
        let generation = Generation::from_str(&gen_str)?;
        let compensation_generation = comp_gen_str.map(|s| Generation::from_str(&s)).transpose()?;
        out.push(OperationRecord {
            operation_id: op_id,
            kind,
            key_id,
            disposition,
            generation,
            compensation_id: comp_id,
            compensation_generation,
            nonterminal_follow_up: nf,
        });
    }
    Ok(out)
}

// ── Immutable inbox / outbox rows ─────────────────────────────────────────────

/// Outcome of an immutable-row insertion.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertEventOutcome {
    /// New row inserted.
    Inserted,
    /// A row with this `event_id` already exists and is byte-identical
    /// (safe exact-replay idempotency).
    ExactDuplicate,
    /// A row with this `event_id` already exists but has different
    /// `operation_id` or `payload` — identity collision, fail closed.
    IdentityCollision,
}

/// Insert an immutable outbox row.  Fail-closed on identity collision:
/// a duplicate event_id is only accepted when operation_id and payload
/// are byte-identical.
pub fn insert_outbox_event(
    conn: &Connection,
    event_id: &str,
    operation_id: &str,
    payload: &[u8],
) -> Result<InsertEventOutcome, String> {
    let now = unix_now_secs();
    // Try an unconditional insert first.
    let affected = conn
        .execute(
            "INSERT OR IGNORE INTO outbox_events
                 (event_id, operation_id, payload, published_state, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![event_id, operation_id, payload, now],
        )
        .map_err(|e| format!("insert_outbox_event({event_id}): {e}"))?;

    if affected == 1 {
        return Ok(InsertEventOutcome::Inserted);
    }

    // Row already exists — check identity.
    let existing: Option<(String, Vec<u8>)> = conn
        .query_row(
            "SELECT operation_id, payload FROM outbox_events WHERE event_id = ?1",
            params![event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("read existing outbox_event({event_id}): {e}"))?;

    match existing {
        Some((op, pl)) if op == operation_id && pl == payload => {
            Ok(InsertEventOutcome::ExactDuplicate)
        }
        _ => Ok(InsertEventOutcome::IdentityCollision),
    }
}

/// Advance publication state of an outbox event via CAS on
/// `expected_state → new_state`.  Requires exactly one affected row.
#[allow(dead_code)]
pub fn mark_outbox_published(
    conn: &Connection,
    event_id: &str,
    expected_state: i64,
    new_state: i64,
) -> Result<bool, String> {
    let affected = conn
        .execute(
            "UPDATE outbox_events SET published_state = ?1
             WHERE event_id = ?2 AND published_state = ?3",
            params![new_state, event_id, expected_state],
        )
        .map_err(|e| format!("mark_outbox_published({event_id}): {e}"))?;
    Ok(affected == 1)
}

/// Insert an immutable inbox row.  Fail-closed on identity collision.
#[allow(dead_code)]
pub fn insert_inbox_event(
    conn: &Connection,
    event_id: &str,
    operation_id: &str,
    payload: &[u8],
) -> Result<InsertEventOutcome, String> {
    let now = unix_now_secs();
    let affected = conn
        .execute(
            "INSERT OR IGNORE INTO inbox_events
                 (event_id, operation_id, payload, received_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![event_id, operation_id, payload, now],
        )
        .map_err(|e| format!("insert_inbox_event({event_id}): {e}"))?;

    if affected == 1 {
        return Ok(InsertEventOutcome::Inserted);
    }

    let existing: Option<(String, Vec<u8>)> = conn
        .query_row(
            "SELECT operation_id, payload FROM inbox_events WHERE event_id = ?1",
            params![event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("read existing inbox_event({event_id}): {e}"))?;

    match existing {
        Some((op, pl)) if op == operation_id && pl == payload => {
            Ok(InsertEventOutcome::ExactDuplicate)
        }
        _ => Ok(InsertEventOutcome::IdentityCollision),
    }
}

/// Read outbox events for `operation_id`.
#[allow(dead_code)]
pub fn read_outbox_events(
    conn: &Connection,
    operation_id: &str,
) -> Result<Vec<(String, Vec<u8>, i64)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT event_id, payload, published_state FROM outbox_events
             WHERE operation_id = ?1 ORDER BY created_at",
        )
        .map_err(|e| format!("prepare outbox query: {e}"))?;
    let rows = stmt
        .query_map(params![operation_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| format!("query outbox: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read outbox row: {e}"))
}

/// Read inbox events for `operation_id`.
#[allow(dead_code)]
pub fn read_inbox_events(
    conn: &Connection,
    operation_id: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT event_id, payload FROM inbox_events
             WHERE operation_id = ?1 ORDER BY received_at",
        )
        .map_err(|e| format!("prepare inbox query: {e}"))?;
    let rows = stmt
        .query_map(params![operation_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| format!("query inbox: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read inbox row: {e}"))
}

// ── Fail-closed codec (v6) ────────────────────────────────────────────────────

/// Parse error type. The raw bytes are never silently discarded — callers
/// receive this error and MUST NOT proceed with mutation.
#[derive(Debug)]
pub struct StoreDecodeError {
    pub message: String,
}

impl std::fmt::Display for StoreDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Decode `managed-agents.json` bytes using the fail-closed codec.
///
/// Returns `Ok(records)` or `Err(StoreDecodeError)`.
/// On error the raw bytes are preserved in place (see `backup_invalid_store`).
/// The caller MUST NOT proceed with any mutation when this returns `Err`.
pub fn decode_agent_store(bytes: &[u8]) -> Result<Vec<ManagedAgentRecord>, StoreDecodeError> {
    // Fail-closed: unknown/malformed content ⇒ error, zero mutation.
    serde_json::from_slice(bytes).map_err(|e| StoreDecodeError {
        message: format!("agent store decode failed: {e}"),
    })
}

/// Decode `teams.json` bytes using the fail-closed codec.
pub fn decode_team_store(bytes: &[u8]) -> Result<Vec<TeamRecord>, StoreDecodeError> {
    serde_json::from_slice(bytes).map_err(|e| StoreDecodeError {
        message: format!("team store decode failed: {e}"),
    })
}

/// Leniently decode a single `ManagedAgentRecord` from a `serde_json::Value`
/// that may contain unknown fields from a future schema version.
///
/// Intended for **read-only** migration helpers that compute hashes or
/// inspect existing store records — not for decoding user-provided or
/// externally-sourced data (use `decode_agent_store` for those paths).
///
/// Repeatedly strips unrecognized fields (identified from the serde error
/// message) and retries until decode succeeds or we've exhausted 32 attempts.
/// Returns `None` when the record is structurally invalid (missing required
/// fields, wrong types) after stripping.
pub fn decode_agent_record_permissive(mut v: serde_json::Value) -> Option<ManagedAgentRecord> {
    for _ in 0..32 {
        match serde_json::from_value::<ManagedAgentRecord>(v.clone()) {
            Ok(r) => return Some(r),
            Err(e) => {
                // serde_json formats unknown-field errors as
                // `unknown field `<NAME>`, expected ...`
                let msg = e.to_string();
                if let Some(name) = msg
                    .strip_prefix("unknown field `")
                    .and_then(|s| s.split('`').next())
                {
                    let field = name.to_string();
                    if let Some(obj) = v.as_object_mut() {
                        obj.remove(&field);
                    } else {
                        return None;
                    }
                } else {
                    // Not an unknown-field error (missing required field, type
                    // mismatch, etc.) — cannot recover.
                    return None;
                }
            }
        }
    }
    None
}

// ── Atomic write (with fsync) ─────────────────────────────────────────────────

/// Write `payload` to `path` atomically (tmp → fsync → rename → fsync-parent).
/// Resolves symlinks so the rename lands on the physical target.
///
/// On Unix, fsyncs the parent directory after rename to durably commit the
/// directory entry — without it the data blocks may survive a crash while the
/// renamed entry does not.  This matches the durability guarantee provided by
/// `atomic-write-file::commit()` on the restricted-write path.
pub fn atomic_write_with_fsync(path: &Path, payload: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = resolved.with_extension("json.tmp");
    let mut file =
        std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    file.write_all(payload)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &resolved)
        .map_err(|e| format!("rename {} → {}: {e}", tmp.display(), resolved.display()))?;

    // Fsync the parent directory so the directory entry for the new name is
    // durably committed.  Best-effort on platforms that do not support it.
    #[cfg(unix)]
    if let Some(parent) = resolved.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// Atomic write (tmp → fsync → rename) with `0o600` permissions. Used for
/// `managed-agents.json`, which may carry plaintext agent nsecs.
pub fn atomic_write_restricted_with_fsync(path: &Path, payload: &[u8]) -> Result<(), String> {
    use atomic_write_file::AtomicWriteFile;
    use std::io::Write;

    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut file = AtomicWriteFile::open(&resolved)
        .map_err(|e| format!("open {} for atomic write: {e}", resolved.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set {} permissions: {e}", resolved.display()))?;
    }

    file.write_all(payload)
        .map_err(|e| format!("write {}: {e}", resolved.display()))?;

    // AtomicWriteFile::commit() handles the rename; sync the temp fd first.
    // The `sync_before_close` feature isn't exposed, so we flush explicitly.
    file.flush()
        .map_err(|e| format!("flush {}: {e}", resolved.display()))?;
    file.commit()
        .map_err(|e| format!("commit {}: {e}", resolved.display()))
}

// ── Closure-only mutation API (v7) ────────────────────────────────────────────

/// The decoded state passed to a mutation or read closure.
pub struct StoreState<'a> {
    /// Agent records decoded from `managed-agents.json` (all records —
    /// instances with a pubkey AND key-less definitions).
    pub agents: Vec<ManagedAgentRecord>,
    /// Team records decoded from `teams.json`.
    pub teams: Vec<TeamRecord>,
    /// Open journal connection (anchor-locked).
    pub journal: &'a Connection,
    /// Anchor directory (for constructing file paths).
    #[allow(dead_code)]
    pub anchor: &'a Path,
}

/// Mutate the store under the full lock sequence:
///
///   in-process mutex (caller-held `store_mutex_guard`)
///   → anchored OS advisory lock (acquired here)
///   → fresh fail-closed decode
///   → SQLite journal transaction (inside `mutation`)
///   → atomic fsync write of both canonical JSON files
///   → release OS advisory lock
///
/// The `store_mutex_guard` is **returned** so the caller can perform
/// post-write work (e.g. `retain_managed_agent_pending`, which must run
/// while the in-process mutex is still held) before dropping it.
///
/// Network I/O and keyring access must NOT occur inside `mutation`.  They
/// belong outside the critical section, driven by durable operation phases
/// that `mutation` records in the journal.
///
/// On any decode error inside this function no file is written, no journal
/// transition occurs, and the error is propagated with `?`.
pub fn mutate_store<'g, F, T>(
    app: &AppHandle,
    store_mutex_guard: MutexGuard<'g, ()>,
    mutation: F,
) -> Result<(T, MutexGuard<'g, ()>), String>
where
    F: FnOnce(StoreState<'_>) -> Result<(Vec<ManagedAgentRecord>, Vec<TeamRecord>, T), String>,
{
    let anchor = store_anchor_dir(app)?;
    std::fs::create_dir_all(&anchor).map_err(|e| format!("create anchor dir: {e}"))?;

    // Acquire advisory lock while holding the in-process mutex.
    let _advisory = JournalLockGuard::acquire(&anchor)?;

    let agents_path = anchor.join("managed-agents.json");
    let teams_path = anchor.join("teams.json");

    // Fresh fail-closed decode — any parse error is propagated; no mutation.
    let agents: Vec<ManagedAgentRecord> = if agents_path.exists() {
        let bytes =
            std::fs::read(&agents_path).map_err(|e| format!("read managed-agents.json: {e}"))?;
        decode_agent_store(&bytes).map_err(|e| {
            crate::managed_agents::storage::backup_invalid_store(&agents_path);
            e.message
        })?
    } else {
        Vec::new()
    };

    let teams: Vec<TeamRecord> = if teams_path.exists() {
        let bytes = std::fs::read(&teams_path).map_err(|e| format!("read teams.json: {e}"))?;
        decode_team_store(&bytes).map_err(|e| {
            crate::managed_agents::storage::backup_invalid_store(&teams_path);
            e.message
        })?
    } else {
        Vec::new()
    };

    let journal = open_journal(&anchor)?;

    let state = StoreState {
        agents,
        teams,
        journal: &journal,
        anchor: &anchor,
    };

    let (new_agents, new_teams, result) = mutation(state)?;

    // Write back both files atomically with fsync.
    let agents_payload = serde_json::to_vec_pretty(&new_agents)
        .map_err(|e| format!("serialize managed-agents.json: {e}"))?;
    atomic_write_restricted_with_fsync(&agents_path, &agents_payload)?;

    let teams_payload =
        serde_json::to_vec_pretty(&new_teams).map_err(|e| format!("serialize teams.json: {e}"))?;
    atomic_write_with_fsync(&teams_path, &teams_payload)?;

    // Advisory lock drops here; in-process mutex guard returned to caller so
    // post-write work (retain, tombstone) can run inside the in-process lock.
    Ok((result, store_mutex_guard))
}

/// Read the store under the full lock sequence without writing back files.
///
/// The `store_mutex_guard` is returned so callers can continue holding the
/// in-process lock after the read.
#[allow(dead_code)]
pub fn read_store<'g, F, T>(
    app: &AppHandle,
    store_mutex_guard: MutexGuard<'g, ()>,
    reader: F,
) -> Result<(T, MutexGuard<'g, ()>), String>
where
    F: FnOnce(StoreState<'_>) -> Result<T, String>,
{
    let anchor = store_anchor_dir(app)?;
    std::fs::create_dir_all(&anchor).map_err(|e| format!("create anchor dir: {e}"))?;
    let _advisory = JournalLockGuard::acquire(&anchor)?;

    let agents_path = anchor.join("managed-agents.json");
    let teams_path = anchor.join("teams.json");

    let agents: Vec<ManagedAgentRecord> = if agents_path.exists() {
        let bytes =
            std::fs::read(&agents_path).map_err(|e| format!("read managed-agents.json: {e}"))?;
        decode_agent_store(&bytes).map_err(|e| {
            crate::managed_agents::storage::backup_invalid_store(&agents_path);
            e.message
        })?
    } else {
        Vec::new()
    };

    let teams: Vec<TeamRecord> = if teams_path.exists() {
        let bytes = std::fs::read(&teams_path).map_err(|e| format!("read teams.json: {e}"))?;
        decode_team_store(&bytes).map_err(|e| {
            crate::managed_agents::storage::backup_invalid_store(&teams_path);
            e.message
        })?
    } else {
        Vec::new()
    };

    let journal = open_journal(&anchor)?;

    let state = StoreState {
        agents,
        teams,
        journal: &journal,
        anchor: &anchor,
    };

    let result = reader(state)?;
    Ok((result, store_mutex_guard))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Generate a new random UUID v4 operation ID.
pub fn new_operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Boot recovery: open the journal and log any nonterminal operations.
///
/// Nonterminal operations (pending, compensating, uncertain, accepted) may
/// represent interrupted publishes or partial mutations from a previous crash.
/// In the current B1 substrate, recovery logs the operations for observability
/// and marks them as `failed` so they do not accumulate indefinitely.
///
/// A fuller recovery (re-drive pending retentions) is B2 scope.  The goal here
/// is: (a) `store-journal.sqlite` is opened at boot so it actually exists after
/// a normal app launch, (b) stale nonterminal ops are surfaced/cleared.
pub fn run_boot_recovery(app: &AppHandle) -> Result<(), String> {
    let anchor = store_anchor_dir(app)?;
    std::fs::create_dir_all(&anchor).map_err(|e| format!("boot-recovery create anchor: {e}"))?;
    let journal = open_journal(&anchor)?;

    let nonterminal = read_nonterminal_operations(&journal)?;
    if nonterminal.is_empty() {
        return Ok(());
    }

    for op in &nonterminal {
        eprintln!(
            "buzz-desktop: boot-recovery: nonterminal operation {} kind={} key={} disposition={:?}",
            op.operation_id, op.kind, op.key_id, op.disposition
        );
        // Mark as failed so the next boot does not re-surface it.
        // A B2 recovery driver would re-try or compensate instead.
        let _ = advance_disposition(
            &journal,
            &op.operation_id,
            &op.disposition,
            &Disposition::Failed,
        );
    }

    eprintln!(
        "buzz-desktop: boot-recovery: {} nonterminal operation(s) found and marked failed",
        nonterminal.len()
    );
    Ok(())
}

#[cfg(test)]
#[path = "store_journal_tests.rs"]
mod tests;

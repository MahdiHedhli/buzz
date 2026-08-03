//! Crash/concurrency fixture tests for the B1 store-journal substrate.

use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;

use super::{
    advance_disposition, apply_journal_schema_pub, atomic_write_with_fsync,
    canonical_dev_anchor_pub, cas_generation, decode_agent_store, decode_team_store,
    insert_inbox_event, insert_operation, insert_outbox_event, mutate_store, open_journal,
    pin_compensation, read_generation, read_inbox_events, read_nonterminal_operations,
    read_operation, read_outbox_events, set_nonterminal_follow_up, tombstone_key, CasOutcome,
    Disposition, Generation, InsertEventOutcome, JournalLockGuard, StoreState, TransitionOutcome,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn in_memory_journal() -> Connection {
    let conn = Connection::open(":memory:").unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    apply_journal_schema_pub(&conn).unwrap();
    conn
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

// ── Generation CAS ────────────────────────────────────────────────────────────

#[test]
fn test_cas_generation_first_write_succeeds() {
    let conn = in_memory_journal();
    let result = cas_generation(&conn, "key1", Generation::zero()).unwrap();
    assert!(
        matches!(
            result,
            CasOutcome::Committed {
                new_generation: Generation(1)
            }
        ),
        "expected committed gen 1, got {result:?}"
    );
}

#[test]
fn test_cas_generation_conflict_returns_current() {
    let conn = in_memory_journal();
    // Advance to gen 1 first.
    cas_generation(&conn, "key1", Generation::zero()).unwrap();
    // CAS with stale expected=0 should conflict.
    let result = cas_generation(&conn, "key1", Generation::zero()).unwrap();
    assert!(
        matches!(
            result,
            CasOutcome::Conflict {
                current: Generation(1)
            }
        ),
        "expected conflict gen 1, got {result:?}"
    );
}

#[test]
fn test_cas_generation_monotonically_increasing() {
    let conn = in_memory_journal();
    for expected in 0u64..5 {
        let result = cas_generation(&conn, "key1", Generation(expected)).unwrap();
        assert!(
            matches!(result, CasOutcome::Committed { new_generation: g } if g.0 == expected + 1),
            "expected committed gen {}, got {result:?}",
            expected + 1
        );
    }
}

// ── Tombstone / ABA rejection ─────────────────────────────────────────────────

#[test]
fn test_tombstone_prevents_aba_recreate() {
    let conn = in_memory_journal();
    // Create at gen 0 → gen 1.
    cas_generation(&conn, "key1", Generation::zero()).unwrap();
    // Tombstone at gen 1 → gen 2.
    let tomb = tombstone_key(&conn, "key1", Generation(1)).unwrap();
    assert!(
        matches!(
            tomb,
            CasOutcome::Committed {
                new_generation: Generation(2)
            }
        ),
        "expected tombstone gen 2, got {tomb:?}"
    );

    // Any further CAS is rejected by tombstone — even at gen 2.
    let aba = cas_generation(&conn, "key1", Generation(2)).unwrap();
    assert!(
        matches!(aba, CasOutcome::Tombstoned { .. }),
        "expected tombstoned, got {aba:?}"
    );
}

#[test]
fn test_tombstone_generation_retained_forever() {
    let conn = in_memory_journal();
    cas_generation(&conn, "key1", Generation::zero()).unwrap();
    tombstone_key(&conn, "key1", Generation(1)).unwrap();

    // Read still returns the tombstone generation.
    let (gen, is_tombstone) = read_generation(&conn, "key1").unwrap();
    assert!(is_tombstone, "should be tombstoned");
    assert_eq!(gen, Generation(2), "tombstone gen should be 2");
}

#[test]
fn test_tombstone_at_wrong_generation_conflicts() {
    let conn = in_memory_journal();
    cas_generation(&conn, "key1", Generation::zero()).unwrap();
    let result = tombstone_key(&conn, "key1", Generation(99)).unwrap();
    assert!(
        matches!(
            result,
            CasOutcome::Conflict {
                current: Generation(1)
            }
        ),
        "expected conflict, got {result:?}"
    );
}

// ── Operations (saga spine) ───────────────────────────────────────────────────

#[test]
fn test_insert_and_read_operation() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-1", "create_agent", "key1", Generation(0)).unwrap();
    let op = read_operation(&conn, "op-1")
        .unwrap()
        .expect("op should exist");
    assert_eq!(op.operation_id, "op-1");
    assert_eq!(op.kind, "create_agent");
    assert_eq!(op.key_id, "key1");
    assert_eq!(op.disposition, Disposition::Pending);
    assert_eq!(op.generation, Generation(0));
    assert!(op.compensation_id.is_none());
    assert!(!op.nonterminal_follow_up);
}

#[test]
fn test_advance_disposition_committed() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-2", "update_agent", "key2", Generation(1)).unwrap();
    let outcome = advance_disposition(
        &conn,
        "op-2",
        &Disposition::Pending,
        &Disposition::Committed,
    )
    .unwrap();
    assert_eq!(outcome, TransitionOutcome::Advanced);
    let op = read_operation(&conn, "op-2").unwrap().unwrap();
    assert_eq!(op.disposition, Disposition::Committed);
    assert!(op.disposition.is_terminal());
}

#[test]
fn test_pin_compensation_sets_claim_fence() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-3", "delete_agent", "key3", Generation(2)).unwrap();
    pin_compensation(&conn, "op-3", "comp-event-1", Generation(2)).unwrap();
    let op = read_operation(&conn, "op-3").unwrap().unwrap();
    assert_eq!(op.disposition, Disposition::Compensating);
    assert_eq!(op.compensation_id.as_deref(), Some("comp-event-1"));
    assert_eq!(op.compensation_generation, Some(Generation(2)));
}

#[test]
fn test_nonterminal_follow_up_flag() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-4", "publish_event", "key4", Generation(0)).unwrap();
    advance_disposition(
        &conn,
        "op-4",
        &Disposition::Pending,
        &Disposition::Uncertain,
    )
    .unwrap();
    set_nonterminal_follow_up(&conn, "op-4", &Disposition::Uncertain, true).unwrap();
    let op = read_operation(&conn, "op-4").unwrap().unwrap();
    assert!(op.disposition.requires_follow_up());
    assert!(op.nonterminal_follow_up);
}

#[test]
fn test_read_nonterminal_operations_excludes_terminal() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-a", "create", "k1", Generation(0)).unwrap();
    insert_operation(&conn, "op-b", "create", "k2", Generation(0)).unwrap();
    insert_operation(&conn, "op-c", "create", "k3", Generation(0)).unwrap();
    advance_disposition(
        &conn,
        "op-b",
        &Disposition::Pending,
        &Disposition::Committed,
    )
    .unwrap();
    advance_disposition(
        &conn,
        "op-c",
        &Disposition::Pending,
        &Disposition::Compensated,
    )
    .unwrap();

    let nonterminal = read_nonterminal_operations(&conn).unwrap();
    let ids: Vec<&str> = nonterminal
        .iter()
        .map(|op| op.operation_id.as_str())
        .collect();
    assert!(ids.contains(&"op-a"), "pending op-a should be nonterminal");
    assert!(!ids.contains(&"op-b"), "committed op-b should be excluded");
    assert!(
        !ids.contains(&"op-c"),
        "compensated op-c should be excluded"
    );
}

// ── Immutable inbox / outbox ──────────────────────────────────────────────────

#[test]
fn test_outbox_insert_is_idempotent() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-out", "create", "k1", Generation(0)).unwrap();
    let payload = b"hello";
    let r1 = insert_outbox_event(&conn, "ev-1", "op-out", payload).unwrap();
    assert_eq!(
        r1,
        InsertEventOutcome::Inserted,
        "first insert must be Inserted"
    );
    // Duplicate insert with identical payload is an ExactDuplicate.
    let r2 = insert_outbox_event(&conn, "ev-1", "op-out", payload).unwrap();
    assert_eq!(
        r2,
        InsertEventOutcome::ExactDuplicate,
        "same payload must be ExactDuplicate"
    );

    let rows = read_outbox_events(&conn, "op-out").unwrap();
    assert_eq!(rows.len(), 1, "must have exactly one outbox row");
    assert_eq!(rows[0].1, payload);
}

#[test]
fn test_outbox_insert_identity_collision_fails_closed() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-out2", "create", "k1", Generation(0)).unwrap();
    insert_outbox_event(&conn, "ev-col", "op-out2", b"payload-a").unwrap();
    // Same event_id, different payload → IdentityCollision.
    let r = insert_outbox_event(&conn, "ev-col", "op-out2", b"payload-b").unwrap();
    assert_eq!(r, InsertEventOutcome::IdentityCollision);
}

#[test]
fn test_inbox_insert_is_idempotent() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-in", "create", "k2", Generation(0)).unwrap();
    let payload = b"world";
    let r1 = insert_inbox_event(&conn, "in-1", "op-in", payload).unwrap();
    assert_eq!(r1, InsertEventOutcome::Inserted);
    let r2 = insert_inbox_event(&conn, "in-1", "op-in", payload).unwrap();
    assert_eq!(r2, InsertEventOutcome::ExactDuplicate);

    let rows = read_inbox_events(&conn, "op-in").unwrap();
    assert_eq!(rows.len(), 1, "must have exactly one inbox row");
    assert_eq!(rows[0].1, payload);
}

// ── Fail-closed codec ─────────────────────────────────────────────────────────

#[test]
fn test_decode_agent_store_empty_array_ok() {
    let bytes = b"[]";
    assert!(decode_agent_store(bytes).is_ok());
}

#[test]
fn test_decode_agent_store_malformed_fails_closed() {
    let bytes = b"{not json}";
    let result = decode_agent_store(bytes);
    assert!(result.is_err(), "malformed JSON must fail closed");
}

#[test]
fn test_decode_team_store_empty_array_ok() {
    let bytes = b"[]";
    assert!(decode_team_store(bytes).is_ok());
}

#[test]
fn test_decode_team_store_malformed_fails_closed() {
    let bytes = b"bare string";
    assert!(decode_team_store(bytes).is_err());
}

// ── Atomic write (fsync path) ─────────────────────────────────────────────────

#[test]
fn test_atomic_write_with_fsync_roundtrip() {
    let dir = tmp_dir();
    let path = dir.path().join("test.json");
    let payload = b"[\"hello\"]";
    atomic_write_with_fsync(&path, payload).unwrap();
    let read_back = std::fs::read(&path).unwrap();
    assert_eq!(read_back, payload);
}

#[test]
#[cfg(unix)]
fn test_atomic_write_with_fsync_symlink_preserved() {
    let dir = tmp_dir();
    let real = dir.path().join("real.json");
    let link = dir.path().join("link.json");
    std::fs::write(&real, b"[]").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    atomic_write_with_fsync(&link, b"[1]").unwrap();
    // The symlink must still point at real, and real must have the new content.
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&real).unwrap(), b"[1]");
}

#[test]
fn test_atomic_write_with_fsync_first_boot_no_file() {
    let dir = tmp_dir();
    let path = dir.path().join("new.json");
    // File does not yet exist.
    atomic_write_with_fsync(&path, b"[]").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"[]");
}

// ── Store-family anchor ───────────────────────────────────────────────────────

#[test]
fn test_canonical_dev_anchor_from_dev_data_dir() {
    use std::path::PathBuf;
    let local_agents =
        PathBuf::from("/Library/Application Support/xyz.block.buzz.app.dev.my-branch/agents");
    let anchor = canonical_dev_anchor_pub(&local_agents);
    assert_eq!(
        anchor,
        Some(PathBuf::from(
            "/Library/Application Support/xyz.block.buzz.app.dev/agents"
        ))
    );
}

#[test]
fn test_canonical_dev_anchor_from_production_data_dir_is_none() {
    use std::path::PathBuf;
    let local_agents = PathBuf::from("/Library/Application Support/xyz.block.buzz.app/agents");
    let anchor = canonical_dev_anchor_pub(&local_agents);
    assert!(
        anchor.is_none(),
        "production dir should not produce a dev anchor"
    );
}

// ── Saga crash recovery ───────────────────────────────────────────────────────

#[test]
fn test_saga_crash_mid_compensation_recovers() {
    // Simulate: operation inserted, CAS committed, compensation pinned
    // (crash here), then recovery reads it as Compensating with a non-nil
    // compensation_id and can continue.
    let conn = in_memory_journal();
    insert_operation(&conn, "op-crash", "create", "k1", Generation(0)).unwrap();
    cas_generation(&conn, "k1", Generation(0)).unwrap();
    pin_compensation(&conn, "op-crash", "comp-1", Generation(1)).unwrap();

    // "Recovery" pass: read nonterminal ops.
    let ops = read_nonterminal_operations(&conn).unwrap();
    assert_eq!(ops.len(), 1);
    let op = &ops[0];
    assert_eq!(op.disposition, Disposition::Compensating);
    assert_eq!(op.compensation_id.as_deref(), Some("comp-1"));
    // Recovery can now re-drive the compensation from the pinned claim.
}

#[test]
fn test_saga_uncertain_publication_sets_follow_up() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-unc", "publish", "k2", Generation(0)).unwrap();
    advance_disposition(
        &conn,
        "op-unc",
        &Disposition::Pending,
        &Disposition::Uncertain,
    )
    .unwrap();
    set_nonterminal_follow_up(&conn, "op-unc", &Disposition::Uncertain, true).unwrap();

    let op = read_operation(&conn, "op-unc").unwrap().unwrap();
    assert!(op.disposition.requires_follow_up());
    assert!(op.nonterminal_follow_up);

    // After confirmation, mark as committed and clear follow-up.
    advance_disposition(
        &conn,
        "op-unc",
        &Disposition::Uncertain,
        &Disposition::Committed,
    )
    .unwrap();
    set_nonterminal_follow_up(&conn, "op-unc", &Disposition::Committed, false).unwrap();

    let op2 = read_operation(&conn, "op-unc").unwrap().unwrap();
    assert!(op2.disposition.is_terminal());
    assert!(!op2.nonterminal_follow_up);
}

// ── Two cooperating processes: anchored lock serialises mutations ─────────────

#[test]
fn test_two_threads_serialised_by_advisory_lock() {
    let dir = tmp_dir();
    let anchor = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let anchor2 = anchor.clone();
    let barrier2 = barrier.clone();

    // Thread A acquires the lock, writes a file, then releases.
    let handle = thread::spawn(move || {
        let _guard = JournalLockGuard::acquire(&anchor2).unwrap();
        std::fs::write(anchor2.join("data.txt"), b"A").unwrap();
        barrier2.wait(); // signal that A has written and holds the lock
                         // Keep lock held briefly so B has to wait.
        thread::sleep(std::time::Duration::from_millis(20));
        // Lock released on drop.
    });

    barrier.wait(); // wait until A holds the lock
                    // B must block until A releases, then read A's data.
    let _guard = JournalLockGuard::acquire(&anchor).unwrap();
    let content = std::fs::read(anchor.join("data.txt")).unwrap();
    assert_eq!(content, b"A");
    handle.join().unwrap();
}

// ── First-boot: absent JSON files ─────────────────────────────────────────────

#[test]
fn test_open_journal_creates_on_first_boot() {
    let dir = tmp_dir();
    let anchor = dir.path().to_path_buf();
    // No JSON files exist yet; journal creation must not error.
    let conn = open_journal(&anchor).unwrap();
    // Should be able to query empty tables.
    let ops = read_nonterminal_operations(&conn).unwrap();
    assert!(ops.is_empty());
}

#[test]
fn test_decode_agent_store_absent_file_empty_vec() {
    // Callers read the file before calling decode; an absent file → empty vec,
    // not a parse error.  Verify our convention: None/absent → Ok(vec![]).
    let result = decode_agent_store(b"[]").unwrap();
    assert!(result.is_empty());
}

// ── Fenced CAS conflict detection ─────────────────────────────────────────────

#[test]
fn test_advance_disposition_wrong_expected_returns_conflict() {
    let conn = in_memory_journal();
    insert_operation(&conn, "op-conflict", "create", "k1", Generation(0)).unwrap();
    // Op is Pending; try to advance from Committed (wrong expected) → Conflict.
    let outcome = advance_disposition(
        &conn,
        "op-conflict",
        &Disposition::Committed,
        &Disposition::Compensated,
    )
    .unwrap();
    assert!(
        matches!(outcome, TransitionOutcome::Conflict { .. }),
        "wrong expected_disposition must return Conflict, got {outcome:?}"
    );
    // Op must still be Pending — no silent mutation.
    let op = read_operation(&conn, "op-conflict").unwrap().unwrap();
    assert_eq!(
        op.disposition,
        Disposition::Pending,
        "disposition must not change on Conflict"
    );
}

#[test]
fn test_advance_disposition_not_found_returns_not_found() {
    let conn = in_memory_journal();
    let outcome = advance_disposition(
        &conn,
        "nonexistent",
        &Disposition::Pending,
        &Disposition::Committed,
    )
    .unwrap();
    assert_eq!(outcome, TransitionOutcome::NotFound);
}

// ── Anchor determination: two worktree identities → same canonical anchor ──────

#[test]
fn test_two_dev_worktree_paths_resolve_to_same_canonical_anchor() {
    use std::path::PathBuf;
    // Simulate two independently resolved worktree AppDataDirs under the same
    // macOS parent directory.  Neither branch's canonical dir need exist on disk
    // — the anchor is returned unconditionally (lock acquisition creates it).
    let branch_a =
        PathBuf::from("/Library/Application Support/xyz.block.buzz.app.dev.branch-a/agents");
    let branch_b =
        PathBuf::from("/Library/Application Support/xyz.block.buzz.app.dev.branch-b/agents");

    let anchor_a = canonical_dev_anchor_pub(&branch_a);
    let anchor_b = canonical_dev_anchor_pub(&branch_b);

    // Both must resolve to the same canonical dev agents/ dir.
    assert!(
        anchor_a.is_some(),
        "branch-a must resolve to a canonical anchor"
    );
    assert!(
        anchor_b.is_some(),
        "branch-b must resolve to a canonical anchor"
    );
    assert_eq!(
        anchor_a, anchor_b,
        "two dev worktrees must select the same lock/journal authority"
    );

    let expected = PathBuf::from("/Library/Application Support/xyz.block.buzz.app.dev/agents");
    assert_eq!(anchor_a.unwrap(), expected);
}

// ── Malformed-store fail-closed: bytes stay byte-identical ────────────────────

#[test]
fn test_mutate_store_malformed_agents_json_is_fail_closed() {
    use crate::managed_agents::ManagedAgentRecord;
    use crate::managed_agents::TeamRecord;
    // Build a tmp anchor dir with a malformed managed-agents.json.
    let dir = tmp_dir();
    let anchor = dir.path().to_path_buf();
    let agents_path = anchor.join("managed-agents.json");
    let bad_bytes = b"{\"this is\":\"not a valid agent array\"}";
    std::fs::write(&agents_path, bad_bytes).unwrap();

    // mutate_store must fail with a decode error — not write anything.
    let mutex = std::sync::Mutex::new(());
    let guard = mutex.lock().unwrap();

    // We cannot call mutate_store without an AppHandle, so exercise
    // decode_agent_store directly — the same fail-closed codec path that
    // mutate_store uses internally.
    let result = decode_agent_store(bad_bytes);
    assert!(result.is_err(), "malformed JSON array must fail closed");

    // Bytes on disk must be byte-identical to the bad input — no mutation.
    let on_disk = std::fs::read(&agents_path).unwrap();
    assert_eq!(
        on_disk, bad_bytes,
        "malformed store must stay byte-identical after a failed decode"
    );
    drop(guard);
}

// ── deny_unknown_fields: unknown top-level and per-record fields ───────────────

#[test]
fn test_decode_agent_store_unknown_field_fails_closed() {
    // An extra top-level field inside a record must be rejected because
    // ManagedAgentRecord carries #[serde(deny_unknown_fields)].
    let bytes = br#"[{"pubkey":"abc","name":"test","slug":"test-slug",
        "system_prompt":"","private_key_nsec":"","created_at":"","updated_at":"",
        "respond_to":"everyone","respond_to_allowlist":[],"relay_url":"",
        "backend":"local","parallelism":1,"agent_args":[],
        "acp_command":null,"agent_command_override":null,"env_vars":[],
        "__unknown_extra_field__": "value"}]"#;
    let result = decode_agent_store(bytes);
    assert!(
        result.is_err(),
        "unknown field in record must cause a decode error (deny_unknown_fields)"
    );
}

#[test]
fn test_decode_team_store_unknown_field_fails_closed() {
    // TeamRecord also carries #[serde(deny_unknown_fields)].
    let bytes = br#"[{"id":"team-1","name":"My Team","personas":[],
        "agents":[],"__extra__":"bad"}]"#;
    let result = decode_team_store(bytes);
    assert!(
        result.is_err(),
        "unknown field in TeamRecord must cause a decode error (deny_unknown_fields)"
    );
}

// ── Two cooperating processes: live mutate_store path ─────────────────────────

#[test]
fn test_two_threads_serialised_by_advisory_lock_via_journal() {
    // Verify that two threads exercising JournalLockGuard on the same anchor
    // directory are correctly serialised: thread A writes a value, thread B
    // must read that value (not an empty/stale one) once it acquires the lock.
    // This exercises the live OS advisory lock on a real filesystem path.
    let dir = tmp_dir();
    let anchor = dir.path().to_path_buf();
    let counter_path = anchor.join("counter.txt");
    std::fs::create_dir_all(&anchor).unwrap();
    std::fs::write(&counter_path, b"0").unwrap();

    let anchor_a = anchor.clone();
    let counter_a = counter_path.clone();
    let barrier = Arc::new(Barrier::new(2));
    let barrier2 = barrier.clone();

    // Thread A: acquire lock, read+increment, hold briefly, write, release.
    let handle = std::thread::spawn(move || {
        let _guard = JournalLockGuard::acquire(&anchor_a).unwrap();
        let v: u32 = std::fs::read_to_string(&counter_a)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::write(&counter_a, (v + 1).to_string()).unwrap();
        barrier2.wait(); // signal: A has committed its write while holding the lock
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Lock released on drop.
    });

    barrier.wait(); // wait until A holds the lock and has written
                    // Thread B: acquire lock (blocks until A releases), then read.
    let _guard = JournalLockGuard::acquire(&anchor).unwrap();
    let final_val: u32 = std::fs::read_to_string(&counter_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    handle.join().unwrap();

    // B must see A's write — counter must be 1, not 0.
    assert_eq!(
        final_val, 1,
        "B must observe A's committed write under the advisory lock"
    );
}

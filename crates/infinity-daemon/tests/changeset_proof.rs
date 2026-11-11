//! Changeset WAL proofs — D-N054 SC-2, H-SC-1 / H-M3.
//!
//! All tests are local-only: a SQLite store (in-memory or a temp file),
//! no network, no subprocess, no clone. The H-SC-1 proof is
//! `changeset_created_without_copy`: a 1 MiB blob is stored once, and
//! the changeset row holds its SHA-256 only — never the content.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use infinity_daemon::changeset::{
    CHANGESET_SCHEMA_VERSION, ChangesetStore, GENESIS_PREV_HASH, MAX_BLOB_BYTES,
    MAX_CHANGESET_FILES, NewFile,
};
use rusqlite::{Connection, params};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Nanos since epoch + a per-process counter: unique without touching
    // `std::process` (the module's no-subprocess proof greps for it).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("aspace-sc2-changeset-{tag}-{nanos}-{n}.sqlite"))
}

fn cleanup(path: &PathBuf) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(p);
    }
}

/// H-SC-1 — a changeset is created without a copy: the blob lives once
/// in `changeset_blobs`, the changeset row holds its SHA-256 only.
#[test]
fn changeset_created_without_copy() {
    let mut store = ChangesetStore::open_in_memory().expect("open");
    // Distinctive 1 MiB content: the changeset must never hold it.
    let content: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
    let first = store
        .create_changeset(
            "le-1",
            "add big blob",
            &[NewFile::write("data/blob.bin", content.clone())],
        )
        .expect("create");
    assert_eq!(first.files.len(), 1);
    let hash = first.files[0].blob_sha256.clone().expect("hash");
    assert_eq!(hash.len(), 64, "pointer is a 64-hex SHA-256, not content");
    assert_eq!(first.files[0].mode, "write");
    assert_eq!(first.prev_hash, GENESIS_PREV_HASH);
    assert_eq!(store.blob_count().expect("count"), 1);
    assert_eq!(store.blob_bytes_total().expect("bytes"), 1024 * 1024);
    // The stored blob round-trips byte-identical.
    assert_eq!(store.read_blob(&hash).expect("read"), Some(content));
}

/// Same content in two changesets: one blob row (dedup across the log).
#[test]
fn dedup_across_changesets() {
    let mut store = ChangesetStore::open_in_memory().expect("open");
    for (msg, path) in [("one", "a.txt"), ("two", "b.txt")] {
        store
            .create_changeset("le-1", msg, &[NewFile::write(path, b"shared".to_vec())])
            .expect("create");
    }
    assert_eq!(store.changeset_count().expect("count"), 2);
    assert_eq!(store.blob_count().expect("blobs"), 1);
}

/// Chain linkage + replay: three entries fold into the working state,
/// a deletion removes its path.
#[test]
fn wal_chain_and_replay() {
    let mut store = ChangesetStore::open_in_memory().expect("open");
    let first = store
        .create_changeset("le-1", "add a", &[NewFile::write("a.txt", b"v1".to_vec())])
        .expect("c1");
    let second = store
        .create_changeset(
            "le-1",
            "modify a, add b",
            &[
                NewFile::write("a.txt", b"v2".to_vec()),
                NewFile::write("b.txt", b"bee".to_vec()),
            ],
        )
        .expect("c2");
    let third = store
        .create_changeset("le-1", "delete b", &[NewFile::delete("b.txt")])
        .expect("c3");
    assert_eq!(second.parent_id.as_deref(), Some(first.id.as_str()));
    assert_eq!(third.parent_id.as_deref(), Some(second.id.as_str()));
    assert_eq!(second.prev_hash, first.entry_hash);
    assert_eq!(third.prev_hash, second.entry_hash);
    assert_eq!(store.verify_chain().expect("verify"), 3);
    let state = store.replay().expect("replay");
    let mut expected = BTreeMap::new();
    expected.insert(
        "a.txt".to_owned(),
        infinity_daemon::changeset::sha256_hex(b"v2"),
    );
    assert_eq!(state, expected, "b deleted, a at v2");
    assert_eq!(store.head().expect("head").expect("head").id, third.id);
    assert_eq!(
        store
            .get_changeset(&first.id)
            .expect("get")
            .expect("found")
            .files
            .len(),
        1
    );
}

/// A tampered link is refused: the chain is verified before any replay.
#[test]
fn tamper_detected() {
    let path = tmp_path("tamper");
    cleanup(&path);
    let first_id;
    {
        let mut store = ChangesetStore::open(&path).expect("open");
        let first = store
            .create_changeset("le-1", "one", &[NewFile::write("a.txt", b"v1".to_vec())])
            .expect("c1");
        first_id = first.id;
        store
            .create_changeset("le-1", "two", &[NewFile::write("b.txt", b"v2".to_vec())])
            .expect("c2");
    }
    {
        let conn = Connection::open(&path).expect("raw open");
        conn.execute(
            "UPDATE changesets SET prev_hash = ?1 WHERE seq = 2",
            params!["00".repeat(32)],
        )
        .expect("tamper");
    }
    let store = ChangesetStore::open(&path).expect("reopen");
    assert!(
        store.verify_chain().is_err(),
        "broken link must fail verify"
    );
    assert!(store.replay().is_err(), "broken link must fail replay");
    // The untampered head is still addressable by id.
    assert_eq!(
        store
            .get_changeset(&first_id)
            .expect("get")
            .expect("found")
            .seq,
        1
    );
    cleanup(&path);
}

/// D2 — the changeset schema extends an existing store (here a
/// factory-style base with a `rigs` table): pre-existing rows survive.
#[test]
fn extends_existing_store() {
    let path = tmp_path("extend");
    cleanup(&path);
    {
        let raw = Connection::open(&path).expect("raw open");
        raw.execute_batch(
            "CREATE TABLE rigs (id TEXT PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO rigs (id, name) VALUES ('rig-1', 'factory');",
        )
        .expect("seed factory store");
        let mut store = ChangesetStore::from_connection(raw).expect("extend");
        assert_eq!(
            store.schema_version().expect("version"),
            CHANGESET_SCHEMA_VERSION
        );
        store
            .create_changeset("le-1", "first", &[NewFile::write("a.txt", b"v1".to_vec())])
            .expect("create");
        assert_eq!(store.changeset_count().expect("count"), 1);
    }
    // The pre-existing factory table and row are untouched.
    let raw = Connection::open(&path).expect("raw reopen");
    let name: String = raw
        .query_row("SELECT name FROM rigs WHERE id = 'rig-1'", [], |row| {
            row.get(0)
        })
        .expect("factory row survives");
    assert_eq!(name, "factory");
    let tables: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'changeset%'",
            [],
            |row| row.get(0),
        )
        .expect("tables");
    assert_eq!(tables, 4, "3 changeset tables + schema version table");
    cleanup(&path);
}

/// D2 (file-backed) + reprise — WAL mode on a file store, data survives
/// close + reopen, and the pre-existing factory table survives the
/// schema extension.
#[test]
fn wal_mode_on_file_and_reopen() {
    let path = tmp_path("wal");
    cleanup(&path);
    let first_id;
    {
        let raw = Connection::open(&path).expect("raw open");
        raw.execute_batch(
            "CREATE TABLE rigs (id TEXT PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO rigs (id, name) VALUES ('rig-1', 'factory');",
        )
        .expect("seed factory store");
        let mut store = ChangesetStore::from_connection(raw).expect("extend");
        assert_eq!(store.journal_mode().expect("mode"), "wal");
        let first = store
            .create_changeset("le-1", "one", &[NewFile::write("a.txt", b"v1".to_vec())])
            .expect("c1");
        first_id = first.id;
    }
    let store = ChangesetStore::open(&path).expect("reopen");
    assert_eq!(store.journal_mode().expect("mode"), "wal");
    let head = store
        .head()
        .expect("head")
        .expect("head present after reopen");
    assert_eq!(head.id, first_id);
    assert_eq!(store.verify_chain().expect("verify"), 1);
    // Factory row still there, read through a raw handle.
    let raw = Connection::open(&path).expect("raw reopen");
    let name: String = raw
        .query_row("SELECT name FROM rigs WHERE id = 'rig-1'", [], |row| {
            row.get(0)
        })
        .expect("factory row survives");
    assert_eq!(name, "factory");
    cleanup(&path);
}

/// Caps and validation: refused with a reason, never a partial write.
#[test]
fn caps_and_validation_refused() {
    let mut store = ChangesetStore::open_in_memory().expect("open");
    assert!(store.create_changeset("le-1", "empty", &[]).is_err());
    assert!(
        store
            .create_changeset(
                "le-1",
                "dup",
                &[
                    NewFile::write("a.txt", b"1".to_vec()),
                    NewFile::write("a.txt", b"2".to_vec()),
                ],
            )
            .is_err()
    );
    assert!(
        store
            .create_changeset(
                "le-1",
                "abs",
                &[NewFile::write("/etc/passwd", b"x".to_vec())]
            )
            .is_err()
    );
    assert!(
        store
            .create_changeset(
                "le-1",
                "escape",
                &[NewFile::write("../evil", b"x".to_vec())]
            )
            .is_err()
    );
    assert!(
        store
            .create_changeset("", "no author", &[NewFile::write("a.txt", b"x".to_vec())])
            .is_err()
    );
    let too_many: Vec<NewFile> = (0..=MAX_CHANGESET_FILES)
        .map(|i| NewFile::write(format!("f{i}.txt"), b"x".to_vec()))
        .collect();
    assert!(
        store
            .create_changeset("le-1", "too many", &too_many)
            .is_err()
    );
    assert_eq!(
        store.changeset_count().expect("count"),
        0,
        "no partial write"
    );
    let too_big = vec![0u8; MAX_BLOB_BYTES + 1];
    assert!(
        store
            .create_changeset("le-1", "too big", &[NewFile::write("big.bin", too_big)])
            .is_err()
    );
    assert_eq!(store.changeset_count().expect("count"), 0);
}

/// Reading an absent blob is `None` — an absence of data, never an
/// empty blob presented as data.
#[test]
fn read_blob_absent_is_none() {
    let store = ChangesetStore::open_in_memory().expect("open");
    assert_eq!(
        store.read_blob("00".repeat(32).as_str()).expect("read"),
        None
    );
}

//! Changeset WAL — D-N054 SC-2 (design D2, H-SC-1).
//!
//! The native source control module grows its write-ahead log: a changeset
//! is one append-only entry carrying **content-addressed pointers**, never
//! a copy of the content it describes. The store is the factory's SQLite
//! store, extended — the same store as `factory.sqlite`, not a second one
//! (D2): [`ChangesetStore::from_connection`] applies the changeset schema
//! to any open connection, so the factory store gains three tables and
//! loses nothing. The machine (the PC, where `factory.sqlite` lives) is
//! where the WAL is replayed; the replay proof is a receipt, not paper.
//!
//! Tables (schema version 1):
//!
//! | table | role |
//! |-------|------|
//! | `changeset_blobs` | content, once, keyed by its SHA-256 (`INSERT OR IGNORE` dedupes) |
//! | `changesets` | one WAL entry per changeset: ULID id, parent, author, message, ISO-8601 UTC instant, `entry_hash`, `prev_hash` |
//! | `changeset_files` | the pointers: `(changeset_seq, path)` → `blob_sha256` (`NULL` = deletion) |
//!
//! The chain is tamper-evident: every entry hashes its predecessor
//! (`prev_hash` = previous `entry_hash`, genesis = 64 zeroes) and its own
//! canonical serialization, so [`ChangesetStore::verify_chain`] and
//! [`ChangesetStore::replay`] refuse a broken chain instead of replaying
//! it. A changeset is created against the chain head — its parent is the
//! head's id, never a caller-supplied guess — so two writers cannot fork
//! the log silently; the second create simply parents on the first.
//!
//! **No copy (H-SC-1).** [`ChangesetStore::create_changeset`] stores each
//! blob once in `changeset_blobs` and writes only its 64-hex SHA-256 into
//! `changeset_files`. The changeset row holds hashes, never content.
//!
//! **What is NOT here yet.** Derived data is SC-3 (receipts, judgments
//! and hold-outs become computed data, D3); the `r10-receipt` check and
//! the structured "no" are SC-4 (D6); hold-outs as gates are SC-5. This
//! module carries no judgment and no receipt — a changeset without a
//! verdict is data, not a gate.
//!
//! **No clone, no network, no subprocess (H-M4).** This module touches
//! one SQLite file only. It never invokes `git`, never spawns a process,
//! never opens a socket. Content addressing is SHA-256 (`sha2`, the only
//! dependency this step adds — `std` has no SHA-256, and
//! `DefaultHasher` is not stable across releases, which disqualifies it
//! for addressing); ids are the daemon's own ULIDs
//! (`openrig_lineage::generate_ulid`, 0 dependency); timestamps are the
//! daemon's std-only ISO-8601 UTC (`openrig_state::iso_now`).
//!
//! **Clean-room.** Written from the design and the SQLite WAL semantics.
//! No code is copied from `CarterT27/scion` (reference only, per the
//! design) or from any other implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::BoxError;
use crate::openrig_lineage::generate_ulid;
use crate::openrig_state::iso_now;

/// This step's schema version, recorded in `changeset_schema_version`.
pub const CHANGESET_SCHEMA_VERSION: i64 = 1;

/// Maximum files in one changeset. More is refused with a reason.
pub const MAX_CHANGESET_FILES: usize = 1_000;

/// Maximum bytes of one blob. More is refused with a reason.
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// `prev_hash` of the first changeset: the log starts from zeroes.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Tables this module owns inside the (shared) factory store.
pub const CHANGESET_TABLES: &[&str] = &["changeset_blobs", "changesets", "changeset_files"];

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS changeset_schema_version (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS changeset_blobs (
  sha256 TEXT PRIMARY KEY, bytes BLOB NOT NULL, size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS changesets (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE, parent_id TEXT,
  author TEXT NOT NULL, message TEXT NOT NULL, created_at TEXT NOT NULL,
  entry_hash TEXT NOT NULL, prev_hash TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS changeset_files (
  changeset_seq INTEGER NOT NULL, path TEXT NOT NULL,
  blob_sha256 TEXT, mode TEXT NOT NULL,
  PRIMARY KEY (changeset_seq, path),
  FOREIGN KEY (changeset_seq) REFERENCES changesets(seq)
);
CREATE INDEX IF NOT EXISTS idx_changeset_files_path ON changeset_files(path);
";

/// SHA-256 of `bytes`, lowercase hex (64 chars).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One file pointer inside a changeset: the path and the hash of its
/// content (`None` = the file is deleted by this changeset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetFile {
    pub path: String,
    pub blob_sha256: Option<String>,
    /// `"write"` (content present) or `"delete"` (content absent).
    pub mode: String,
}

/// A changeset: one WAL entry plus its file pointers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub seq: i64,
    pub id: String,
    pub parent_id: Option<String>,
    pub author: String,
    pub message: String,
    pub created_at: String,
    pub entry_hash: String,
    pub prev_hash: String,
    pub files: Vec<ChangesetFile>,
}

/// One file of a changeset to create: the path and its new content
/// (`None` = delete the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFile {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

impl NewFile {
    pub fn write(path: impl Into<String>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content: Some(content),
        }
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: None,
        }
    }
}

/// The changeset WAL store: one SQLite file in WAL mode, opened on
/// demand. The same connection may already carry other tables (the
/// factory store) — this store only adds its own three tables (D2).
pub struct ChangesetStore {
    conn: Connection,
}

impl ChangesetStore {
    /// Open (or create) the store at an explicit path, enabling WAL mode
    /// and applying the schema.
    pub fn open(path: &Path) -> Result<Self, BoxError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory store (tests, throwaway runs). WAL mode is a no-op on
    /// `:memory:` — [`Self::journal_mode`] reports `"memory"` there, and
    /// the file-backed proof lives in `changeset_proof.rs`.
    pub fn open_in_memory() -> Result<Self, BoxError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    /// Attach the changeset schema to an already-open connection — the
    /// D2 operation: the factory store is extended, not duplicated.
    /// Pre-existing tables and rows are untouched.
    pub fn from_connection(conn: Connection) -> Result<Self, BoxError> {
        // WAL first: the log's durability mode is set before any write.
        // On `:memory:` this is a no-op (mode stays "memory").
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(SCHEMA_SQL)?;
        let version: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM changeset_schema_version",
            [],
            |row| row.get(0),
        )?;
        if version < CHANGESET_SCHEMA_VERSION {
            conn.execute(
                "INSERT INTO changeset_schema_version (version) VALUES (?1)",
                params![CHANGESET_SCHEMA_VERSION],
            )?;
        }
        Ok(Self { conn })
    }

    /// The journal mode actually in effect (`"wal"` on a file store).
    pub fn journal_mode(&self) -> Result<String, BoxError> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    pub fn schema_version(&self) -> Result<i64, BoxError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM changeset_schema_version",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn changeset_count(&self) -> Result<i64, BoxError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM changesets", [], |row| row.get(0))?)
    }

    pub fn blob_count(&self) -> Result<i64, BoxError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM changeset_blobs", [], |row| row.get(0))?)
    }

    /// Total stored blob bytes — the content lives once, whatever the
    /// number of changesets pointing at it.
    pub fn blob_bytes_total(&self) -> Result<i64, BoxError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM changeset_blobs",
            [],
            |row| row.get(0),
        )?)
    }

    /// The chain head, if any.
    pub fn head(&self) -> Result<Option<Changeset>, BoxError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM changesets ORDER BY seq DESC LIMIT 1")?;
        let seq: Option<i64> = stmt.query_row([], |row| row.get(0)).ok();
        match seq {
            Some(seq) => Ok(Some(self.load_changeset(seq)?)),
            None => Ok(None),
        }
    }

    pub fn get_changeset(&self, id: &str) -> Result<Option<Changeset>, BoxError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM changesets WHERE id = ?1")?;
        let seq: Option<i64> = stmt.query_row(params![id], |row| row.get(0)).ok();
        match seq {
            Some(seq) => Ok(Some(self.load_changeset(seq)?)),
            None => Ok(None),
        }
    }

    pub fn list_changesets(&self) -> Result<Vec<Changeset>, BoxError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM changesets ORDER BY seq ASC")?;
        let seqs: Vec<i64> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        seqs.iter().map(|seq| self.load_changeset(*seq)).collect()
    }

    fn load_changeset(&self, seq: i64) -> Result<Changeset, BoxError> {
        let (id, parent_id, author, message, created_at, entry_hash, prev_hash) =
            self.conn.query_row(
                "SELECT id, parent_id, author, message, created_at, entry_hash, prev_hash \
                 FROM changesets WHERE seq = ?1",
                params![seq],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )?;
        let mut stmt = self.conn.prepare(
            "SELECT path, blob_sha256, mode FROM changeset_files \
             WHERE changeset_seq = ?1 ORDER BY path ASC",
        )?;
        let files: Vec<ChangesetFile> = stmt
            .query_map(params![seq], |row| {
                Ok(ChangesetFile {
                    path: row.get(0)?,
                    blob_sha256: row.get(1)?,
                    mode: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Changeset {
            seq,
            id,
            parent_id,
            author,
            message,
            created_at,
            entry_hash,
            prev_hash,
            files,
        })
    }

    /// Create one changeset, parented on the chain head, in a single
    /// transaction. Each blob is stored once (`INSERT OR IGNORE`); the
    /// changeset row stores hashes only — no copy (H-SC-1). Refuses an
    /// empty file list, duplicate paths, unsafe paths, and blobs over
    /// [`MAX_BLOB_BYTES`], with a reason, never a partial write.
    pub fn create_changeset(
        &mut self,
        author: &str,
        message: &str,
        files: &[NewFile],
    ) -> Result<Changeset, BoxError> {
        validate_files(files)?;
        if author.trim().is_empty() {
            return Err("changeset author must not be empty".into());
        }
        let tx = self.conn.transaction()?;
        let head: Option<(String, String)> = tx
            .query_row(
                "SELECT id, entry_hash FROM changesets ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();
        let (parent_id, prev_hash) = match head {
            Some((id, hash)) => (Some(id), hash),
            None => (None, GENESIS_PREV_HASH.to_owned()),
        };
        let id = generate_ulid();
        let created_at = iso_now();
        // Store each blob once; the entry keeps the hash only.
        let mut pointers: Vec<ChangesetFile> = Vec::with_capacity(files.len());
        for file in files {
            match &file.content {
                Some(bytes) => {
                    let hash = sha256_hex(bytes);
                    tx.execute(
                        "INSERT OR IGNORE INTO changeset_blobs (sha256, bytes, size) \
                         VALUES (?1, ?2, ?3)",
                        params![hash, bytes, bytes.len() as i64],
                    )?;
                    pointers.push(ChangesetFile {
                        path: file.path.clone(),
                        blob_sha256: Some(hash),
                        mode: "write".to_owned(),
                    });
                }
                None => pointers.push(ChangesetFile {
                    path: file.path.clone(),
                    blob_sha256: None,
                    mode: "delete".to_owned(),
                }),
            }
        }
        pointers.sort_by(|a, b| a.path.cmp(&b.path));
        let entry_hash = canonical_entry_hash(
            &prev_hash,
            &id,
            parent_id.as_deref(),
            author,
            message,
            &created_at,
            &pointers,
        );
        tx.execute(
            "INSERT INTO changesets \
             (id, parent_id, author, message, created_at, entry_hash, prev_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id, parent_id, author, message, created_at, entry_hash, prev_hash
            ],
        )?;
        let seq = tx.last_insert_rowid();
        for pointer in &pointers {
            tx.execute(
                "INSERT INTO changeset_files (changeset_seq, path, blob_sha256, mode) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![seq, pointer.path, pointer.blob_sha256, pointer.mode],
            )?;
        }
        tx.commit()?;
        Ok(Changeset {
            seq,
            id,
            parent_id,
            author: author.to_owned(),
            message: message.to_owned(),
            created_at,
            entry_hash,
            prev_hash,
            files: pointers,
        })
    }

    /// Read one blob by its content hash (`None` when absent — an
    /// absence of data, never a silent empty blob).
    pub fn read_blob(&self, sha256: &str) -> Result<Option<Vec<u8>>, BoxError> {
        let mut stmt = self
            .conn
            .prepare("SELECT bytes FROM changeset_blobs WHERE sha256 = ?1")?;
        Ok(stmt.query_row(params![sha256], |row| row.get(0)).ok())
    }

    /// Verify the hash chain end to end (linkage + recomputed entry
    /// hashes). Returns the number of verified entries. Any break is an
    /// error naming the sequence — never a silent replay past a break.
    pub fn verify_chain(&self) -> Result<usize, BoxError> {
        let changesets = self.list_changesets()?;
        let mut expected_prev = GENESIS_PREV_HASH.to_owned();
        for changeset in &changesets {
            if changeset.prev_hash != expected_prev {
                return Err(format!(
                    "changeset chain broken at seq {}: prev_hash {} != expected {}",
                    changeset.seq, changeset.prev_hash, expected_prev
                )
                .into());
            }
            let recomputed = canonical_entry_hash(
                &changeset.prev_hash,
                &changeset.id,
                changeset.parent_id.as_deref(),
                &changeset.author,
                &changeset.message,
                &changeset.created_at,
                &changeset.files,
            );
            if recomputed != changeset.entry_hash {
                return Err(format!(
                    "changeset chain broken at seq {}: entry_hash mismatch (stored {}, recomputed {})",
                    changeset.seq, changeset.entry_hash, recomputed
                )
                .into());
            }
            expected_prev = changeset.entry_hash.clone();
        }
        Ok(changesets.len())
    }

    /// Replay the whole log, after verifying the chain: fold every entry
    /// oldest → newest into `path → blob SHA-256` (a deletion removes the
    /// path). The result is the working state the log describes.
    pub fn replay(&self) -> Result<BTreeMap<String, String>, BoxError> {
        self.verify_chain()?;
        let mut state: BTreeMap<String, String> = BTreeMap::new();
        for changeset in self.list_changesets()? {
            for file in changeset.files {
                match file.blob_sha256 {
                    Some(hash) => {
                        state.insert(file.path, hash);
                    }
                    None => {
                        state.remove(&file.path);
                    }
                }
            }
        }
        Ok(state)
    }
}

/// Canonical serialization hashed into `entry_hash`. Field order and
/// separators are fixed; files sort by path. Any change to a stored
/// field changes the hash — that is the tamper evidence.
fn canonical_entry_hash(
    prev_hash: &str,
    id: &str,
    parent_id: Option<&str>,
    author: &str,
    message: &str,
    created_at: &str,
    files: &[ChangesetFile],
) -> String {
    let mut canonical = String::new();
    canonical.push_str(prev_hash);
    canonical.push('\n');
    canonical.push_str(id);
    canonical.push('\n');
    canonical.push_str(parent_id.unwrap_or(""));
    canonical.push('\n');
    canonical.push_str(author);
    canonical.push('\n');
    canonical.push_str(message);
    canonical.push('\n');
    canonical.push_str(created_at);
    canonical.push('\n');
    for file in files {
        canonical.push_str(&file.mode);
        canonical.push('\n');
        canonical.push_str(&file.path);
        canonical.push('\n');
        canonical.push_str(file.blob_sha256.as_deref().unwrap_or(""));
        canonical.push('\n');
    }
    sha256_hex(canonical.as_bytes())
}

fn validate_files(files: &[NewFile]) -> Result<(), BoxError> {
    if files.is_empty() {
        return Err("changeset must carry at least one file (empty changesets are refused)".into());
    }
    if files.len() > MAX_CHANGESET_FILES {
        return Err(format!(
            "changeset carries {} files, over the cap of {}",
            files.len(),
            MAX_CHANGESET_FILES
        )
        .into());
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for file in files {
        let path = file.path.as_str();
        if path.is_empty() {
            return Err("changeset file path must not be empty".into());
        }
        if path.contains('\0') {
            return Err(format!("changeset file path contains NUL: {path:?}").into());
        }
        if path.starts_with('/') {
            return Err(
                format!("changeset file path must be repo-relative, not absolute: {path}").into(),
            );
        }
        if path.split('/').any(|part| part == "..") {
            return Err(format!("changeset file path must not escape the repo: {path}").into());
        }
        if !seen.insert(path) {
            return Err(format!("changeset lists path twice: {path}").into());
        }
        if let Some(bytes) = &file.content
            && bytes.len() > MAX_BLOB_BYTES
        {
            return Err(format!(
                "blob for {path} is {} bytes, over the cap of {}",
                bytes.len(),
                MAX_BLOB_BYTES
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // FIPS 180-4: SHA-256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn canonical_hash_is_deterministic_and_sensitive() {
        let files = vec![ChangesetFile {
            path: "a.txt".to_owned(),
            blob_sha256: Some("f".repeat(64)),
            mode: "write".to_owned(),
        }];
        let base = canonical_entry_hash(
            GENESIS_PREV_HASH,
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            None,
            "le-1",
            "init",
            "2026-11-11T01:00:00Z",
            &files,
        );
        let again = canonical_entry_hash(
            GENESIS_PREV_HASH,
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            None,
            "le-1",
            "init",
            "2026-11-11T01:00:00Z",
            &files,
        );
        assert_eq!(base, again);
        let changed = canonical_entry_hash(
            GENESIS_PREV_HASH,
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            None,
            "le-1",
            "init!",
            "2026-11-11T01:00:00Z",
            &files,
        );
        assert_ne!(base, changed);
    }

    #[test]
    fn create_and_replay_in_memory() {
        let mut store = ChangesetStore::open_in_memory().expect("open");
        let first = store
            .create_changeset(
                "le-1",
                "add a",
                &[NewFile::write("a.txt", b"hello".to_vec())],
            )
            .expect("create");
        assert_eq!(first.parent_id, None);
        assert_eq!(first.prev_hash, GENESIS_PREV_HASH);
        let second = store
            .create_changeset("le-1", "delete a", &[NewFile::delete("a.txt")])
            .expect("create");
        assert_eq!(second.parent_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(store.verify_chain().expect("verify"), 2);
        assert!(store.replay().expect("replay").is_empty());
    }
}

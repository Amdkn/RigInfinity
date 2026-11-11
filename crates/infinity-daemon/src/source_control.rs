//! Source-control-as-service read API — D-N054 SC-1 (design D4, D5, D10).
//!
//! The native source control module of the RigInfinity daemon. SC-1 is the
//! first step that replaces a Git read with a native read: **one diff, one
//! blame, one ancestor served by API without a clone** — the first partial
//! proof (H-SC-3) that a question can be answered without the 28.33 Go
//! clone (D10). Git stays the substrate; this module reads it through the
//! GitHub REST API, above Git first (D1, D4).
//!
//! The API contract is exactly the 6 queries of D4 — the client asks, it
//! does not compute:
//!
//! | query     | live at SC-1 | served by |
//! |-----------|--------------|-----------|
//! | `diff`    | yes | GitHub compare API (`/compare/{base}...{head}`) |
//! | `blame`   | yes | GitHub contents + commits APIs, forward application (see below) |
//! | `ancestor`| yes | GitHub compare API (`merge_base_commit`) |
//! | `receipt` | no — SC-3 | structured `pending` (receipts become derived data at SC-3) |
//! | `judgment`| no — SC-4 | structured `pending` (judgment becomes changeset data at SC-4) |
//! | `holdout` | no — SC-5 | structured `pending` (hold-outs become gates at SC-5) |
//!
//! A pending query never returns fake data: it returns a structured
//! `pending` answer naming the step that will serve it. An absence of
//! data is never presented as data of absence (Discover §4.3).
//!
//! **Blame method (forward application, deferred per D3).** GitHub has no
//! blame REST endpoint, so the service computes blame on demand from API
//! data: it lists the commits touching the path (newest first, capped —
//! see [`MAX_BLAME_COMMITS`]), replays their patches oldest → newest
//! starting from the file's creation commit, and verifies the result
//! against the file content fetched at the requested ref. Any mismatch
//! (truncated history, missing patch, line-count drift) is an error, not
//! a silent wrong attribution. The cap keeps the query cheap; a file with
//! more touching commits than the cap is refused with a reason.
//!
//! **No clone, ever.** This module never invokes `git`, never spawns a
//! subprocess, never writes a checkout. The only outbound traffic is
//! HTTPS to the GitHub API, and only when a query is asked (H-M4). The
//! token, when present, comes from the environment (`GITHUB_TOKEN`, else
//! `GH_TOKEN`) — never from a file, never logged, never stored.
//!
//! **Clean-room.** Written from the GitHub REST API surface and the
//! standard line-attribution algorithm. No code is copied from
//! `CarterT27/scion` (reference only, per the design) or from any other
//! implementation.
//!
//! **Transport as a trait** (house pattern, `provider.rs`): the production
//! transport is reqwest; tests run against a loopback stub, no network.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::BoxError;

/// The 6 queries of the D4 contract, exactly.
pub const QUERIES: &[&str] = &[
    "diff", "blame", "ancestor", "receipt", "judgment", "holdout",
];

/// Default GitHub API base. Overridable per client (tests point it at a
/// loopback stub).
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Maximum commits touching a path that blame will replay. More than this
/// is refused with a reason (see [`BlameError::HistoryTruncated`]).
pub const MAX_BLAME_COMMITS: usize = 100;

/// Maximum lines blame will attribute. More than this is refused.
pub const MAX_BLAME_LINES: usize = 2_000;

/// How the service identifies itself to the GitHub API.
pub const USER_AGENT: &str = "aspace-v5-source-control-natif/1 (D-N054 SC-1)";

/// One of the 6 contract queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Query {
    Diff,
    Blame,
    Ancestor,
    Receipt,
    Judgment,
    Holdout,
}

impl Query {
    /// Parse a query name; unknown names are an error, not a guess.
    pub fn parse(name: &str) -> Result<Self, BoxError> {
        match name {
            "diff" => Ok(Self::Diff),
            "blame" => Ok(Self::Blame),
            "ancestor" => Ok(Self::Ancestor),
            "receipt" => Ok(Self::Receipt),
            "judgment" => Ok(Self::Judgment),
            "holdout" => Ok(Self::Holdout),
            other => Err(format!("unknown source-control query: {other}").into()),
        }
    }

    /// Canonical name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Blame => "blame",
            Self::Ancestor => "ancestor",
            Self::Receipt => "receipt",
            Self::Judgment => "judgment",
            Self::Holdout => "holdout",
        }
    }

    /// True for the 3 queries live at SC-1.
    pub fn live_at_sc1(self) -> bool {
        matches!(self, Self::Diff | Self::Blame | Self::Ancestor)
    }

    /// The step that will serve a pending query (`None` when live).
    pub fn pending_pas(self) -> Option<&'static str> {
        match self {
            Self::Receipt => Some("SC-3"),
            Self::Judgment => Some("SC-4"),
            Self::Holdout => Some("SC-5"),
            _ => None,
        }
    }
}

/// Structured answer for a query the design has not served yet. The
/// `reason` names the dependency; the `pas` names the step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAnswer {
    pub query: String,
    pub pas: String,
    pub reason: String,
    /// Always true: this is an absence of data, not data of absence.
    pub pending: bool,
}

impl PendingAnswer {
    fn for_query(query: Query) -> Result<Self, BoxError> {
        let pas = query
            .pending_pas()
            .ok_or_else(|| format!("query {} is live at SC-1, not pending", query.name()))?;
        let reason = match query {
            Query::Receipt => {
                "receipts become derived data of the changeset at SC-3 (D3, H-SC-2)".to_owned()
            }
            Query::Judgment => "judgments become changeset data at SC-4 (D6, H-SC-6)".to_owned(),
            Query::Holdout => "hold-outs become gates of the changeset at SC-5 (H-SC-7)".to_owned(),
            _ => unreachable!(),
        };
        Ok(Self {
            query: query.name().to_owned(),
            pas: pas.to_owned(),
            reason,
            pending: true,
        })
    }
}

/// One file in a diff answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub changes: u64,
    /// Unified patch when the API returned one (text files only).
    pub patch: Option<String>,
}

/// A diff served by API, without a clone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffAnswer {
    pub base: String,
    pub head: String,
    pub merge_base: String,
    pub ahead_by: u64,
    pub behind_by: u64,
    pub files: Vec<DiffFile>,
    /// Always `"github-api"`: the substrate that served the read.
    pub served_by: String,
    /// Always `false`: no clone was made to answer.
    pub clone: bool,
}

/// The merge-base ancestor of two refs, served by API, without a clone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AncestorAnswer {
    pub base: String,
    pub head: String,
    pub merge_base: String,
    pub merge_base_message: Option<String>,
    pub served_by: String,
    pub clone: bool,
}

/// One attributed line of a blame answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    pub line: usize,
    pub sha: String,
    pub text: String,
}

/// A blame served by API, without a clone. Attribution is by forward
/// application of the touching commits' patches (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameAnswer {
    pub path: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub lines: Vec<BlameLine>,
    /// SHAs of the commits replayed, oldest first.
    pub commits: Vec<String>,
    pub method: String,
    pub served_by: String,
    pub clone: bool,
}

/// Blame failure modes — every one names the missing data, never a guess.
#[derive(Debug)]
pub enum BlameError {
    /// The file has no commit touching it at this ref.
    NoTouchingCommit,
    /// The touching history is at (or over) the replay cap.
    HistoryTruncated { touching: usize, cap: usize },
    /// The oldest replayed commit does not create the file, so the base
    /// content is unknown.
    NoCreationCommit,
    /// A touching commit has no usable patch for the path.
    PatchUnavailable { sha: String },
    /// The replayed content does not match the content at the ref.
    ContentMismatch { replayed: usize, at_ref: usize },
    /// The file is larger than the attribution cap.
    TooManyLines { lines: usize, cap: usize },
}

impl std::fmt::Display for BlameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTouchingCommit => write!(f, "no commit touches this path at this ref"),
            Self::HistoryTruncated { touching, cap } => write!(
                f,
                "blame history truncated: {touching} touching commits reach the cap of {cap} — refusing rather than misattributing"
            ),
            Self::NoCreationCommit => write!(
                f,
                "oldest replayed commit does not create the file — base content unknown"
            ),
            Self::PatchUnavailable { sha } => {
                write!(f, "commit {sha} has no usable patch for this path")
            }
            Self::ContentMismatch { replayed, at_ref } => write!(
                f,
                "replayed content ({replayed} lines) does not match content at ref ({at_ref} lines)"
            ),
            Self::TooManyLines { lines, cap } => {
                write!(f, "file has {lines} lines, over the blame cap of {cap}")
            }
        }
    }
}

impl std::error::Error for BlameError {}

/// The HTTP transport behind the client. The production transport is
/// reqwest; tests stub it (house pattern, `provider.rs`).
#[async_trait]
pub trait SourceControlTransport: Send + Sync {
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, BoxError>;
}

/// Production transport: reqwest against the GitHub API (or a loopback
/// stub when the client base URL says so). Token from the environment
/// only (`GITHUB_TOKEN`, else `GH_TOKEN`); never logged, never stored.
pub struct ReqwestTransport {
    client: reqwest::Client,
    token: Option<String>,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .or_else(|| std::env::var("GH_TOKEN").ok())
            .filter(|t| !t.trim().is_empty());
        Self {
            client: reqwest::Client::new(),
            token,
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SourceControlTransport for ReqwestTransport {
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, BoxError> {
        let mut req = self.client.get(url).header("User-Agent", USER_AGENT);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(format!("github api {status} for {url}: {body}").into());
        }
        Ok(serde_json::from_str(&body)?)
    }
}

/// The read API client. `base_url` defaults to the GitHub API; tests
/// point it at a loopback stub.
pub struct SourceControlClient<T: SourceControlTransport> {
    transport: T,
    base_url: String,
}

impl<T: SourceControlTransport> SourceControlClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            base_url: GITHUB_API_BASE.to_owned(),
        }
    }

    pub fn with_base_url(transport: T, base_url: &str) -> Self {
        Self {
            transport,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// The structured `pending` answer for a query not served yet.
    pub fn pending(&self, query: Query) -> Result<PendingAnswer, BoxError> {
        PendingAnswer::for_query(query)
    }

    /// `diff`: the files changed between `base` and `head`, via the
    /// compare API. No clone.
    pub async fn diff(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> Result<DiffAnswer, BoxError> {
        let url = format!(
            "{}/repos/{}/{}/compare/{}...{}",
            self.base_url,
            encode_segment(owner),
            encode_segment(repo),
            encode_segment(base),
            encode_segment(head)
        );
        let v = self.transport.get_json(&url).await?;
        let files = v
            .get("files")
            .and_then(|f| f.as_array())
            .ok_or("compare response has no files array")?;
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            out.push(DiffFile {
                filename: req_str(f, "filename")?,
                status: req_str(f, "status")?,
                additions: f.get("additions").and_then(|n| n.as_u64()).unwrap_or(0),
                deletions: f.get("deletions").and_then(|n| n.as_u64()).unwrap_or(0),
                changes: f.get("changes").and_then(|n| n.as_u64()).unwrap_or(0),
                patch: f
                    .get("patch")
                    .and_then(|p| p.as_str())
                    .map(|p| p.to_owned()),
            });
        }
        Ok(DiffAnswer {
            base: base.to_owned(),
            head: head.to_owned(),
            merge_base: req_str(
                v.get("merge_base_commit")
                    .ok_or("compare response has no merge_base_commit")?,
                "sha",
            )?,
            ahead_by: v.get("ahead_by").and_then(|n| n.as_u64()).unwrap_or(0),
            behind_by: v.get("behind_by").and_then(|n| n.as_u64()).unwrap_or(0),
            files: out,
            served_by: "github-api".to_owned(),
            clone: false,
        })
    }

    /// `ancestor`: the merge-base of `base` and `head`, via the compare
    /// API. No clone.
    pub async fn ancestor(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> Result<AncestorAnswer, BoxError> {
        let url = format!(
            "{}/repos/{}/{}/compare/{}...{}",
            self.base_url,
            encode_segment(owner),
            encode_segment(repo),
            encode_segment(base),
            encode_segment(head)
        );
        let v = self.transport.get_json(&url).await?;
        let mb = v
            .get("merge_base_commit")
            .ok_or("compare response has no merge_base_commit")?;
        Ok(AncestorAnswer {
            base: base.to_owned(),
            head: head.to_owned(),
            merge_base: req_str(mb, "sha")?,
            merge_base_message: mb
                .get("commit")
                .and_then(|c| c.get("message"))
                .and_then(|m| m.as_str())
                .map(|m| m.to_owned()),
            served_by: "github-api".to_owned(),
            clone: false,
        })
    }

    /// `blame`: per-line attribution for `path` at `ref_`, by forward
    /// application of the touching commits' patches, verified against
    /// the content at the ref. No clone.
    pub async fn blame(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_: &str,
    ) -> Result<BlameAnswer, BoxError> {
        // 1. Commits touching the path, newest first.
        let list_url = format!(
            "{}/repos/{}/{}/commits?path={}&sha={}&per_page={}",
            self.base_url,
            encode_segment(owner),
            encode_segment(repo),
            encode_path(path),
            encode_segment(ref_),
            MAX_BLAME_COMMITS
        );
        let list = self.transport.get_json(&list_url).await?;
        let arr = list.as_array().ok_or("commits response is not an array")?;
        if arr.is_empty() {
            return Err(Box::new(BlameError::NoTouchingCommit));
        }
        if arr.len() >= MAX_BLAME_COMMITS {
            return Err(Box::new(BlameError::HistoryTruncated {
                touching: arr.len(),
                cap: MAX_BLAME_COMMITS,
            }));
        }
        let mut shas: Vec<String> = Vec::with_capacity(arr.len());
        for c in arr {
            shas.push(req_str(c, "sha")?);
        }
        // Oldest first for the replay.
        shas.reverse();

        // 2. Replay patches oldest -> newest, from an empty file.
        let mut lines: Vec<String> = Vec::new();
        let mut attrib: Vec<String> = Vec::new();
        let mut seen_creation = false;
        for sha in &shas {
            let commit_url = format!(
                "{}/repos/{}/{}/commits/{}",
                self.base_url,
                encode_segment(owner),
                encode_segment(repo),
                encode_segment(sha)
            );
            let commit = self.transport.get_json(&commit_url).await?;
            let files = commit
                .get("files")
                .and_then(|f| f.as_array())
                .ok_or(format!("commit {sha} has no files array"))?;
            let mut entry: Option<&serde_json::Value> = None;
            for f in files {
                if f.get("filename").and_then(|n| n.as_str()) == Some(path) {
                    entry = Some(f);
                    break;
                }
            }
            let Some(f) = entry else {
                // Commit listed for the path but carries no entry for it
                // (rename edge): skip it rather than guessing.
                continue;
            };
            let status = f.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "removed" {
                lines.clear();
                attrib.clear();
                seen_creation = false;
                continue;
            }
            if status == "renamed" {
                // A rename carries no content patch: the content at this
                // commit becomes the base, attributed to this commit.
                let renamed_url = format!(
                    "{}/repos/{}/{}/contents/{}?ref={}",
                    self.base_url,
                    encode_segment(owner),
                    encode_segment(repo),
                    encode_path(path),
                    encode_segment(sha)
                );
                let renamed_v = self.transport.get_json(&renamed_url).await?;
                let renamed_raw = decode_base64(&req_str(&renamed_v, "content")?)?;
                let renamed_text = String::from_utf8(renamed_raw)?;
                lines = split_lines(&renamed_text);
                attrib = vec![sha.clone(); lines.len()];
                seen_creation = true;
                continue;
            }
            if status == "added" {
                seen_creation = true;
            }
            let Some(patch) = f.get("patch").and_then(|p| p.as_str()) else {
                return Err(Box::new(BlameError::PatchUnavailable { sha: sha.clone() }));
            };
            apply_patch(&mut lines, &mut attrib, patch, sha)?;
            if lines.len() > MAX_BLAME_LINES {
                return Err(Box::new(BlameError::TooManyLines {
                    lines: lines.len(),
                    cap: MAX_BLAME_LINES,
                }));
            }
        }
        if !seen_creation {
            return Err(Box::new(BlameError::NoCreationCommit));
        }

        // 3. Verify against the content at the ref — never trust the
        // replay alone.
        let content_url = format!(
            "{}/repos/{}/{}/contents/{}?ref={}",
            self.base_url,
            encode_segment(owner),
            encode_segment(repo),
            encode_path(path),
            encode_segment(ref_)
        );
        let content_v = self.transport.get_json(&content_url).await?;
        let encoded = req_str(&content_v, "content")?;
        let raw = decode_base64(&encoded)?;
        let at_ref = String::from_utf8(raw)?;
        let at_ref_lines = split_lines(&at_ref);
        if at_ref_lines.len() != lines.len() || at_ref_lines != lines {
            return Err(Box::new(BlameError::ContentMismatch {
                replayed: lines.len(),
                at_ref: at_ref_lines.len(),
            }));
        }
        // The touching list may name commits the replay skipped (rename
        // edge); the answer reports the commits actually replayed.
        let replayed: HashSet<&str> = attrib.iter().map(|s| s.as_str()).collect();
        let commits: Vec<String> = shas
            .into_iter()
            .filter(|s| replayed.contains(s.as_str()))
            .collect();
        Ok(BlameAnswer {
            path: path.to_owned(),
            ref_: ref_.to_owned(),
            lines: lines
                .into_iter()
                .enumerate()
                .zip(attrib)
                .map(|((i, text), sha)| BlameLine {
                    line: i + 1,
                    sha,
                    text,
                })
                .collect(),
            commits,
            method: "forward-application".to_owned(),
            served_by: "github-api".to_owned(),
            clone: false,
        })
    }
}

fn req_str(v: &serde_json::Value, key: &str) -> Result<String, BoxError> {
    v.get(key)
        .and_then(|s| s.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| format!("response has no string field {key}").into())
}

/// Percent-encode one path segment (RFC 3986 unreserved set kept bare).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a repo path, keeping `/` separators.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Split file text into lines (no trailing empty line for a final `\n`).
fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.to_owned()).collect();
    // `str::lines` already drops the final empty piece; an empty file
    // yields no lines, which is what the replay produces too.
    if text.is_empty() {
        lines.clear();
    }
    lines
}

/// Minimal base64 decoder (no new dependency): the contents API returns
/// file content base64-encoded, wrapped at 60 columns.
fn decode_base64(input: &str) -> Result<Vec<u8>, BoxError> {
    fn val(b: u8) -> Result<u8, BoxError> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a' + 26),
            b'0'..=b'9' => Ok(b - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte: {b}").into()),
        }
    }
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(4) {
        return Err("base64 input length is not a multiple of 4".into());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let pad = chunk.iter().filter(|b| **b == b'=').count();
        if pad > 2 {
            return Err("invalid base64 padding".into());
        }
        let mut n: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            if *b == b'=' {
                if i < 2 {
                    return Err("invalid base64 padding position".into());
                }
                n <<= 6;
            } else {
                n = (n << 6) | u32::from(val(*b)?);
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// One hunk of a unified patch.
#[derive(Debug, PartialEq, Eq)]
struct Hunk {
    old_start: usize, // 1-based; 0 when old_len == 0
    old_len: usize,
    new_start: usize, // 1-based; 0 when new_len == 0
    new_len: usize,
    /// (' ', context) | ('-', removed) | ('+', added), with line text.
    ops: Vec<(char, String)>,
}

/// Parse the hunks of a unified patch. Returns an error on a malformed
/// header or on a hunk whose op counts disagree with its header.
fn parse_hunks(patch: &str) -> Result<Vec<Hunk>, BoxError> {
    fn parse_range(s: &str) -> Result<(usize, usize), BoxError> {
        match s.split_once(',') {
            Some((a, b)) => Ok((
                a.parse().map_err(|_| format!("bad hunk range: {s}"))?,
                b.parse().map_err(|_| format!("bad hunk range: {s}"))?,
            )),
            None => Ok((s.parse().map_err(|_| format!("bad hunk range: {s}"))?, 1)),
        }
    }
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("@@ -") {
            if let Some(cur) = current.take() {
                check_hunk(&cur)?;
                hunks.push(cur);
            }
            let (ranges, _) = rest.split_once(" @@").ok_or("malformed hunk header")?;
            let (old_s, new_s) = ranges.split_once(" +").ok_or("malformed hunk header")?;
            let (old_start, old_len) = parse_range(old_s)?;
            let (new_start, new_len) = parse_range(new_s)?;
            current = Some(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                ops: Vec::new(),
            });
        } else if raw.starts_with('\\') {
            // "\ No newline at end of file" — marker, not a line.
            continue;
        } else if let Some(cur) = current.as_mut() {
            let (op, text) = match raw.chars().next() {
                Some(' ') => (' ', raw[1..].to_owned()),
                Some('-') => ('-', raw[1..].to_owned()),
                Some('+') => ('+', raw[1..].to_owned()),
                None => (' ', String::new()),
                _ => return Err(format!("unexpected patch line: {raw}").into()),
            };
            cur.ops.push((op, text));
        }
    }
    if let Some(cur) = current.take() {
        check_hunk(&cur)?;
        hunks.push(cur);
    }
    Ok(hunks)
}

fn check_hunk(h: &Hunk) -> Result<(), BoxError> {
    let old_count = h
        .ops
        .iter()
        .filter(|(op, _)| *op == ' ' || *op == '-')
        .count();
    let new_count = h
        .ops
        .iter()
        .filter(|(op, _)| *op == ' ' || *op == '+')
        .count();
    if old_count != h.old_len || new_count != h.new_len {
        return Err(format!(
            "hunk @@ -{},{} +{},{} @@ has {} old / {} new lines",
            h.old_start, h.old_len, h.new_start, h.new_len, old_count, new_count
        )
        .into());
    }
    Ok(())
}

/// Apply one commit's patch to the replayed file, attributing added
/// lines to `sha`. Context lines must match the replayed content —
/// a mismatch is an error, never a silent drift.
fn apply_patch(
    lines: &mut Vec<String>,
    attrib: &mut Vec<String>,
    patch: &str,
    sha: &str,
) -> Result<(), BoxError> {
    let hunks = parse_hunks(patch)?;
    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() + 8);
    let mut new_attrib: Vec<String> = Vec::with_capacity(attrib.len() + 8);
    let mut cursor = 0usize; // 0-based index into the old file
    for h in &hunks {
        let hunk_start = h.old_start.saturating_sub(1);
        if hunk_start < cursor || hunk_start > lines.len() {
            return Err(format!(
                "hunk starts at line {} but replay cursor is at {}",
                h.old_start,
                cursor + 1
            )
            .into());
        }
        while cursor < hunk_start {
            new_lines.push(lines[cursor].clone());
            new_attrib.push(attrib[cursor].clone());
            cursor += 1;
        }
        for (op, text) in &h.ops {
            match op {
                ' ' => {
                    if cursor >= lines.len() || lines[cursor] != *text {
                        return Err(format!(
                            "patch context mismatch at line {}: expected {:?}",
                            cursor + 1,
                            text
                        )
                        .into());
                    }
                    new_lines.push(lines[cursor].clone());
                    new_attrib.push(attrib[cursor].clone());
                    cursor += 1;
                }
                '-' => {
                    if cursor >= lines.len() || lines[cursor] != *text {
                        return Err(format!(
                            "patch removal mismatch at line {}: expected {:?}",
                            cursor + 1,
                            text
                        )
                        .into());
                    }
                    cursor += 1;
                }
                '+' => {
                    new_lines.push(text.clone());
                    new_attrib.push(sha.to_owned());
                }
                _ => unreachable!(),
            }
        }
    }
    while cursor < lines.len() {
        new_lines.push(lines[cursor].clone());
        new_attrib.push(attrib[cursor].clone());
        cursor += 1;
    }
    *lines = new_lines;
    *attrib = new_attrib;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_names_parse_and_pending() {
        assert_eq!(Query::parse("diff").expect("test"), Query::Diff);
        assert_eq!(Query::parse("holdout").expect("test"), Query::Holdout);
        assert!(Query::parse("merge").is_err());
        assert_eq!(QUERIES.len(), 6);
        for name in QUERIES {
            let q = Query::parse(name).expect("test");
            assert_eq!(q.name(), *name);
        }
        assert!(Query::Diff.live_at_sc1());
        assert!(Query::Blame.live_at_sc1());
        assert!(Query::Ancestor.live_at_sc1());
        assert!(!Query::Receipt.live_at_sc1());
        assert_eq!(Query::Receipt.pending_pas(), Some("SC-3"));
        assert_eq!(Query::Judgment.pending_pas(), Some("SC-4"));
        assert_eq!(Query::Holdout.pending_pas(), Some("SC-5"));
        assert_eq!(Query::Diff.pending_pas(), None);
        let p = PendingAnswer::for_query(Query::Receipt).expect("test");
        assert!(p.pending);
        assert_eq!(p.pas, "SC-3");
        assert!(PendingAnswer::for_query(Query::Diff).is_err());
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(decode_base64("aGVsbG8=").expect("test"), b"hello");
        assert_eq!(
            decode_base64("aGVsbG8sIHdvcmxkIQ==").expect("test"),
            b"hello, world!"
        );
        assert_eq!(decode_base64("b25l\nVFdP\n").expect("test"), b"oneTWO");
        assert!(decode_base64("!!!").is_err());
        assert!(decode_base64("abc").is_err());
    }

    #[test]
    fn patch_apply_two_commits() {
        // Creation: 3 lines added.
        let mut lines = vec![];
        let mut attrib = vec![];
        apply_patch(
            &mut lines,
            &mut attrib,
            "@@ -0,0 +1,3 @@\n+one\n+two\n+three\n",
            "c111",
        )
        .expect("test");
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert_eq!(attrib, vec!["c111", "c111", "c111"]);
        // Second commit: line 2 changed.
        apply_patch(
            &mut lines,
            &mut attrib,
            "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n",
            "c222",
        )
        .expect("test");
        assert_eq!(lines, vec!["one", "TWO", "three"]);
        assert_eq!(attrib, vec!["c111", "c222", "c111"]);
        // Context mismatch is an error, not a drift: "two" no longer
        // exists at line 2 (it is "TWO" now).
        assert!(
            apply_patch(
                &mut lines,
                &mut attrib,
                "@@ -1,3 +1,3 @@\n one\n-two\n+deux\n three\n",
                "c333",
            )
            .is_err()
        );
    }

    #[test]
    fn hunk_counts_checked() {
        // Header says 1 new line, patch carries 2: rejected.
        assert!(parse_hunks("@@ -1,1 +1,1 @@\n old\n+new\n+extra\n").is_err());
        // Omitted lengths mean 1.
        let h = parse_hunks("@@ -2 +2 @@\n ctx\n").expect("test");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_len), (2, 1));
    }
}

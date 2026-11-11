//! Native source-control read API proofs — D-N054 SC-1, H-SC-3 / H-M3.
//!
//! 1. Stubbed (always run, no network): a loopback HTTP stub serves canned
//!    GitHub compare / commits / contents JSON; the real client answers
//!    one diff, one blame, one ancestor, and returns structured `pending`
//!    for receipt / judgment / holdout. The blame expectation is exact:
//!    line 1 and 3 attributed to the creation commit, line 2 to the
//!    modifying commit.
//! 2. Live (env-gated, `SC1_LIVE=1`): the same client hits the real
//!    GitHub API for Amdkn/Aspace_OS_V5 and serves one diff, one blame,
//!    one ancestor without cloning anything. The harness sets the
//!    refs/path from the live repo and cross-checks the answers with
//!    `gh api` independently. The token, when present, comes from the
//!    environment only (`GITHUB_TOKEN` / `GH_TOKEN`).

use infinity_daemon::source_control::{
    GITHUB_API_BASE, Query, ReqwestTransport, SourceControlClient,
};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Canned GitHub API fixtures for the stub.
fn compare_fixture() -> serde_json::Value {
    serde_json::json!({
        "merge_base_commit": {"sha": "aaa111", "commit": {"message": "base"}},
        "ahead_by": 1,
        "behind_by": 0,
        "files": [
            {"filename": "hello.txt", "status": "modified",
             "additions": 1, "deletions": 1, "changes": 2,
             "patch": "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"},
            {"filename": "new.txt", "status": "added",
             "additions": 2, "deletions": 0, "changes": 2,
             "patch": "@@ -0,0 +1,2 @@\n+alpha\n+beta\n"}
        ]
    })
}

fn commits_list_fixture() -> serde_json::Value {
    // Newest first, as the API returns them.
    serde_json::json!([
        {"sha": "c222"},
        {"sha": "c111"}
    ])
}

fn commit_fixture(sha: &str) -> serde_json::Value {
    match sha {
        "c111" => serde_json::json!({
            "sha": "c111",
            "files": [{"filename": "hello.txt", "status": "added",
                       "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three\n"}]
        }),
        "c222" => serde_json::json!({
            "sha": "c222",
            "files": [{"filename": "hello.txt", "status": "modified",
                       "patch": "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"}]
        }),
        // Merge that introduced the file: no files entry, one parent.
        "m333" => serde_json::json!({
            "sha": "m333",
            "parents": [{"sha": "p000"}],
            "files": []
        }),
        "c444" => serde_json::json!({
            "sha": "c444",
            "files": [{"filename": "hello-merge.txt", "status": "modified",
                       "patch": "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"}]
        }),
        _ => serde_json::json!({"sha": sha, "files": []}),
    }
}

fn commits_list_merge_fixture() -> serde_json::Value {
    // Newest first: a modification, then the introducing merge.
    serde_json::json!([
        {"sha": "c444"},
        {"sha": "m333"}
    ])
}

fn contents_fixture() -> serde_json::Value {
    // "one\nTWO\nthree\n" base64-encoded (final state).
    serde_json::json!({"content": "b25lClRXTwp0aHJlZQo=\n", "encoding": "base64"})
}

fn contents_before_fixture() -> serde_json::Value {
    // "one\ntwo\nthree\n" base64-encoded (state at the introducing
    // merge m333, before the c444 modification).
    serde_json::json!({"content": "b25lCnR3bwp0aHJlZQo=\n", "encoding": "base64"})
}

/// One-shot loopback stub: serves the fixtures above by request path.
/// Returns the base URL (http://127.0.0.1:PORT).
async fn stub_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub server");
    let port = listener.local_addr().expect("stub port").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut reader = BufReader::new(&mut stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body).await;
                }
                drop(reader);
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                // The merge-introduction probe: the file is absent at the
                // merge's first parent.
                if path.contains("/contents/") && path.contains("ref=p000") {
                    let body = r#"{"message":"Not Found"}"#;
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await;
                    return;
                }
                let body = if path.contains("/compare/") {
                    compare_fixture()
                } else if path.contains("/commits?") && path.contains("hello-merge.txt") {
                    commits_list_merge_fixture()
                } else if path.contains("/commits?") {
                    commits_list_fixture()
                } else if path.contains("/commits/c111") {
                    commit_fixture("c111")
                } else if path.contains("/commits/c222") {
                    commit_fixture("c222")
                } else if path.contains("/commits/m333") {
                    commit_fixture("m333")
                } else if path.contains("/commits/c444") {
                    commit_fixture("c444")
                } else if path.contains("/contents/") && path.contains("ref=m333") {
                    contents_before_fixture()
                } else if path.contains("/contents/") {
                    contents_fixture()
                } else {
                    serde_json::json!({"message": "stub: unknown path"})
                };
                let body = serde_json::to_string(&body).expect("fixture json");
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn stub_client(base_url: &str) -> SourceControlClient<ReqwestTransport> {
    SourceControlClient::with_base_url(ReqwestTransport::new(), base_url)
}

#[tokio::test]
async fn diff_blame_ancestor_without_clone() {
    let base = stub_server().await;
    let client = stub_client(&base);

    // diff: 2 files, merge-base aaa111, ahead 1.
    let diff = client
        .diff("Amdkn", "Aspace_OS_V5", "aaa111", "c222")
        .await
        .expect("stub diff");
    assert_eq!(diff.merge_base, "aaa111");
    assert_eq!(diff.ahead_by, 1);
    assert_eq!(diff.behind_by, 0);
    assert_eq!(diff.files.len(), 2);
    assert_eq!(diff.files[0].filename, "hello.txt");
    assert_eq!(diff.files[0].additions, 1);
    assert_eq!(diff.files[0].deletions, 1);
    assert!(!diff.clone);
    assert_eq!(diff.served_by, "github-api");

    // ancestor: the same merge-base.
    let anc = client
        .ancestor("Amdkn", "Aspace_OS_V5", "aaa111", "c222")
        .await
        .expect("stub ancestor");
    assert_eq!(anc.merge_base, "aaa111");
    assert_eq!(anc.merge_base_message.as_deref(), Some("base"));
    assert!(!anc.clone);

    // blame: exact per-line attribution.
    let blame = client
        .blame("Amdkn", "Aspace_OS_V5", "hello.txt", "c222")
        .await
        .expect("stub blame");
    assert_eq!(blame.lines.len(), 3);
    assert_eq!(blame.lines[0].sha, "c111");
    assert_eq!(blame.lines[0].text, "one");
    assert_eq!(blame.lines[1].sha, "c222");
    assert_eq!(blame.lines[1].text, "TWO");
    assert_eq!(blame.lines[2].sha, "c111");
    assert_eq!(blame.lines[2].text, "three");
    assert_eq!(blame.commits, vec!["c111", "c222"]);
    assert_eq!(blame.method, "forward-application");
    assert!(!blame.clone);

    // The 3 pending queries answer structured-pending, never fake data.
    for (name, pas) in [
        ("receipt", "SC-3"),
        ("judgment", "SC-4"),
        ("holdout", "SC-5"),
    ] {
        let q = Query::parse(name).expect("test");
        assert!(!q.live_at_sc1());
        let p = client.pending(q).expect("pending answer");
        assert!(p.pending);
        assert_eq!(p.pas, pas);
        assert_eq!(p.query, name);
    }

    // Unknown query names are an error, not a guess.
    assert!(Query::parse("merge").is_err());
}

#[tokio::test]
async fn blame_merge_introduced_file() {
    // The file's creation commit is hidden by a merge: the
    // `commits?path=` list names the merge (no files entry) instead.
    // The replay seeds from the merge's content — attributed to the
    // merge — instead of refusing or misattributing.
    let base = stub_server().await;
    let client = stub_client(&base);
    let blame = client
        .blame("Amdkn", "Aspace_OS_V5", "hello-merge.txt", "c444")
        .await
        .expect("stub blame (merge-introduced)");
    assert_eq!(blame.lines.len(), 3);
    assert_eq!(blame.lines[0].sha, "m333");
    assert_eq!(blame.lines[0].text, "one");
    assert_eq!(blame.lines[1].sha, "c444");
    assert_eq!(blame.lines[1].text, "TWO");
    assert_eq!(blame.lines[2].sha, "m333");
    assert_eq!(blame.lines[2].text, "three");
    assert_eq!(blame.commits, vec!["m333", "c444"]);
    assert!(!blame.clone);
}

#[tokio::test]
async fn live_diff_blame_ancestor_without_clone() {
    // Env-gated: the default `cargo test` run stays offline. The SC-1
    // harness runs this with SC1_LIVE=1 and refs/path from the live repo.
    if std::env::var("SC1_LIVE").as_deref() != Ok("1") {
        return;
    }
    let base = std::env::var("SC1_LIVE_BASE").expect("SC1_LIVE_BASE");
    let head = std::env::var("SC1_LIVE_HEAD").expect("SC1_LIVE_HEAD");
    let path = std::env::var("SC1_LIVE_BLAME_PATH").expect("SC1_LIVE_BLAME_PATH");
    let client = SourceControlClient::with_base_url(ReqwestTransport::new(), GITHUB_API_BASE);

    let diff = client
        .diff("Amdkn", "Aspace_OS_V5", &base, &head)
        .await
        .expect("live diff");
    assert!(!diff.files.is_empty(), "live diff has no files");
    assert_eq!(
        diff.merge_base, base,
        "base is the direct parent, so it is the merge-base"
    );
    assert!(!diff.clone);

    let anc = client
        .ancestor("Amdkn", "Aspace_OS_V5", &base, &head)
        .await
        .expect("live ancestor");
    assert_eq!(anc.merge_base, base);
    assert!(!anc.clone);

    let blame = client
        .blame("Amdkn", "Aspace_OS_V5", &path, &head)
        .await
        .expect("live blame");
    assert!(!blame.lines.is_empty(), "live blame has no lines");
    assert!(!blame.commits.is_empty(), "live blame has no commits");
    assert!(!blame.clone);
    // Every attributed SHA must be one of the replayed commits.
    let known: HashMap<&str, ()> = blame.commits.iter().map(|s| (s.as_str(), ())).collect();
    for line in &blame.lines {
        assert!(
            known.contains_key(line.sha.as_str()),
            "blame line {} attributed to unknown sha {}",
            line.line,
            line.sha
        );
    }
}

use std::sync::Arc;
use std::{path::Path, process::Command};

use forgeclaw::grants::{GrantStore, SessionKey};
use forgeclaw::http_tools::ToolServer;
use forgeclaw_core::{RepoId, ScopedToken, Subject, ThreadKey};
use forgeclaw_forgejo::Forgejo;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn bridge(forge: &MockServer, grants: Arc<GrantStore>) -> (String, tempfile::TempDir) {
    bridge_at(forge, grants, forge.uri().parse().unwrap()).await
}

async fn bridge_at(
    forge: &MockServer,
    grants: Arc<GrantStore>,
    forge_url: Url,
) -> (String, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let adapter = Arc::new(
        Forgejo::new(
            forge.uri().parse().unwrap(),
            "read-token",
            Some("password".into()),
        )
        .unwrap(),
    );
    let app = ToolServer::new(
        forge_url,
        adapter,
        "read-token".into(),
        Some("Bearer bridge-secret".into()),
        grants,
        workspace.path().to_path_buf(),
    )
    .router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/tools/call"), workspace)
}

async fn call(
    url: &str,
    name: &str,
    arguments: Value,
    session: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Client::new()
        .post(url)
        .header("authorization", "Bearer bridge-secret")
        .json(&json!({"name": name, "arguments": arguments}));
    if let Some(session) = session {
        request = request.header("x-forgeclaw-session-key", session);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.json().await.unwrap())
}

async fn mock_temporary_tokens(forge: &MockServer, id: u64, count: u64) {
    Mock::given(method("POST"))
        .and(path("/api/v1/users/bot/tokens"))
        .and(header("authorization", "Basic Ym90OnBhc3N3b3Jk"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": id, "sha1": "scoped-token"
        })))
        .expect(count)
        .mount(forge)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v1/users/bot/tokens/{id}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(count)
        .mount(forge)
        .await;
}

#[tokio::test]
async fn bridge_rejects_unauthorized_requests_before_running_a_tool() {
    let forge = MockServer::start().await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let response = Client::new()
        .post(url)
        .json(&json!({"name": "forge_read", "arguments": {"subject": "o/r#issue/7"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "unauthorized"
    );
    assert!(forge.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bridge_reads_an_issue_without_a_write_grant() {
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "title": "Broken", "body": "Details", "state": "open",
            "user": {"login": "alice"}, "html_url": "https://example.test/o/r/issues/7"
        })))
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, body) = call(&url, "forge_read", json!({"subject": "o/r#issue/7"}), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let content: Value =
        serde_json::from_str(body["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(content["title"], "Broken");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_restricts_comments_to_the_granted_subject_and_scoped_token() {
    let forge = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .and(header("authorization", "token scoped-token"))
        .and(body_partial_json(json!({"body": "Ready"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 42})))
        .expect(1)
        .mount(&forge)
        .await;
    let grants = Arc::new(GrantStore::default());
    let _lease = grants.insert(
        SessionKey::new("Agent:Main:Turn"),
        ThreadKey {
            repo: "o/r".parse::<RepoId>().unwrap(),
            subject: Subject::Issue(7),
        },
        ScopedToken {
            id: 9,
            secret: "scoped-token".into(),
        },
    );
    let (url, _workspace) = bridge(&forge, grants.clone()).await;
    let arguments = json!({"subject": "o/r#issue/7", "body": "Ready"});
    let (status, body) = call(&url, "forge_comment", arguments.clone(), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "missing OpenClaw session identity");
    let (status, body) = call(
        &url,
        "forge_comment",
        json!({"subject": "o/r#issue/8", "body": "Ready"}),
        Some("agent:main:turn"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "write is not authorized for this subject");
    let (status, body) = call(&url, "forge_comment", arguments, Some("agent:main:turn")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content"][0]["text"], "posted comment #42");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_revokes_temporary_token_after_creating_issue() {
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "bot"})))
        .mount(&forge)
        .await;
    mock_temporary_tokens(&forge, 9, 1).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(header("authorization", "token scoped-token"))
        .and(body_partial_json(
            json!({"title": "New task", "body": "Details"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 11})))
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, body) = call(
        &url,
        "forge_create_issue",
        json!({"repo": "o/r", "title": "New task", "body": "Details"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content"][0]["text"], "opened issue #11");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_reads_large_body_by_offset() {
    let forge = MockServer::start().await;
    let body = "a".repeat(8 * 1024) + "tail";
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "body": body, "title": "Long issue"
        })))
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_read_body",
        json!({
            "subject": "o/r#issue/7", "offset": 8 * 1024
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let page: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["text"], "tail");
    assert!(page["next_offset"].is_null());
    forge.verify().await;
}

#[tokio::test]
async fn bridge_rejects_invalid_requests_before_minting_credentials() {
    let forge = MockServer::start().await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    for (name, arguments, error) in [
        (
            "forge_read_body",
            json!({"subject": "o/r#issue/7", "offset": -1}),
            "offset must be a non-negative integer",
        ),
        (
            "forge_submit_review",
            json!({"subject": "o/r#issue/7", "verdict": "approve", "summary": "LGTM"}),
            "reviews require a pull request subject",
        ),
        (
            "forge_edit_pr",
            json!({"subject": "o/r#issue/7", "updates": {"title": "x"}}),
            "editing requires a pull request subject",
        ),
        (
            "forge_create_pr",
            json!({"repo": "o/r", "title": "x", "body": "x", "branch": "--bad"}),
            "invalid branch",
        ),
    ] {
        let (status, response) = call(&url, name, arguments, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {response}");
        assert!(
            response["error"].as_str().unwrap().contains(error),
            "{name}: {response}"
        );
    }
    assert!(forge.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bridge_submits_review_only_with_matching_grant() {
    let forge = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls/5/reviews"))
        .and(header("authorization", "token scoped-token"))
        .and(body_partial_json(
            json!({"body": "LGTM", "event": "APPROVED", "comments": []}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 10})))
        .expect(1)
        .mount(&forge)
        .await;
    let grants = Arc::new(GrantStore::default());
    let _lease = grants.insert(
        SessionKey::new("review-session"),
        ThreadKey {
            repo: "o/r".parse().unwrap(),
            subject: Subject::Pr(5),
        },
        ScopedToken {
            id: 9,
            secret: "scoped-token".into(),
        },
    );
    let (url, _workspace) = bridge(&forge, grants.clone()).await;
    let (status, response) = call(
        &url,
        "forge_submit_review",
        json!({
            "subject": "o/r#pr/5", "verdict": "approve", "summary": "LGTM"
        }),
        Some("review-session"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["content"][0]["text"], "submitted review");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_creates_pull_request_from_bot_fork_and_revokes_token() {
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "bot"})))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"default_branch": "main"})))
        .expect(1)
        .mount(&forge)
        .await;
    mock_temporary_tokens(&forge, 19, 1).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls"))
        .and(header("authorization", "token scoped-token"))
        .and(body_partial_json(
            json!({"title": "Fix", "body": "Done", "head": "bot:fix", "base": "main"}),
        ))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 5, "requested_reviewers": []})),
        )
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_create_pr",
        json!({
            "repo": "o/r", "title": "Fix", "body": "Done", "branch": "fix"
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["content"][0]["text"], "opened PR #5");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_refuses_to_edit_pull_request_owned_by_another_user() {
    let forge = MockServer::start().await;
    mock_pr_context(&forge, "alice").await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_edit_pr",
        json!({
            "subject": "o/r#pr/5", "updates": {"title": "New title"}
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("owned by alice"),
        "{response}"
    );
    assert!(
        forge
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "POST"
                && request.method != "PATCH"
                && request.method != "DELETE")
    );
    forge.verify().await;
}

async fn mock_pr_context(forge: &MockServer, owner: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/5"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"number": 5, "title": "Fix"})),
        )
        .expect(1)
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/5/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 5,
            "requested_reviewers": [],
            "head": {"ref": "fix", "repo": {"name": "r", "owner": {"login": owner}, "full_name": format!("{owner}/r")}},
            "base": {"ref": "main"}
        })))
        .expect(1)
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "bot"})))
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflow_runs": []})))
        .mount(forge)
        .await;
}

#[tokio::test]
async fn bridge_edits_bot_owned_pull_request_and_revokes_token() {
    let forge = MockServer::start().await;
    mock_pr_context(&forge, "bot").await;
    mock_temporary_tokens(&forge, 21, 1).await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/pulls/5"))
        .and(header("authorization", "token scoped-token"))
        .and(body_partial_json(json!({"title": "New title"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 5, "requested_reviewers": []})),
        )
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_edit_pr",
        json!({
            "subject": "o/r#pr/5", "updates": {"title": "New title"}
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["content"][0]["text"], "updated PR #5");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_reads_an_older_comment_with_body_pagination() {
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "body": "first", "user": {"login": "alice"}},
            {"id": 2, "body": "x".repeat(9000), "user": {"login": "bob"}}
        ])))
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_read_comment",
        json!({
            "subject": "o/r#issue/7", "offset": 0, "body_offset": 8192
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let comment: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(comment["id"], 2);
    assert_eq!(comment["author"], "bob");
    assert_eq!(comment["body"].as_str().unwrap().len(), 808);
    assert_eq!(comment["next_offset"], 1);
    forge.verify().await;
}

#[tokio::test]
async fn bridge_searches_issues_with_fallback_when_forgejo_ignores_query() {
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(wiremock::matchers::query_param("q", "needle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"number": 3, "title": "Needle in title", "state": "open"},
            {"number": 4, "title": "Unrelated", "body": "haystack", "state": "closed"}
        ])))
        .expect(1)
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_search_issues",
        json!({"repo": "o/r", "query": "needle"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let issues: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(issues.as_array().unwrap().len(), 1);
    assert_eq!(issues[0]["number"], 3);
    forge.verify().await;
}

fn git(path: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(path)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn checkout_and_push_create_a_branch_but_reject_ungranted_updates() {
    let forge = MockServer::start().await;
    let git_root = tempfile::tempdir().unwrap();
    let bare_dir = git_root.path().join("bot");
    std::fs::create_dir(&bare_dir).unwrap();
    let bare = bare_dir.join("r.git");
    let seed = git_root.path().join("seed");
    git(git_root.path(), &["init", "--bare", bare.to_str().unwrap()]);
    git(
        git_root.path(),
        &["init", "-b", "main", seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "ForgeClaw test"]);
    git(&seed, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(seed.join("README.md"), "one\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "seed"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "origin", "main"]);
    git(
        git_root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
    );

    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "bot"})))
        .mount(&forge)
        .await;
    mock_temporary_tokens(&forge, 31, 3).await;
    let (url, workspace) = bridge_at(
        &forge,
        Arc::new(GrantStore::default()),
        Url::from_directory_path(git_root.path()).unwrap(),
    )
    .await;
    let subject = json!({"subject": "bot/r#issue/7"});
    let (status, response) = call(&url, "forge_checkout", subject, None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let checkout = workspace.path().join("bot/r/issue-7");
    assert_eq!(
        std::fs::read_to_string(checkout.join("README.md")).unwrap(),
        "one\n"
    );

    git(&checkout, &["checkout", "-b", "feature"]);
    std::fs::write(checkout.join("README.md"), "two\n").unwrap();
    git(&checkout, &["config", "user.name", "ForgeClaw test"]);
    git(&checkout, &["config", "user.email", "test@example.invalid"]);
    git(&checkout, &["commit", "-am", "feature"]);
    let arguments = json!({"subject": "bot/r#issue/7", "branch": "feature"});
    let (status, response) = call(&url, "forge_push", arguments.clone(), None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    git(
        git_root.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "show-ref",
            "--verify",
            "refs/heads/feature",
        ],
    );
    let (status, response) = call(&url, "forge_push", arguments, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"], "missing OpenClaw session identity");
    forge.verify().await;
}

#[tokio::test]
async fn bridge_reads_pull_request_diff_page_and_ci_context() {
    let forge = MockServer::start().await;
    let diff = "x".repeat(16 * 1024) + "tail";
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(diff))
        .expect(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "workflow_runs": [{"id": 8, "status": "failure", "workflow_id": "ci.yaml",
                "event_payload": "{\"pull_request\":{\"number\":5}}"}]
        })))
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/runs/8/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 9, "name": "test", "status": "failure"}
        ])))
        .mount(&forge)
        .await;
    let log = format!("{}failure at the end\n", "deps-tools\n".repeat(1100));
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/jobs/9/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(log.clone()))
        .mount(&forge)
        .await;
    let (url, _workspace) = bridge(&forge, Arc::new(GrantStore::default())).await;
    let (status, response) = call(
        &url,
        "forge_read_diff",
        json!({
            "subject": "o/r#pr/5", "offset": 16 * 1024
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let page: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["text"], "tail");
    assert!(page["next_offset"].is_null());
    let (status, response) =
        call(&url, "forge_read_ci", json!({"subject": "o/r#pr/5"}), None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let ci: Value = serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(ci["id"], 8);
    assert_eq!(ci["status"], "failure");
    assert_eq!(ci["jobs"][0]["id"], 9);
    let (status, response) = call(
        &url,
        "forge_read_ci",
        json!({"subject": "o/r#pr/5", "job_id": 9}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let first: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(first["text"].as_str().unwrap().len(), 8 * 1024);
    let (status, response) = call(
        &url,
        "forge_read_ci",
        json!({"subject": "o/r#pr/5", "job_id": 9, "offset": first["next_offset"]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let second: Value =
        serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        second["text"]
            .as_str()
            .unwrap()
            .contains("failure at the end")
    );
    assert!(second["next_offset"].is_null());
    assert_eq!(
        format!(
            "{}{}",
            first["text"].as_str().unwrap(),
            second["text"].as_str().unwrap()
        ),
        log
    );
    let (status, _) = call(
        &url,
        "forge_read_ci",
        json!({"subject": "o/r#pr/5", "job_id": 10}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    forge.verify().await;
}

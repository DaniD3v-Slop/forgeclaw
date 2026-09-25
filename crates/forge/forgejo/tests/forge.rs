use forgeclaw_core::{Forge, NewPr, RepoId, Subject, ThreadKey};
use forgeclaw_forgejo::Forgejo;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Forgejo {
    Forgejo::new(server.uri().parse().unwrap(), "test-token", None).unwrap()
}

fn token_client(server: &MockServer) -> Forgejo {
    Forgejo::new(
        server.uri().parse().unwrap(),
        "read-token",
        Some("test-password".into()),
    )
    .unwrap()
}

fn repo() -> RepoId {
    "o/r".parse().unwrap()
}

fn thread(subject: Subject) -> ThreadKey {
    ThreadKey {
        repo: repo(),
        subject,
    }
}

#[tokio::test]
async fn task_tokens_are_minted_and_revoked_with_basic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "bot"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/users/bot/tokens"))
        .and(wiremock::matchers::header(
            "authorization",
            "Basic Ym90OnRlc3QtcGFzc3dvcmQ=",
        ))
        .and(body_partial_json(json!({
            "name": "one-turn",
            "scopes": ["write:repository", "write:issue", "read:user"]
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 4,
            "name": "one-turn",
            "sha1": "scoped-token"
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/users/bot/tokens/4"))
        .and(wiremock::matchers::header(
            "authorization",
            "Basic Ym90OnRlc3QtcGFzc3dvcmQ=",
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let forge = token_client(&server);
    let token = forge.mint_token("one-turn").await.unwrap();
    assert_eq!(token.id, 4);
    forge.revoke_token(&token).await.unwrap();
}

async fn mock(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn reads_issue_context() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/7",
        200,
        json!({
            "number": 7,
            "title": "Broken thing",
            "body": "details",
            "state": "open",
            "user": {"login": "alice"},
            "html_url": "https://forge.example/o/r/issues/7"
        }),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/7/comments",
        200,
        json!([{"body": "more", "user": {"login": "bob"}}]),
    )
    .await;

    let context = client(&server)
        .context(&thread(Subject::Issue(7)))
        .await
        .unwrap();
    assert_eq!(context["title"], "Broken thing");
    assert_eq!(context["comments"][0]["author"], "bob");
}

#[tokio::test]
async fn creates_pull_request_from_bot_fork() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r",
        200,
        json!({"default_branch": "main"}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls"))
        .and(body_partial_json(json!({
            "head": "bot:feature",
            "base": "main",
            "title": "Fix it"
        })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 9, "requested_reviewers": []})),
        )
        .mount(&server)
        .await;

    let number = client(&server)
        .create_pr(
            &repo(),
            NewPr {
                title: "Fix it".into(),
                body: "Done".into(),
                branch: "feature".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(number, 9);
    let requests = server.received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|request| request.url.path() == "/api/v1/repos/o/r/pulls")
        .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body.get("assignees").is_none_or(|assignees| {
        assignees.is_null() || assignees.as_array().is_some_and(Vec::is_empty)
    }));
}

#[tokio::test]
async fn creates_issue_in_selected_repository() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(body_partial_json(
            json!({"title": "New task", "body": "Details"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 12})))
        .mount(&server)
        .await;

    assert_eq!(
        client(&server)
            .create_issue(&repo(), "New task", "Details")
            .await
            .unwrap(),
        12
    );
}

#[tokio::test]
async fn pull_request_context_exposes_head_ownership() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/7",
        200,
        json!({"number": 7, "title": "Change", "state": "open"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/7/comments",
        200,
        json!([]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/7",
        200,
        json!({
            "number": 7,
            "requested_reviewers": [],
            "head": {
                "ref": "feature",
                "repo": {"name": "r", "full_name": "alice/r", "owner": {"login": "alice"}}
            },
            "base": {"ref": "main"}
        }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/7.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff"))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"workflow_runs": [
            {"id": 41, "status": "success", "event_payload": "{\"pull_request\":{\"number\":7}}"},
            {"id": 42, "status": "running", "workflow_id": "ci.yaml",
             "event_payload": "{\"pull_request\":{\"number\":7}}"},
            {"id": 43, "status": "failure", "event_payload": "{\"pull_request\":{\"number\":8}}"}
        ]}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs/42/jobs",
        200,
        json!([{"id": 9, "name": "test", "status": "running"},
               {"id": 10, "name": "package", "status": "waiting"}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/jobs/9/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("compiling\n"))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/7/reviews",
        200,
        json!([{"id": 11, "body": "Review", "user": {"login": "bob"}}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/7/reviews/11/comments",
        200,
        json!([{"id": 42, "body": "Fixed?", "resolver": {"login": "alice"}, "user": {"login": "bob"}}]),
    )
    .await;

    let context = client(&server)
        .context(&thread(Subject::Pr(7)))
        .await
        .unwrap();
    assert_eq!(context["head_owner"], "alice");
    assert_eq!(context["head_repo"], "alice/r");
    assert_eq!(context["head_branch"], "feature");
    assert_eq!(context["ci_run"]["id"], 42);
    assert_eq!(context["ci_run"]["status"], "running");
    assert_eq!(context["ci_run"]["jobs"][0]["log"], "compiling\n");
    assert_eq!(context["ci_run"]["jobs"][1]["status"], "waiting");
    assert_eq!(context["reviews"][0]["comments"][0]["resolved"], true);
}

#[tokio::test]
async fn comment_is_scoped_to_the_named_subject() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .and(body_partial_json(json!({"body": "answer"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 42})))
        .mount(&server)
        .await;

    let id = client(&server)
        .comment(&thread(Subject::Issue(7)), "answer", None)
        .await
        .unwrap();
    assert_eq!(id, 42);
}

#[tokio::test]
async fn missing_comment_id_is_an_error() {
    let server = MockServer::start().await;
    mock(
        &server,
        "POST",
        "/api/v1/repos/o/r/issues/7/comments",
        201,
        json!({}),
    )
    .await;

    assert!(
        client(&server)
            .comment(&thread(Subject::Issue(7)), "answer", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn resolves_only_a_comment_on_the_named_pull_request() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/7/reviews",
        200,
        json!([{"id": 11}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/7/reviews/11/comments",
        200,
        json!([{"id": 42, "body": "fix this", "resolver": null}]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls/comments/42/resolve"))
        .and(wiremock::matchers::header(
            "authorization",
            "token test-token",
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let forge = client(&server);
    forge
        .resolve_review_comment(&thread(Subject::Pr(7)), 42)
        .await
        .unwrap();
    assert!(
        forge
            .resolve_review_comment(&thread(Subject::Pr(7)), 43)
            .await
            .is_err()
    );
    assert!(
        forge
            .resolve_review_comment(&thread(Subject::Issue(7)), 42)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn existing_fork_is_reused() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    mock(
        &server,
        "POST",
        "/api/v1/repos/o/r/forks",
        409,
        json!({"message": "already forked"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/forks",
        200,
        json!([{"name": "r", "owner": {"login": "bot"}}]),
    )
    .await;

    assert_eq!(
        client(&server).ensure_fork(&repo()).await.unwrap(),
        "bot/r".parse().unwrap()
    );
}

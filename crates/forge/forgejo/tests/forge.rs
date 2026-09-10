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

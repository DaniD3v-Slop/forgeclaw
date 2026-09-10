use forgeclaw_core::{
    Forge, ForgeEvent, InlineComment, NewPr, PrPatch, RepoId, Review, ScopedToken, Subject,
    ThreadKey, Verdict,
};
use forgeclaw_forgejo::Forgejo;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Forgejo {
    Forgejo::new(server.uri().parse().unwrap(), "testtoken", None).unwrap()
}

/// A client that can mint tokens: Forgejo's token API is basic-auth only, so
/// these paths use the bot password rather than the token.
fn client_pw(server: &MockServer) -> Forgejo {
    Forgejo::new(
        server.uri().parse().unwrap(),
        "testtoken",
        Some("botpass".into()),
    )
    .unwrap()
}

fn repo() -> RepoId {
    "o/r".parse().unwrap()
}

fn pr_thread(n: u64) -> ThreadKey {
    ThreadKey {
        repo: repo(),
        subject: Subject::Pr(n),
    }
}

async fn mock(server: &MockServer, meth: &str, p: &str, status: u16, body: Value) {
    Mock::given(method(meth))
        .and(path(p))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mock_pr_branches(server: &MockServer, number: u64) {
    mock(
        server,
        "GET",
        &format!("/api/v1/repos/o/r/pulls/{number}"),
        200,
        json!({
            "number": number,
            "head": {"ref": "feature"},
            "base": {"ref": "main"},
            "requested_reviewers": []
        }),
    )
    .await;
}

#[tokio::test]
async fn context_for_pr_has_comments_and_clipped_diff() {
    let server = MockServer::start().await;
    mock_pr_branches(&server, 5).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5/comments",
        200,
        json!([{"id": 1, "body": "looks fine", "user": {"login": "alice"}}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5",
        200,
        json!({
            "number": 5, "title": "Add thing", "body": "does thing", "state": "open",
            "user": {"login": "bot"}, "html_url": "https://git.example/o/r/pulls/5",
        }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("d".repeat(70_000)))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([]),
    )
    .await;

    let ctx = client(&server).context(&pr_thread(5)).await.unwrap();
    assert_eq!(ctx["title"], "Add thing");
    assert_eq!(ctx["author"], "bot");
    assert_eq!(ctx["state"], "open");
    assert_eq!(ctx["url"], "https://git.example/o/r/pulls/5");
    assert_eq!(ctx["head_branch"], "feature");
    assert_eq!(ctx["base_branch"], "main");
    assert_eq!(
        ctx["comments"],
        json!([{"author": "alice", "body": "looks fine"}])
    );
    assert_eq!(
        ctx["diff"].as_str().unwrap().len(),
        64 * 1024,
        "diff clipped to 64KB"
    );
}

#[tokio::test]
async fn context_for_pr_surfaces_reviews_with_inline_comments() {
    let server = MockServer::start().await;
    mock_pr_branches(&server, 5).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5",
        200,
        json!({"number": 5, "title": "t", "user": {"login": "bot"}}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5/comments",
        200,
        json!([]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff"))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([
            {"id": 9, "state": "REQUEST_CHANGES", "body": "please fix", "user": {"login": "alice"}},
            // No body and (below) no comments — dropped as noise.
            {"id": 10, "state": "APPROVED", "body": "", "user": {"login": "carol"}},
        ]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews/9/comments",
        200,
        json!([{"id": 1, "body": "drop this line", "path": "Containerfile",
                "position": 3, "user": {"login": "alice"}}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews/10/comments",
        200,
        json!([]),
    )
    .await;

    let ctx = client(&server).context(&pr_thread(5)).await.unwrap();
    let reviews = ctx["reviews"].as_array().unwrap();
    assert_eq!(
        reviews.len(),
        1,
        "the empty APPROVED review is dropped as noise"
    );
    let rev = &reviews[0];
    assert_eq!(rev["author"], "alice");
    assert_eq!(rev["body"], "please fix");
    assert!(
        rev["state"]
            .as_str()
            .unwrap()
            .to_uppercase()
            .contains("REQUEST"),
        "review state surfaced: {}",
        rev["state"]
    );
    assert_eq!(
        rev["comments"],
        json!([{"id": 1, "author": "alice", "path": "Containerfile", "line": 3, "body": "drop this line"}])
    );
}

#[tokio::test]
async fn context_for_pr_with_failed_run_carries_the_log_tail() {
    let server = MockServer::start().await;
    mock_pr_branches(&server, 5).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5",
        200,
        json!({"number": 5, "title": "t", "user": {"login": "bot"}}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/5/comments",
        200,
        json!([]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/5.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff"))
        .mount(&server)
        .await;

    let run_payload = json!({"pull_request": {"number": 5, "user": {"login": "bot"}}});
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"total_count": 2, "workflow_runs": [
            {"id": 41, "status": "success", "event_payload": run_payload.to_string()},
            {"id": 42, "status": "failure", "event_payload": run_payload.to_string()},
        ]}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs/42/jobs",
        200,
        json!([
            {"id": 7, "status": "success"},
            {"id": 8, "status": "failure"},
        ]),
    )
    .await;
    let log = format!(
        "{}error[E0308]: mismatched types",
        "log line\n".repeat(8_000)
    );
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/actions/jobs/8/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(&log))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([]),
    )
    .await;

    let ctx = client(&server).context(&pr_thread(5)).await.unwrap();
    let ci_log = ctx["ci_log"].as_str().unwrap();
    assert_eq!(ci_log.len(), 64 * 1024, "log clipped to 64KB");
    assert!(
        ci_log.ends_with("error[E0308]: mismatched types"),
        "tail kept, head dropped"
    );
}

#[tokio::test]
async fn repo_config_decodes_content_and_handles_absence() {
    let server = MockServer::start().await;
    let content = "[sandbox]\nnested = false\n";
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/contents/.forgebot.toml",
        200,
        json!({
            "type": "file", "name": ".forgebot.toml", "encoding": "base64",
            "content": base64_of(content), "sha": "abc123",
        }),
    )
    .await;
    let cfg = client(&server).repo_config(&repo()).await.unwrap();
    assert_eq!(cfg, Some((content.into(), "abc123".into())));

    let empty = MockServer::start().await;
    mock(
        &empty,
        "GET",
        "/api/v1/repos/o/r/contents/.forgebot.toml",
        404,
        json!({
            "message": "GetContentsOrList", "errors": [], "url": "https://git.example/api/swagger",
        }),
    )
    .await;
    assert_eq!(client(&empty).repo_config(&repo()).await.unwrap(), None);
}

#[tokio::test]
async fn repo_file_reads_an_arbitrary_path_with_its_sha() {
    // The client percent-encodes the path's `/`, so match by suffix regex.
    let server = MockServer::start().await;
    let dockerfile = "FROM base\nRUN dnf install -y rust\n";
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"contents/.*Containerfile$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "file", "name": "Containerfile", "encoding": "base64",
            "content": base64_of(dockerfile), "sha": "deadbeef",
        })))
        .mount(&server)
        .await;
    let got = client(&server)
        .repo_file(&repo(), ".forgebot/Containerfile")
        .await
        .unwrap();
    assert_eq!(got, Some((dockerfile.into(), "deadbeef".into())));

    let empty = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"contents/.*Containerfile$"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"message": "GetContentsOrList", "errors": [], "url": "x"})),
        )
        .mount(&empty)
        .await;
    assert_eq!(
        client(&empty)
            .repo_file(&repo(), ".forgebot/Containerfile")
            .await
            .unwrap(),
        None
    );
}

fn base64_of(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s)
}

#[tokio::test]
async fn create_pr_on_own_repo_defaults_base_and_keeps_bare_head() {
    let server = MockServer::start().await;
    // Bot owns `o/r`, so the head stays the bare branch.
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "o"})).await;
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
            "title": "Add thing", "body": "does thing", "head": "feat/thing", "base": "main",
            "assignees": ["o"],
        })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 9, "requested_reviewers": null})),
        )
        .mount(&server)
        .await;

    let pr = NewPr {
        title: "Add thing".into(),
        body: "does thing".into(),
        branch: "feat/thing".into(),
    };
    assert_eq!(client(&server).create_pr(&repo(), pr).await.unwrap(), 9);
}

#[tokio::test]
async fn create_pr_on_foreign_repo_qualifies_head_with_bot_owner() {
    let server = MockServer::start().await;
    // Bot `bot` does not own `o/r`; the head must name the fork's branch as
    // `bot:feat/thing` (Forgejo's cross-repo head form).
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
        .and(body_partial_json(
            json!({"head": "bot:feat/thing", "base": "main", "assignees": ["bot"]}),
        ))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 9, "requested_reviewers": null})),
        )
        .mount(&server)
        .await;

    let pr = NewPr {
        title: "Add thing".into(),
        body: "does thing".into(),
        branch: "feat/thing".into(),
    };
    assert_eq!(client(&server).create_pr(&repo(), pr).await.unwrap(), 9);
}

#[tokio::test]
async fn ensure_fork_is_a_noop_when_the_bot_owns_the_repo() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "o"})).await;
    // Bot owns `o/r`: no fork endpoint is touched.
    assert_eq!(client(&server).ensure_fork(&repo()).await.unwrap(), repo());
}

#[tokio::test]
async fn ensure_fork_creates_and_trusts_the_returned_repo() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    // Forgejo renamed the fork on collision; the returned object is trusted
    // over an assumed `bot/r`.
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/forks"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "name": "r-1", "owner": {"login": "bot"},
        })))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server).ensure_fork(&repo()).await.unwrap(),
        "bot/r-1".parse().unwrap()
    );
}

#[tokio::test]
async fn ensure_fork_resolves_existing_fork_on_conflict() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/forks"))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(json!({"message": "already forked"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/forks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "r", "owner": {"login": "someone-else"}},
            {"name": "r-mine", "owner": {"login": "bot"}},
        ])))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server).ensure_fork(&repo()).await.unwrap(),
        "bot/r-mine".parse().unwrap()
    );
}

#[tokio::test]
async fn submit_review_posts_one_review_with_inline_comments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls/5/reviews"))
        .and(body_partial_json(json!({
            "event": "REQUEST_CHANGES", "body": "needs work",
            "comments": [{"path": "src/lib.rs", "new_position": 14, "body": "off by one"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 77})))
        .mount(&server)
        .await;

    let review = Review {
        verdict: Verdict::RequestChanges,
        summary: "needs work".into(),
        inline: vec![InlineComment {
            path: "src/lib.rs".into(),
            line: 14,
            body: "off by one".into(),
        }],
    };
    client(&server)
        .submit_review(&repo(), 5, review)
        .await
        .unwrap();
}

#[tokio::test]
async fn comment_with_reply_to_lands_in_the_review_thread() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([{"id": 60, "user": {"login": "alice"}}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews/60/comments",
        200,
        json!([{"id": 33, "path": "src/lib.rs", "position": 14, "body": "why?"}]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/pulls/5/reviews/60/comments"))
        .and(body_partial_json(json!({
            "body": "because", "path": "src/lib.rs", "new_position": 14,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 44})))
        .mount(&server)
        .await;

    let id = client(&server)
        .comment(&pr_thread(5), "because", Some(33))
        .await
        .unwrap();
    assert_eq!(id, 44);
}

#[tokio::test]
async fn plain_comment_posts_on_the_issue() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .and(body_partial_json(json!({"body": "done"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 5})))
        .mount(&server)
        .await;
    let thread = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(7),
    };
    assert_eq!(
        client(&server)
            .comment(&thread, "done", None)
            .await
            .unwrap(),
        5
    );
}

#[tokio::test]
async fn resync_full_feeds_synthesize_events_shared_with_the_targeted_pass() {
    let server = MockServer::start().await;
    let base = server.uri();

    mock(
        &server,
        "GET",
        "/api/v1/notifications",
        200,
        json!([{
            "id": 12, "unread": true,
            "repository": {"full_name": "o/r"},
            "subject": {
                "type": "Issue", "title": "t",
                "url": format!("{base}/api/v1/repos/o/r/issues/3"),
                "latest_comment_url": format!("{base}/api/v1/repos/o/r/issues/comments/77"),
            },
        }]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/comments/77",
        200,
        json!({"id": 77, "body": "@bot ping", "user": {"login": "alice"},
               "html_url": "https://git.example/o/r/issues/3#issuecomment-77"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "assignees": [{"login": "bot"}]}),
    )
    .await;

    let issue3 = json!({
        "number": 3, "title": "Fix login", "body": "it breaks", "state": "open",
        "user": {"login": "bot"},
        "repository": {"full_name": "o/r", "name": "r", "owner": "o"},
    });
    let search = |p: &str, v: &str, body: Value| {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/issues/search"))
            .and(query_param(p, v))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    };
    search("assigned", "true", json!([issue3]))
        .mount(&server)
        .await;
    search(
        "review_requested",
        "true",
        json!([{
            "number": 5, "title": "Refactor", "user": {"login": "alice"},
            "repository": {"full_name": "o/r"},
        }]),
    )
    .mount(&server)
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "number": 8, "title": "My PR", "user": {"login": "bot"},
            "repository": {"full_name": "o/r"},
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue3])))
        .mount(&server)
        .await;
    // No body-mention issues here; that feed is exercised on its own below.
    search("mentioned", "true", json!([])).mount(&server).await;

    let run_payload = json!({"pull_request": {"number": 8, "user": {"login": "bot"}}});
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"total_count": 2, "workflow_runs": [
            {"id": 100, "status": "success", "event_payload": run_payload.to_string()},
            {"id": 101, "status": "failure", "workflow_id": "ci.yml",
             "event_payload": run_payload.to_string(),
             "html_url": "https://git.example/o/r/actions/runs/101"},
        ]}),
    )
    .await;
    // The authored-PR feed now also back-fills review comments; PR 8 has none.
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/8/reviews",
        200,
        json!([]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/8/dependencies",
        200,
        json!([]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3/timeline",
        200,
        json!([
            {"id": 900, "type": "comment", "body": "unrelated", "user": {"login": "alice"}},
            {"id": 901, "type": "pull", "user": {"login": "alice"},
             "ref_issue": {"number": 4, "title": "Fix it",
                           "html_url": "https://git.example/o/r/pulls/4",
                           "pull_request": {"merged": true}}},
        ]),
    )
    .await;

    let events = client(&server).resync("bot").await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "77",
            "assigned-3-bot",
            "revreq-5-bot",
            "run-101",
            "prmerged-4-3"
        ]
    );

    // Resync and the webhook path share one builder per kind, so the full
    // (kind, subject, payload) is pinned here — anything drifting between the
    // two paths breaks the idempotency key.
    let expect = |i: usize, kind: &str, subject, payload: Value| {
        let ev = &events[i];
        assert_eq!(ev.kind, kind);
        assert_eq!(ev.subject, subject);
        assert_eq!(ev.payload, payload);
    };
    expect(
        0,
        "comment.created",
        Subject::Issue(3),
        json!({"author": "alice", "body": "@bot ping", "mentions": ["bot"], "assignees": ["bot"],
               "url": "https://git.example/o/r/issues/3#issuecomment-77", "_notification": 12}),
    );
    expect(
        1,
        "issue.assigned",
        Subject::Issue(3),
        json!({"assignee": "bot", "author": "bot", "title": "Fix login", "body": "it breaks"}),
    );
    expect(
        2,
        "pull_request.review_requested",
        Subject::Pr(5),
        json!({"reviewer": "bot", "author": "alice", "title": "Refactor"}),
    );
    expect(
        3,
        "ci.run_completed",
        Subject::Pr(8),
        json!({"conclusion": "failure", "pr_author": "bot", "pr": 8,
               "workflow": "ci.yml", "run_url": "https://git.example/o/r/actions/runs/101"}),
    );
    expect(
        4,
        "issue.referenced_pr_merged",
        Subject::Issue(3),
        json!({"issue_author": "bot", "pr": 4, "pr_title": "Fix it",
               "pr_url": "https://git.example/o/r/pulls/4"}),
    );
}

/// A minimal full resync whose only created bot PR is #3 ("Add memnix"), with
/// its issue-dependency list set to `deps`. Returns the synthesized events.
async fn resync_with_deps(deps: Value) -> Vec<ForgeEvent> {
    let server = MockServer::start().await;

    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    let search = |p: &str, v: &str, body: Value| {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/issues/search"))
            .and(query_param(p, v))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    };
    search("assigned", "true", json!([])).mount(&server).await;
    search("review_requested", "true", json!([]))
        .mount(&server)
        .await;
    search("mentioned", "true", json!([])).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "number": 3, "title": "Add memnix", "user": {"login": "bot"},
            "repository": {"full_name": "o/r"},
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"total_count": 0, "workflow_runs": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/3/reviews",
        200,
        json!([]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3/dependencies",
        200,
        deps,
    )
    .await;

    client(&server).resync("bot").await.unwrap()
}

#[tokio::test]
async fn resync_resumes_a_pr_once_its_only_blocker_is_closed() {
    let events = resync_with_deps(json!([{"number": 2, "state": "closed"}])).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "pull_request.unblocked");
    assert_eq!(events[0].subject, Subject::Pr(3));
    assert_eq!(events[0].event_id, "unblocked-3");
    assert_eq!(
        events[0].payload,
        json!({"author": "bot", "title": "Add memnix", "pr": 3})
    );
}

#[tokio::test]
async fn resync_leaves_a_pr_blocked_while_a_dependency_is_open() {
    let events = resync_with_deps(json!([{"number": 2, "state": "open"}])).await;
    let unblocked: Vec<&ForgeEvent> = events
        .iter()
        .filter(|e| e.kind == "pull_request.unblocked")
        .collect();
    assert!(unblocked.is_empty());
}

#[tokio::test]
async fn resync_does_not_unblock_a_pr_without_dependencies() {
    let events = resync_with_deps(json!([])).await;
    let unblocked: Vec<&ForgeEvent> = events
        .iter()
        .filter(|e| e.kind == "pull_request.unblocked")
        .collect();
    assert!(unblocked.is_empty());
}

#[tokio::test]
async fn full_resync_recovers_a_body_mention_missed_by_the_poke() {
    // An open issue whose body `@`-mentions the bot — unassigned, no comment —
    // is enqueued only by the targeted `resync_thread`. If that live poke is
    // missed, the full resync's `mentioned=true` feed is the sole recovery, so
    // here it must surface the body under the stable `body` key with no poke.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    let search = |p: &str, v: &str, body: Value| {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/issues/search"))
            .and(query_param(p, v))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    };
    search("assigned", "true", json!([])).mount(&server).await;
    search("review_requested", "true", json!([]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // The only feed that fires: an unassigned open issue mentioning the bot.
    search(
        "mentioned",
        "true",
        json!([{
            "number": 8, "title": "Toolchain?", "body": "@bot do you have rust?",
            "state": "open", "user": {"login": "alice"}, "assignees": [],
            "repository": {"full_name": "o/r"},
        }]),
    )
    .mount(&server)
    .await;

    let events = client(&server).resync("bot").await.unwrap();
    assert_eq!(events.len(), 1, "only the recovered body mention");
    let ev = &events[0];
    assert_eq!(ev.kind, "comment.created");
    assert_eq!(ev.event_id, "body");
    assert_eq!(ev.subject, Subject::Issue(8));
    assert_eq!(ev.payload["author"], "alice");
    assert_eq!(ev.payload["mentions"], json!(["bot"]));
    assert_eq!(ev.payload["assignees"], json!([]));
}

#[tokio::test]
async fn ack_marks_the_notification_read() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/notifications/threads/12"))
        .respond_with(ResponseTemplate::new(205).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let with_note = forgeclaw_core::ForgeEvent {
        repo: repo(),
        kind: "comment.created".into(),
        subject: Subject::Issue(3),
        actor: "alice".into(),
        event_id: "77".into(),
        payload: json!({"_notification": 12}),
    };
    let forge = client(&server);
    forge.ack(&with_note).await.unwrap();

    let webhook_event = forgeclaw_core::ForgeEvent {
        payload: json!({}),
        ..with_note
    };
    forge.ack(&webhook_event).await.unwrap();
}

#[tokio::test]
async fn resync_thread_issue_derives_comment_assignment_and_referenced_merge() {
    // The targeted pass for one issue: the notification cursor is filtered to
    // this thread, and the issue's own state yields the assignment and the
    // merged cross-referencing PR — the resync-equivalent kinds, scoped to id.
    let server = MockServer::start().await;
    let base = server.uri();
    mock(
        &server,
        "GET",
        "/api/v1/notifications",
        200,
        json!([
            {"id": 12, "unread": true, "repository": {"full_name": "o/r"},
             "subject": {"type": "Issue", "title": "t",
                 "url": format!("{base}/api/v1/repos/o/r/issues/3"),
                 "latest_comment_url": format!("{base}/api/v1/repos/o/r/issues/comments/77")}},
            // A note on a different thread must be filtered out.
            {"id": 13, "unread": true, "repository": {"full_name": "o/r"},
             "subject": {"type": "Issue", "title": "other",
                 "url": format!("{base}/api/v1/repos/o/r/issues/9"),
                 "latest_comment_url": format!("{base}/api/v1/repos/o/r/issues/comments/78")}},
        ]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/comments/77",
        200,
        json!({"id": 77, "body": "@bot ping", "user": {"login": "alice"},
               "html_url": "https://git.example/o/r/issues/3#issuecomment-77"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "title": "Fix login", "body": "b", "state": "open",
               "user": {"login": "bot"}, "assignees": [{"login": "bot"}]}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3/timeline",
        200,
        json!([{"id": 901, "type": "pull", "user": {"login": "alice"},
                "ref_issue": {"number": 4, "title": "Fix it",
                              "html_url": "https://git.example/o/r/pulls/4",
                              "pull_request": {"merged": true}}}]),
    )
    .await;

    let thread = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(3),
    };
    let events = client(&server).resync_thread("bot", &thread).await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(
        ids,
        ["77", "assigned-3-bot", "prmerged-4-3"],
        "only this thread's notification, plus assignment and referenced merge"
    );
}

#[tokio::test]
async fn resync_thread_pr_derives_review_request_and_ci_failure() {
    // The targeted pass for one PR: a review request of the bot and, since the
    // bot authored it, its latest failed CI run.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5",
        200,
        json!({"number": 5, "title": "Refactor", "body": "b", "state": "open",
               "user": {"login": "bot"}, "requested_reviewers": [{"login": "bot"}]}),
    )
    .await;
    // No inline review replies await the bot on this PR.
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([]),
    )
    .await;
    let run_payload = json!({"pull_request": {"number": 5, "user": {"login": "bot"}}});
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"total_count": 1, "workflow_runs": [
            {"id": 101, "status": "failure", "workflow_id": "ci.yml",
             "event_payload": run_payload.to_string(),
             "html_url": "https://git.example/o/r/actions/runs/101"}]}),
    )
    .await;

    let events = client(&server)
        .resync_thread("bot", &pr_thread(5))
        .await
        .unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    // review request, CI failure, plus the bot-authored PR's own opened event
    // (auto-review). The order follows resync_thread's derivation.
    assert_eq!(ids, ["revreq-5-bot", "run-101", "opened-5"]);
    assert_eq!(events[0].subject, Subject::Pr(5));
    assert_eq!(events[1].kind, "ci.run_completed");
    assert_eq!(events[2].kind, "pull_request.opened");
}

#[tokio::test]
async fn resync_thread_pr_closed_merged_only_retires() {
    // A merged PR yields exactly its retirement — never a re-`opened` (which
    // would re-review it), review request, or CI event.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5",
        200,
        json!({"number": 5, "title": "t", "body": "b", "state": "closed", "merged": true,
               "user": {"login": "bot"}, "requested_reviewers": [{"login": "bot"}]}),
    )
    .await;
    let events = client(&server)
        .resync_thread("bot", &pr_thread(5))
        .await
        .unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, ["closed-5"], "a merged PR only retires");
    assert_eq!(events[0].kind, "pull_request.closed");
    assert_eq!(events[0].payload, json!({"merged": true}));
}

#[tokio::test]
async fn closed_event_reports_closed_subjects_only() {
    // The retirement correctness-net's cheap single fetch: `Some(*.closed)` for
    // a closed issue and a closed+merged PR, `None` for an open subject.
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "state": "closed", "user": {"login": "alice"}}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5",
        200,
        json!({"number": 5, "state": "closed", "merged": true,
               "user": {"login": "bot"}, "requested_reviewers": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/9",
        200,
        json!({"number": 9, "state": "open", "user": {"login": "alice"}}),
    )
    .await;

    let forge = client(&server);
    let issue = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(3),
    };
    let closed = forge.closed_event(&issue).await.unwrap().unwrap();
    assert_eq!(closed.kind, "issue.closed");
    assert_eq!(closed.event_id, "closed-3");
    assert_eq!(closed.payload, json!({"merged": false}));

    let pr = forge.closed_event(&pr_thread(5)).await.unwrap().unwrap();
    assert_eq!(pr.kind, "pull_request.closed");
    assert_eq!(pr.event_id, "closed-5");
    assert_eq!(pr.payload, json!({"merged": true}));

    let open = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(9),
    };
    assert!(forge.closed_event(&open).await.unwrap().is_none());
}

#[tokio::test]
async fn resync_thread_issue_closed_emits_closed() {
    // A closed issue only retires — even when still assigned to the bot, the
    // full resync's `state=Open` search would not re-surface it.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "title": "t", "body": "b", "state": "closed",
               "user": {"login": "bot"}, "assignees": [{"login": "bot"}]}),
    )
    .await;
    let thread = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(3),
    };
    let events = client(&server).resync_thread("bot", &thread).await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, ["closed-3"], "closed issue retires, no assignment");
    assert_eq!(events[0].kind, "issue.closed");
}

#[tokio::test]
async fn resync_thread_body_mention_fires_for_issue_and_pr() {
    // An `@bot` in an open issue/PR body rides the comment pipeline under the
    // stable `body` key; a body without a mention yields nothing.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "title": "t", "body": "hey @bot take this", "state": "open",
               "user": {"login": "alice"}, "assignees": [{"login": "carol"}]}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3/timeline",
        200,
        json!([]),
    )
    .await;
    let issue = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(3),
    };
    let events = client(&server).resync_thread("bot", &issue).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "comment.created");
    assert_eq!(events[0].event_id, "body");
    assert_eq!(events[0].payload["mentions"], json!(["bot"]));
    assert_eq!(events[0].payload["assignees"], json!(["carol"]));
    assert_eq!(events[0].payload["author"], "alice");

    // A PR whose body mentions the bot: same `body` event.
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/6",
        200,
        json!({"number": 6, "title": "t", "body": "@bot review approach", "state": "open",
               "user": {"login": "eve"}, "requested_reviewers": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/6/reviews",
        200,
        json!([]),
    )
    .await;
    let events = client(&server)
        .resync_thread("bot", &pr_thread(6))
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["body"],
        "PR body mention, no review/CI/opened for a non-bot-authored PR"
    );
    assert_eq!(events[0].subject, Subject::Pr(6));

    // A body without a mention yields nothing.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/6",
        200,
        json!({"number": 6, "title": "t", "body": "no mention here", "state": "open",
               "user": {"login": "eve"}, "requested_reviewers": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/6/reviews",
        200,
        json!([]),
    )
    .await;
    let events = client(&server)
        .resync_thread("bot", &pr_thread(6))
        .await
        .unwrap();
    assert!(events.is_empty(), "no mention, no event");
}

#[tokio::test]
async fn resync_thread_pr_inline_review_comment_yields_reply_to() {
    // An inline review comment authored by someone else that mentions the bot
    // comes back with `reply_to` set; the bot's own comment and a
    // non-mentioning one are excluded.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5",
        200,
        json!({"number": 5, "title": "t", "body": "b", "state": "open",
               "user": {"login": "alice"}, "requested_reviewers": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews",
        200,
        json!([{"id": 60, "user": {"login": "alice"}}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5/reviews/60/comments",
        200,
        json!([
            {"id": 33, "body": "@bot why this?", "user": {"login": "alice"},
             "path": "src/lib.rs", "position": 14,
             "html_url": "https://git.example/o/r/pulls/5#discussion-33"},
            {"id": 34, "body": "no mention", "user": {"login": "alice"}},
            {"id": 35, "body": "@bot self note", "user": {"login": "bot"}},
        ]),
    )
    .await;

    let events = client(&server)
        .resync_thread("bot", &pr_thread(5))
        .await
        .unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, ["33"], "only the not-bot mentioning comment");
    assert_eq!(events[0].kind, "comment.created");
    assert_eq!(events[0].payload["reply_to"], 33);
    assert_eq!(events[0].payload["mentions"], json!(["bot"]));
}

#[tokio::test]
async fn resync_thread_issue_comment_still_needs_the_cursor() {
    // An issue COMMENT (not the body) surfaces only via the notification
    // cursor. With an empty cursor, a mentioning comment on the issue is not
    // re-derived — the genuine comment race the periodic full resync catches.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3",
        200,
        json!({"number": 3, "title": "t", "body": "no mention in body", "state": "open",
               "user": {"login": "alice"}}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/3/timeline",
        200,
        json!([]),
    )
    .await;
    let thread = ThreadKey {
        repo: repo(),
        subject: Subject::Issue(3),
    };
    let events = client(&server).resync_thread("bot", &thread).await.unwrap();
    assert!(
        events.is_empty(),
        "an issue comment isn't re-derived without the notification cursor"
    );
}

#[tokio::test]
async fn whoami_returns_current_login() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    assert_eq!(client(&server).whoami().await.unwrap(), "bot");
}

#[tokio::test]
async fn whoami_without_login_errors() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"id": 1})).await;
    assert!(client(&server).whoami().await.is_err());
}

#[tokio::test]
async fn search_issues_maps_state_and_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(query_param("q", "login bug"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"number": 1, "title": "open one", "state": "open",
             "html_url": "https://git.example/o/r/issues/1"},
            {"number": 2, "title": "closed one", "state": "closed",
             "html_url": "https://git.example/o/r/issues/2"},
        ])))
        .mount(&server)
        .await;

    let hits = client(&server)
        .search_issues(&repo(), "login bug")
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!((hits[0].number, hits[0].state.as_str()), (1, "open"));
    assert_eq!(hits[0].url, "https://git.example/o/r/issues/1");
    assert_eq!((hits[1].number, hits[1].state.as_str()), (2, "closed"));
}

#[tokio::test]
async fn create_issue_returns_new_number() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(body_partial_json(
            json!({"title": "bug", "body": "it broke"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 42})))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server)
            .create_issue(&repo(), "bug", "it broke")
            .await
            .unwrap(),
        42
    );
}

#[tokio::test]
async fn add_dependency_posts_the_blocker_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues/8/dependencies"))
        .and(body_partial_json(json!({"index": 7})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 8})))
        .mount(&server)
        .await;
    client(&server).add_dependency(&repo(), 8, 7).await.unwrap();
}

#[tokio::test]
async fn close_issue_with_comment_posts_then_closes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/issues/7/comments"))
        .and(body_partial_json(json!({"body": "fixed in #9"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/issues/7"))
        .and(body_partial_json(json!({"state": "closed"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 7})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .close_issue(&repo(), 7, Some("fixed in #9"))
        .await
        .unwrap();
}

#[tokio::test]
async fn close_issue_without_comment_just_closes() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/issues/7"))
        .and(body_partial_json(json!({"state": "closed"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 7})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server).close_issue(&repo(), 7, None).await.unwrap();
}

#[tokio::test]
async fn edit_comment_patches_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/issues/comments/33"))
        .and(body_partial_json(json!({"body": "edited"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 33})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .edit_comment(&repo(), 33, "edited")
        .await
        .unwrap();
}

#[tokio::test]
async fn update_pr_sets_title_and_body_without_draft() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/pulls/5"))
        .and(body_partial_json(
            json!({"title": "New title", "body": "New body"}),
        ))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 5, "requested_reviewers": null})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let patch = PrPatch {
        title: Some("New title".into()),
        body: Some("New body".into()),
        draft: None,
    };
    client(&server).update_pr(&repo(), 5, patch).await.unwrap();
}

#[tokio::test]
async fn update_pr_marking_draft_fetches_current_title_and_prefixes_wip() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/5",
        200,
        json!({"number": 5, "title": "Add feature", "requested_reviewers": null}),
    )
    .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/pulls/5"))
        .and(body_partial_json(json!({"title": "WIP: Add feature"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 5, "requested_reviewers": null})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let patch = PrPatch {
        title: None,
        body: None,
        draft: Some(true),
    };
    client(&server).update_pr(&repo(), 5, patch).await.unwrap();
}

#[tokio::test]
async fn update_pr_clearing_draft_strips_the_wip_prefix_from_given_title() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/pulls/5"))
        .and(body_partial_json(json!({"title": "Ready now"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"number": 5, "requested_reviewers": null})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let patch = PrPatch {
        title: Some("WIP: Ready now".into()),
        body: None,
        draft: Some(false),
    };
    client(&server).update_pr(&repo(), 5, patch).await.unwrap();
}

#[tokio::test]
async fn mint_token_creates_scoped_token_then_revoke_deletes_by_id() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    // Forgejo's token API is basic-auth only; both calls must carry `Basic`,
    // never the `token` scheme the rest of the client uses.
    Mock::given(method("POST"))
        .and(path("/api/v1/users/bot/tokens"))
        .and(header_regex("authorization", "^Basic "))
        .and(body_partial_json(json!({
            "name": "task-1",
            "scopes": ["write:repository", "write:issue", "write:notification", "read:user"],
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 88, "sha1": "sekret"})))
        .expect(1)
        .mount(&server)
        .await;

    let token = client_pw(&server).mint_token("task-1").await.unwrap();
    assert_eq!(token.id, 88);
    assert_eq!(token.secret, "sekret");

    Mock::given(method("DELETE"))
        .and(path("/api/v1/users/bot/tokens/88"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    client_pw(&server).revoke_token(&token).await.unwrap();
}

#[tokio::test]
async fn mint_token_without_a_password_is_an_error() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    // No password configured → the token API is unreachable, caught before any
    // request rather than surfacing as a bare 401.
    assert!(client(&server).mint_token("task-1").await.is_err());
}

#[tokio::test]
async fn revoke_token_propagates_forge_errors() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/user", 200, json!({"login": "bot"})).await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/users/bot/tokens/88"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "nope"})))
        .mount(&server)
        .await;
    let token = ScopedToken {
        id: 88,
        secret: "x".into(),
    };
    assert!(client_pw(&server).revoke_token(&token).await.is_err());
}

#[tokio::test]
async fn merged_pr_notification_does_not_sink_the_reconcile() {
    // Forgejo sets subject.state="merged" on a merged PR's notification — a value
    // forgejo-api's two-variant StateType can't decode. The whole list must still
    // parse (state is unmodeled and ignored) and the comment event must surface.
    let server = MockServer::start().await;
    let base = server.uri();
    mock(
        &server,
        "GET",
        "/api/v1/notifications",
        200,
        json!([{
            "id": 9, "unread": true,
            "repository": {"full_name": "o/r"},
            "subject": {
                "type": "Pull", "title": "t", "state": "merged",
                "url": format!("{base}/api/v1/repos/o/r/pulls/4"),
                "latest_comment_url": format!("{base}/api/v1/repos/o/r/issues/comments/88"),
            },
        }]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/comments/88",
        200,
        json!({"id": 88, "body": "@bot go", "user": {"login": "alice"},
               "html_url": "https://git.example/o/r/pulls/4#issuecomment-88"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/4",
        200,
        json!({"number": 4, "assignees": []}),
    )
    .await;
    // The four search feeds return nothing; only the notification yields an event.
    mock(
        &server,
        "GET",
        "/api/v1/repos/issues/search",
        200,
        json!([]),
    )
    .await;

    let events = client(&server).resync("bot").await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].subject, Subject::Pr(4));
}

#[tokio::test]
async fn full_resync_backfills_authored_pr_review_comments() {
    // A request-changes review comment on the bot's own PR must be recovered by
    // the periodic full resync, not only the live pull_request_review webhook.
    let server = MockServer::start().await;
    mock(&server, "GET", "/api/v1/notifications", 200, json!([])).await;
    let search = |p: &str, v: &str, body: Value| {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/issues/search"))
            .and(query_param(p, v))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    };
    search("assigned", "true", json!([])).mount(&server).await;
    search("review_requested", "true", json!([]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "number": 8, "title": "My PR", "user": {"login": "bot"},
            "assignees": [{"login": "bot"}],
            "repository": {"full_name": "o/r"},
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/issues/search"))
        .and(query_param("created", "true"))
        .and(query_param("type", "issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    search("mentioned", "true", json!([])).mount(&server).await;
    // No CI runs -> ci_failed_event yields nothing.
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/actions/runs",
        200,
        json!({"total_count": 0, "workflow_runs": []}),
    )
    .await;
    // One review carrying one request-changes comment from the operator.
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/8/reviews",
        200,
        json!([{"id": 1}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/pulls/8/reviews/1/comments",
        200,
        json!([{"id": 55, "body": "please fix this", "user": {"login": "alice"},
                "html_url": "https://git.example/o/r/pulls/8#discussion-55"}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repos/o/r/issues/8/dependencies",
        200,
        json!([]),
    )
    .await;

    let events = client(&server).resync("bot").await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, "55");
    assert_eq!(events[0].subject, Subject::Pr(8));
    assert_eq!(events[0].kind, "comment.created");
    // The comment does not mention the bot; the assignees payload is what lets
    // the `reply` trigger fire on the assigned PR.
    assert_eq!(events[0].payload["assignees"], json!(["bot"]));
}

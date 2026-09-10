//! Forgejo backend: the `Forge` trait impl over `forgejo-api`, plus webhook
//! HMAC verification and thread extraction (`webhook` module).

mod webhook;

pub use webhook::webhook_thread;

use std::fmt::Display;
use std::future::IntoFuture;
use std::sync::OnceLock;

use async_trait::async_trait;
use base64::Engine as _;
use forgeclaw_core::{
    Error, Forge, ForgeEvent, IssueSummary, NewPr, PrPatch, RepoId, Result, Review, ScopedToken,
    Subject, ThreadKey, Verdict,
};
use forgejo_api::structs::{
    ActionRun, Issue, IssueListIssuesQuery, IssueSearchIssuesQuery, IssueSearchIssuesQueryState,
    IssueSearchIssuesQueryType, StateType, TimelineComment,
};
use forgejo_api::{ApiErrorKind, Auth, ForgejoError};
use serde_json::{Value, json};
use url::Url;

/// Cap on diff excerpts embedded into prompts.
const EXCERPT_MAX: usize = 64 * 1024;

/// Scopes for per-task minted tokens.
// `read:user` is required for `create_pr` (Forgejo resolves the head user);
// the writes cover branches/PRs, comments, and notification acks.
const TOKEN_SCOPES: &[&str] = &[
    "write:repository",
    "write:issue",
    "write:notification",
    "read:user",
];

pub struct Forgejo {
    api: forgejo_api::Forgejo,
    url: Url,
    /// The access token, kept so `repo_file` can reach the contents endpoint
    /// directly — the generated client double-encodes a `/` in the path.
    token: String,
    /// Bot account password, for the token-management endpoints only —
    /// Forgejo rejects token auth there ("auth method not allowed").
    password: Option<String>,
    /// Bot's own login, fetched once and reused by `whoami`, token ops, and
    /// fork/head qualification.
    me: OnceLock<String>,
}

impl Forgejo {
    pub fn new(url: Url, token: &str, password: Option<String>) -> Result<Self> {
        Ok(Self {
            api: forgejo_api::Forgejo::new(Auth::Token(token), url.clone()).map_err(err)?,
            url,
            token: token.into(),
            password,
            me: OnceLock::new(),
        })
    }

    /// A basic-auth client for `mint_token`/`revoke_token`: Forgejo's token
    /// API accepts HTTP Basic auth only, so an access token cannot mint or
    /// revoke tokens — the bot account password is required.
    async fn token_api(&self) -> Result<forgejo_api::Forgejo> {
        let me = self.me().await?;
        let password = self.password.as_deref().ok_or_else(|| {
            Error::Forge(
                "forge.password_env must be set to mint per-task tokens \
                 (Forgejo's token API rejects token auth)"
                    .into(),
            )
        })?;
        forgejo_api::Forgejo::new(
            Auth::Password {
                username: &me,
                password,
                mfa: None,
            },
            self.url.clone(),
        )
        .map_err(err)
    }

    /// The bot's login, cached after the first lookup.
    async fn me(&self) -> Result<String> {
        if let Some(me) = self.me.get() {
            return Ok(me.clone());
        }
        let login = go(self.api.user_get_current())
            .await?
            .login
            .ok_or_else(|| Error::Forge("current user has no login".into()))?;
        Ok(self.me.get_or_init(|| login).clone())
    }

    /// A comment-bearing unread notification as a `comment.created` event
    /// (same event_id as the webhook path); others are covered by the search
    /// feeds and skipped here.
    async fn notification_event(&self, note: &Notif) -> Option<ForgeEvent> {
        let subject = note.subject.as_ref()?;
        let repo: RepoId = note
            .repository
            .as_ref()?
            .full_name
            .as_deref()?
            .parse()
            .ok()?;
        let n = trailing_id(&subject.url)?;
        let subj = match subject.r#type.as_deref() {
            Some("Issue") => Subject::Issue(n),
            Some("Pull") => Subject::Pr(n),
            _ => return None,
        };
        let cid = trailing_id(&subject.latest_comment_url)?;
        let (o, r) = own(&repo);
        let c = self.api.issue_get_comment(o, r, cid as i64).await.ok()??;
        let author = login(&c.user);
        // Assignees ride the payload so the assignment reply trigger fires on
        // comments recovered here, not only live webhooks; a failed lookup
        // degrades to none rather than dropping the event.
        let assignees: Vec<String> = self
            .api
            .issue_get_issue(o, r, n as i64)
            .await
            .ok()
            .and_then(|i| i.assignees)
            .into_iter()
            .flatten()
            .filter_map(|u| u.login)
            .collect();
        Some(webhook::comment_created(
            &repo,
            subj,
            cid.to_string(),
            &author,
            &author,
            &text(&c.body),
            json!({"url": c.html_url, "_notification": note.id, "assignees": assignees}),
        ))
    }

    /// The PR's latest action run, if any.
    async fn latest_run(&self, repo: &RepoId, pr: u64) -> Option<ActionRun> {
        let (o, r) = own(repo);
        let runs = self
            .api
            .list_action_runs(o, r, Default::default())
            .await
            .ok()?;
        runs.workflow_runs
            .into_iter()
            .flatten()
            .filter(|run| {
                webhook::pr_in_payload(run.event_payload.as_deref().unwrap_or(""))
                    .is_some_and(|(n, _)| n == pr)
            })
            .max_by_key(|run| run.id)
    }

    /// Log tail of the first failed job when the PR's latest run failed —
    /// the `ci_log` context var.
    async fn failed_run_log(&self, repo: &RepoId, pr: u64) -> Option<String> {
        let run = self.latest_run(repo, pr).await?;
        if run.status.as_deref() != Some("failure") {
            return None;
        }
        let (o, r) = own(repo);
        let jobs = self.api.list_action_run_jobs(o, r, run.id?).await.ok()?;
        let job = jobs
            .iter()
            .find(|j| j.status.as_deref() == Some("failure"))?;
        let log = self
            .api
            .repo_get_action_job_logs(o, r, job.id?, Default::default())
            .await
            .ok()?;
        Some(clip_tail(log))
    }

    /// `issue.assigned` naming the bot — the resync analogue of the
    /// `issues.assigned` webhook. The full resync's `assigned=true` search
    /// already filters to the bot, so this is unconditional; the targeted pass
    /// gates it on [`assigned_to`] since it fetches the issue directly.
    fn assigned_event(&self, repo: &RepoId, i: &Issue, me: &str) -> ForgeEvent {
        let subject = match i.pull_request {
            Some(_) => Subject::Pr(num(i.number)),
            None => Subject::Issue(num(i.number)),
        };
        webhook::issue_assigned(
            repo,
            subject,
            me,
            &login(&i.user),
            &text(&i.title),
            &text(&i.body),
            me,
        )
    }

    /// A subject's own description as a `comment.created` (event_id `body`)
    /// when its body `@`-mentions the bot — the description-mention path the
    /// old webhook drove on issue/PR open/edit. `notification_event` can't
    /// recover this: a description carries no comment id.
    fn body_mention_event(
        &self,
        repo: &RepoId,
        subject: Subject,
        author: &str,
        body: &str,
        assignees: Vec<String>,
        me: &str,
    ) -> Option<ForgeEvent> {
        webhook::mentions(body).iter().any(|m| m == me).then(|| {
            webhook::comment_created(
                repo,
                subject,
                "body".into(),
                author,
                author,
                body,
                json!({"assignees": assignees}),
            )
        })
    }

    /// The subject's own `*.closed` retirement event when it is closed, derived
    /// from an already-fetched issue/PR. The `resync_thread` open path reuses the
    /// subject it fetches anyway; the retirement sweep goes through
    /// `closed_subject_event`, which fetches. Keeping the closed→retirement
    /// mapping here means neither caller re-derives it.
    fn issue_retire(&self, thread: &ThreadKey, i: &Issue) -> Option<ForgeEvent> {
        (i.state == Some(StateType::Closed))
            .then(|| webhook::subject_closed(&thread.repo, thread.subject, false))
    }

    fn pr_retire(
        &self,
        thread: &ThreadKey,
        p: &forgejo_api::structs::PullRequest,
    ) -> Option<ForgeEvent> {
        (p.state == Some(StateType::Closed))
            .then(|| webhook::subject_closed(&thread.repo, thread.subject, p.merged == Some(true)))
    }

    /// Retirement sweep entry: one fetch, then the shared mapping. `resync_thread`
    /// does not call this — it reuses the subject it fetches for its open feeds.
    async fn closed_subject_event(&self, thread: &ThreadKey) -> Result<Option<ForgeEvent>> {
        let (o, r) = own(&thread.repo);
        Ok(match thread.subject {
            Subject::Issue(n) => {
                let i = go(self.api.issue_get_issue(o, r, n as i64)).await?;
                self.issue_retire(thread, &i)
            }
            Subject::Pr(n) => {
                let p = go(self.api.repo_get_pull_request(o, r, n as i64)).await?;
                self.pr_retire(thread, &p)
            }
        })
    }

    /// Inline PR review-comment replies awaiting the bot: each review comment
    /// authored by someone else that mentions `me` (or where `me` is assigned),
    /// as a `comment.created` carrying `reply_to`. `notification_event` fetches
    /// via `issue_get_comment` and so never recovers these.
    async fn review_reply_events(
        &self,
        repo: &RepoId,
        pr: u64,
        assignees: Vec<String>,
        me: &str,
    ) -> Result<Vec<ForgeEvent>> {
        let (o, r) = own(repo);
        let (_, reviews) = go(self.api.repo_list_pull_reviews(o, r, pr as i64)).await?;
        let assigned = assignees.iter().any(|a| a == me);
        let mut out = Vec::new();
        for rid in reviews.into_iter().filter_map(|rev| rev.id) {
            let comments = go(self.api.repo_get_pull_review_comments(o, r, pr as i64, rid)).await?;
            for c in comments {
                let author = login(&c.user);
                let body = text(&c.body);
                let Some(id) = c.id else { continue };
                if author != me && (assigned || webhook::mentions(&body).iter().any(|m| m == me)) {
                    // Assignees ride the payload so the `reply` trigger's
                    // assignee arm fires on an assigned PR's review comment,
                    // the same as an issue comment recovered via notifications.
                    out.push(webhook::comment_created(
                        repo,
                        Subject::Pr(pr),
                        (id as u64).to_string(),
                        &author,
                        &author,
                        &body,
                        json!({"reply_to": id as u64, "url": c.html_url, "assignees": assignees.clone()}),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Submitted reviews and their inline comments for a PR, as context. Issue
    /// comments (what `context` gathers) never cover these, so without it the
    /// agent is blind to a `REQUEST_CHANGES` review. Empty reviews (no body, no
    /// inline comments — e.g. a bare approval) are dropped as noise.
    async fn reviews(&self, o: &str, r: &str, pr: i64) -> Result<Vec<Value>> {
        let (_, reviews) = go(self.api.repo_list_pull_reviews(o, r, pr)).await?;
        let mut out = Vec::new();
        for rev in reviews {
            let Some(rid) = rev.id else { continue };
            let comments = go(self.api.repo_get_pull_review_comments(o, r, pr, rid)).await?;
            let inline: Vec<Value> = comments
                .iter()
                .map(|c| {
                    json!({"id": c.id, "author": login(&c.user), "path": c.path,
                           "line": c.position, "body": text(&c.body)})
                })
                .collect();
            let body = text(&rev.body);
            if body.is_empty() && inline.is_empty() {
                continue;
            }
            out.push(json!({"author": login(&rev.user), "state": rev.state,
                            "body": body, "comments": inline}));
        }
        Ok(out)
    }

    async fn changes_requested_events(
        &self,
        repo: &RepoId,
        pr: u64,
        me: &str,
    ) -> Result<Vec<ForgeEvent>> {
        let (o, r) = own(repo);
        let (_, reviews) = go(self.api.repo_list_pull_reviews(o, r, pr as i64)).await?;
        Ok(reviews
            .into_iter()
            .filter_map(|review| {
                let author = login(&review.user);
                (author != me && review.state.as_deref() == Some("REQUEST_CHANGES")).then(|| {
                    webhook::changes_requested(
                        repo,
                        pr,
                        review.id.unwrap_or_default() as u64,
                        &author,
                        &text(&review.body),
                    )
                })
            })
            .collect())
    }

    /// `ci.run_completed` when the PR's latest action run failed.
    async fn ci_failed_event(&self, repo: &RepoId, pr: u64, me: &str) -> Option<ForgeEvent> {
        let run = self.latest_run(repo, pr).await?;
        (run.status.as_deref() == Some("failure")).then(|| {
            webhook::ci_run_completed(
                repo,
                pr,
                me,
                "failure",
                &text(&run.workflow_id),
                (
                    num(run.id),
                    &run.html_url
                        .as_ref()
                        .map(Url::to_string)
                        .unwrap_or_default(),
                ),
                me,
            )
        })
    }

    /// `issue.referenced_pr_merged` for each merged PR cross-referencing this
    /// open issue, read from its timeline.
    async fn referenced_merged_events(
        &self,
        repo: &RepoId,
        issue: u64,
        me: &str,
    ) -> Result<Vec<ForgeEvent>> {
        let (o, r) = own(repo);
        // Forgejo returns JSON `null` rather than `[]` for an empty timeline;
        // decode leniently because the generated client requires an array.
        let body: String = go(self
            .api
            .issue_get_comments_and_timeline(o, r, issue as i64, Default::default())
            .response_type::<String>())
        .await?;
        let timeline: Vec<TimelineComment> =
            serde_json::from_str::<Option<Vec<TimelineComment>>>(&body)
                .map_err(err)?
                .unwrap_or_default();
        Ok(timeline
            .into_iter()
            .filter_map(|tc| {
                let ref_pr = tc.ref_issue?;
                let merged = ref_pr
                    .pull_request
                    .as_ref()
                    .is_some_and(|m| m.merged == Some(true));
                merged.then(|| {
                    webhook::referenced_pr_merged(
                        repo,
                        issue,
                        me,
                        num(ref_pr.number),
                        &text(&ref_pr.title),
                        &ref_pr
                            .html_url
                            .as_ref()
                            .map(Url::to_string)
                            .unwrap_or_default(),
                        &login(&tc.user),
                    )
                })
            })
            .collect())
    }

    /// `pull_request.unblocked` once every issue blocking this bot-authored PR is
    /// closed. Forgejo emits no dependency-resolved webhook, so the resync polls
    /// this each pass; the `unblocked-{pr}` idempotency key makes it fire at most
    /// once (see upstream-fixes #5). A PR with no blockers, or with any still-open
    /// one, yields `None`.
    async fn unblocked_event(
        &self,
        repo: &RepoId,
        pr: u64,
        author: &str,
        title: &str,
    ) -> Result<Option<ForgeEvent>> {
        let (o, r) = own(repo);
        let deps = go(self.api.issue_list_issue_dependencies(o, r, pr as i64)).await?;
        let all_closed =
            !deps.is_empty() && deps.iter().all(|d| d.state == Some(StateType::Closed));
        Ok(all_closed.then(|| webhook::pull_request_unblocked(repo, pr, author, title)))
    }

    /// Comment events from every unread notification on `thread` (the cursor
    /// that surfaces comments); `None` targets all notifications, for a full
    /// resync.
    async fn notification_events(&self, thread: Option<&ThreadKey>) -> Result<Vec<ForgeEvent>> {
        // Take the raw body and decode it into our own lenient shape (see
        // `Notif`): the typed `notify_get_list` fails the whole batch on a
        // `merged` subject state.
        let body: String = go(self
            .api
            .notify_get_list(Default::default())
            .response_type::<String>())
        .await?;
        let notes: Vec<Notif> = serde_json::from_str(&body).map_err(err)?;
        let mut out = Vec::new();
        for note in &notes {
            if let Some(ev) = self.notification_event(note).await
                && thread.is_none_or(|t| ev.thread() == *t)
            {
                out.push(ev);
            }
        }
        Ok(out)
    }

    async fn search(
        &self,
        kind: IssueSearchIssuesQueryType,
        set: impl FnOnce(&mut IssueSearchIssuesQuery) + Send,
    ) -> Result<Vec<Issue>> {
        let mut query = IssueSearchIssuesQuery {
            state: Some(IssueSearchIssuesQueryState::Open),
            r#type: Some(kind),
            ..Default::default()
        };
        set(&mut query);
        Ok(go(self.api.issue_search_issues(query)).await?.1)
    }
}

#[async_trait]
impl Forge for Forgejo {
    async fn whoami(&self) -> Result<String> {
        self.me().await
    }

    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId> {
        let me = self.me().await?;
        if repo.owner == me {
            return Ok(repo.clone());
        }
        let (o, r) = own(repo);
        // Forgejo may rename the fork on collision, so the fork's identity
        // comes from the returned repo, never an assumed `{me}/{name}`.
        match self.api.create_fork(o, r, args(json!({}))).await {
            Ok(fork) => fork_id(&fork),
            // Fork already exists (409 Conflict): resolve it from the source
            // repo's fork list rather than erroring.
            Err(e) if is_conflict(&e) => {
                let (_, forks) = go(self.api.list_forks(o, r)).await?;
                forks
                    .iter()
                    .find(|f| login(&f.owner) == me)
                    .map(fork_id)
                    .unwrap_or_else(|| {
                        Err(Error::Forge(format!("no fork of {repo} owned by {me}")))
                    })
            }
            Err(e) => Err(err(e)),
        }
    }

    async fn context(&self, thread: &ThreadKey) -> Result<Value> {
        let (o, r) = own(&thread.repo);
        let (Subject::Issue(n) | Subject::Pr(n)) = thread.subject;
        // The issues endpoints cover PRs too; only the diff is PR-specific.
        let i = go(self.api.issue_get_issue(o, r, n as i64)).await?;
        let (_, comments) = go(self
            .api
            .issue_get_comments(o, r, n as i64, Default::default()))
        .await?;
        let comments: Vec<Value> = comments
            .iter()
            .map(|c| json!({"author": login(&c.user), "body": text(&c.body)}))
            .collect();
        let mut ctx = json!({
            "title": text(&i.title), "body": text(&i.body), "author": login(&i.user),
            "state": i.state, "url": i.html_url, "comments": comments,
        });
        if let Subject::Pr(_) = thread.subject {
            let pull = go(self.api.repo_get_pull_request(o, r, n as i64)).await?;
            ctx["head_branch"] = pull
                .head
                .as_ref()
                .and_then(|head| head.r#ref.clone())
                .unwrap_or_default()
                .into();
            ctx["base_branch"] = pull
                .base
                .as_ref()
                .and_then(|base| base.r#ref.clone())
                .unwrap_or_default()
                .into();
            let req = self.api.repo_download_pull_diff_or_patch(
                o,
                r,
                n as i64,
                "diff",
                Default::default(),
            );
            ctx["diff"] = clip(go(req).await?).into();
            if let Some(log) = self.failed_run_log(&thread.repo, n).await {
                ctx["ci_log"] = log.into();
            }
            ctx["reviews"] = json!(self.reviews(o, r, n as i64).await?);
        }
        Ok(ctx)
    }

    async fn repo_config(&self, repo: &RepoId) -> Result<Option<(String, String)>> {
        self.repo_file(repo, ".forgebot.toml").await
    }

    async fn repo_file(&self, repo: &RepoId, path: &str) -> Result<Option<(String, String)>> {
        // forgejo-api 0.11 double-encodes a `/` in the filepath (`%2F` → `%252F`
        // → 404), so a subdirectory path never resolves. Reach the contents
        // endpoint directly with the path kept as literal segments.
        let (o, r) = own(repo);
        let base = self.url.as_str().trim_end_matches('/');
        let resp = reqwest::Client::new()
            .get(format!("{base}/api/v1/repos/{o}/{r}/contents/{path}"))
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await
            .map_err(|e| Error::Forge(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Error::Forge(format!("read {path}: http {}", resp.status())));
        }
        let text = resp.text().await.map_err(|e| Error::Forge(e.to_string()))?;
        let body: Value = serde_json::from_str(&text).map_err(err)?;
        let b64: String = body["content"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(err)?;
        let sha = body["sha"].as_str().unwrap_or_default().to_string();
        Ok(Some((String::from_utf8(bytes).map_err(err)?, sha)))
    }

    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>> {
        let (o, r) = own(repo);
        let q = IssueListIssuesQuery {
            q: Some(query.into()),
            ..Default::default()
        };
        let (_, mut issues) = go(self.api.issue_list_issues(o, r, q)).await?;
        // Forgejo 11's repository-list endpoint has returned an empty result
        // for a valid multi-word `q` on real instances, while the same issue
        // is visible without that parameter. Fall back to the bounded default
        // page and filter it locally so the forge search tool remains useful.
        if issues.is_empty() && !query.is_empty() {
            let (_, listed) = go(self.api.issue_list_issues(o, r, Default::default())).await?;
            let needle = query.to_ascii_lowercase();
            issues = listed
                .into_iter()
                .filter(|issue| {
                    issue
                        .title
                        .as_deref()
                        .is_some_and(|title| title.to_ascii_lowercase().contains(&needle))
                        || issue
                            .body
                            .as_deref()
                            .is_some_and(|body| body.to_ascii_lowercase().contains(&needle))
                })
                .collect();
        }
        Ok(issues
            .iter()
            .map(|i| IssueSummary {
                number: num(i.number),
                title: text(&i.title),
                state: match i.state {
                    Some(StateType::Closed) => "closed",
                    _ => "open",
                }
                .into(),
                url: i.html_url.as_ref().map(Url::to_string).unwrap_or_default(),
            })
            .collect())
    }

    async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64> {
        let (o, r) = own(repo);
        let opt = args(json!({"title": title, "body": body}));
        Ok(num(go(self.api.issue_create_issue(o, r, opt))
            .await?
            .number))
    }

    async fn close_issue(&self, repo: &RepoId, number: u64, comment: Option<&str>) -> Result<()> {
        if let Some(note) = comment {
            let thread = ThreadKey {
                repo: repo.clone(),
                subject: Subject::Issue(number),
            };
            self.comment(&thread, note, None).await?;
        }
        let (o, r) = own(repo);
        let opt = args(json!({"state": "closed"}));
        go(self.api.issue_edit_issue(o, r, number as i64, opt)).await?;
        Ok(())
    }

    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64> {
        let (o, r) = own(repo);
        let base = text(&go(self.api.repo_get(o, r)).await?.default_branch);
        // A repo the bot doesn't own is worked from its fork; Forgejo's
        // cross-repo head form names the source branch as `{me}:{branch}`.
        let me = self.me().await?;
        let head = if repo.owner == me {
            pr.branch
        } else {
            format!("{me}:{}", pr.branch)
        };
        let opt = args(
            json!({"title": pr.title, "body": pr.body, "head": head, "base": base, "assignees": [me]}),
        );
        Ok(num(go(self.api.repo_create_pull_request(o, r, opt))
            .await?
            .number))
    }

    async fn update_pr(&self, repo: &RepoId, number: u64, patch: PrPatch) -> Result<()> {
        let (o, r) = own(repo);
        // The edit endpoint has no draft flag; the `WIP: ` title prefix is
        // Forgejo's draft mechanism.
        let title = match (patch.draft, patch.title) {
            (None, title) => title,
            (Some(draft), title) => {
                let cur = match title {
                    Some(t) => t,
                    None => text(
                        &go(self.api.repo_get_pull_request(o, r, number as i64))
                            .await?
                            .title,
                    ),
                };
                let bare = cur.strip_prefix("WIP: ").unwrap_or(&cur);
                Some(if draft {
                    format!("WIP: {bare}")
                } else {
                    bare.into()
                })
            }
        };
        let opt = args(json!({"title": title, "body": patch.body}));
        go(self.api.repo_edit_pull_request(o, r, number as i64, opt)).await?;
        Ok(())
    }

    async fn add_dependency(&self, repo: &RepoId, blocked: u64, blocked_by: u64) -> Result<()> {
        let (o, r) = own(repo);
        // Issues and PRs share one index space, so `blocked` may be a PR; the
        // dependency gates its merge while `blocked_by` is open, but only when
        // the repo has dependencies enabled (Forgejo's default).
        let opt = args(json!({ "index": blocked_by }));
        go(self
            .api
            .issue_create_issue_dependencies(o, r, blocked as i64, opt))
        .await?;
        Ok(())
    }

    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64> {
        let (o, r) = own(&thread.repo);
        let (Subject::Issue(n) | Subject::Pr(n)) = thread.subject;
        let n = n as i64;
        let Some(target) = reply_to else {
            let opt = args(json!({"body": body}));
            return Ok(num(go(self.api.issue_create_comment(o, r, n, opt))
                .await?
                .id));
        };
        // Replying in a review thread: find the target comment's review and
        // location, then attach the reply at the same path/position.
        let (_, reviews) = go(self.api.repo_list_pull_reviews(o, r, n)).await?;
        for rid in reviews.into_iter().filter_map(|rev| rev.id) {
            let comments = go(self.api.repo_get_pull_review_comments(o, r, n, rid)).await?;
            if let Some(c) = comments.into_iter().find(|c| c.id == Some(target as i64)) {
                let opt = json!({
                    "body": body, "path": c.path,
                    "new_position": c.position.unwrap_or(0),
                    "old_position": c.original_position.unwrap_or(0),
                });
                let reply = go(self.api.repo_create_pull_review_comment(o, r, n, rid, opt)).await?;
                return Ok(num(reply.id));
            }
        }
        Err(Error::Forge(format!(
            "review comment {target} not found on {thread}"
        )))
    }

    async fn edit_comment(&self, repo: &RepoId, id: u64, body: &str) -> Result<()> {
        let (o, r) = own(repo);
        let opt = args(json!({"body": body}));
        go(self.api.issue_edit_comment(o, r, id as i64, opt)).await?;
        Ok(())
    }

    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()> {
        let (o, r) = own(repo);
        let event = match review.verdict {
            Verdict::Approve => "APPROVED",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Comment => "COMMENT",
        };
        let comments: Vec<Value> = review
            .inline
            .iter()
            .map(|c| json!({"body": c.body, "path": c.path, "new_position": c.line}))
            .collect();
        let opt = args(json!({"body": review.summary, "event": event, "comments": comments}));
        go(self.api.repo_create_pull_review(o, r, pr as i64, opt)).await?;
        Ok(())
    }

    async fn mint_token(&self, label: &str) -> Result<ScopedToken> {
        let me = self.me().await?;
        let opt = args(json!({"name": label, "scopes": TOKEN_SCOPES}));
        let t = go(self.token_api().await?.user_create_token(&me, opt)).await?;
        Ok(ScopedToken {
            id: t.id.unwrap_or_default(),
            secret: t.sha1.unwrap_or_default(),
        })
    }

    async fn revoke_token(&self, token: &ScopedToken) -> Result<()> {
        let me = self.me().await?;
        go(self
            .token_api()
            .await?
            .user_delete_access_token(&me, &token.id.to_string()))
        .await
    }

    async fn resync(&self, me: &str) -> Result<Vec<ForgeEvent>> {
        let mut out = self.notification_events(None).await?;

        let issues = IssueSearchIssuesQueryType::Issues;
        let pulls = IssueSearchIssuesQueryType::Pulls;

        for i in self.search(issues, |q| q.assigned = Some(true)).await? {
            if let Some(repo) = meta_repo(&i) {
                out.push(self.assigned_event(&repo, &i, me));
            }
        }

        for p in self
            .search(pulls, |q| q.review_requested = Some(true))
            .await?
        {
            let Some(repo) = meta_repo(&p) else { continue };
            out.push(webhook::review_requested(
                &repo,
                num(p.number),
                me,
                &login(&p.user),
                &text(&p.title),
                &login(&p.user),
            ));
        }

        for p in self.search(pulls, |q| q.created = Some(true)).await? {
            let Some(repo) = meta_repo(&p) else { continue };
            let pr = num(p.number);
            if let Some(ev) = self.ci_failed_event(&repo, pr, me).await {
                out.push(ev);
            }
            // Review comments on the bot's own PRs are otherwise seen only via
            // the live `pull_request_review` webhook; back-fill them so a missed
            // delivery is recovered (comment id dedupes against that path).
            out.extend(
                self.review_reply_events(&repo, pr, assignees(p.assignees.as_deref()), me)
                    .await?,
            );
            out.extend(self.changes_requested_events(&repo, pr, me).await?);
            // Forgejo fires no webhook when an issue dependency is resolved, so
            // a PR blocked on one is never re-picked-up by the live path; poll
            // it here and resume it once every blocker is closed (upstream-fixes #5).
            if let Some(ev) = self
                .unblocked_event(&repo, pr, &login(&p.user), &text(&p.title))
                .await?
            {
                out.push(ev);
            }
        }

        for i in self.search(issues, |q| q.created = Some(true)).await? {
            if let Some(repo) = meta_repo(&i) {
                out.extend(
                    self.referenced_merged_events(&repo, num(i.number), me)
                        .await?,
                );
            }
        }

        // Body mentions have no other resync feed: only the targeted
        // `resync_thread` derives them, so a missed live poke would strip a
        // body-mention forever. This makes "the full resync recovers a missed
        // poke" true for them too — the `body` idempotency key dedupes against
        // the poke, and `body_mention_event` no-ops on a body that doesn't
        // actually mention `me` (a mention living only in a comment is left to
        // the notification feed).
        for i in self.search(issues, |q| q.mentioned = Some(true)).await? {
            let Some(repo) = meta_repo(&i) else { continue };
            let subject = match i.pull_request {
                Some(_) => Subject::Pr(num(i.number)),
                None => Subject::Issue(num(i.number)),
            };
            out.extend(self.body_mention_event(
                &repo,
                subject,
                &login(&i.user),
                &text(&i.body),
                assignees(i.assignees.as_deref()),
                me,
            ));
        }
        Ok(out)
    }

    async fn resync_thread(&self, me: &str, thread: &ThreadKey) -> Result<Vec<ForgeEvent>> {
        let (o, r) = own(&thread.repo);
        let (Subject::Issue(n) | Subject::Pr(n)) = thread.subject;
        let mut out = self.notification_events(Some(thread)).await?;

        // A closed subject only retires — the open feeds below are all
        // `state=Open`, so it emits nothing else (and re-emitting `opened` would
        // re-review a merged PR). The subject is fetched once per arm and reused
        // for the closed-check and the open feeds; the mapping is shared with the
        // sweep via `issue_retire`/`pr_retire`.
        match thread.subject {
            Subject::Issue(_) => {
                let i = go(self.api.issue_get_issue(o, r, n as i64)).await?;
                if let Some(closed) = self.issue_retire(thread, &i) {
                    out.push(closed);
                    return Ok(out);
                }
                if assigned_to(&i, me) {
                    out.push(self.assigned_event(&thread.repo, &i, me));
                }
                out.extend(self.body_mention_event(
                    &thread.repo,
                    thread.subject,
                    &login(&i.user),
                    &text(&i.body),
                    assignees(i.assignees.as_deref()),
                    me,
                ));
                out.extend(self.referenced_merged_events(&thread.repo, n, me).await?);
            }
            Subject::Pr(_) => {
                let p = go(self.api.repo_get_pull_request(o, r, n as i64)).await?;
                if let Some(closed) = self.pr_retire(thread, &p) {
                    out.push(closed);
                    return Ok(out);
                }
                if reviewer_requested(&p, me) {
                    out.push(webhook::review_requested(
                        &thread.repo,
                        n,
                        me,
                        &login(&p.user),
                        &text(&p.title),
                        &login(&p.user),
                    ));
                }
                out.extend(self.body_mention_event(
                    &thread.repo,
                    thread.subject,
                    &login(&p.user),
                    &text(&p.body),
                    assignees(p.assignees.as_deref()),
                    me,
                ));
                out.extend(
                    self.review_reply_events(
                        &thread.repo,
                        n,
                        assignees(p.assignees.as_deref()),
                        me,
                    )
                    .await?,
                );
                if login(&p.user) == me {
                    out.extend(self.changes_requested_events(&thread.repo, n, me).await?);
                    out.extend(self.ci_failed_event(&thread.repo, n, me).await);
                    out.push(webhook::pull_request_opened(
                        &thread.repo,
                        n,
                        &login(&p.user),
                        &text(&p.title),
                        &text(&p.body),
                    ));
                }
            }
        }
        Ok(out)
    }

    async fn closed_event(&self, thread: &ThreadKey) -> Result<Option<ForgeEvent>> {
        self.closed_subject_event(thread).await
    }

    async fn ack(&self, event: &ForgeEvent) -> Result<()> {
        if let Some(id) = event.payload.get("_notification").and_then(Value::as_i64) {
            go(self.api.notify_read_thread(id, Default::default())).await?;
        }
        Ok(())
    }
}

async fn go<T>(req: impl IntoFuture<Output = std::result::Result<T, ForgejoError>>) -> Result<T> {
    req.await.map_err(err)
}

/// Build a request struct from the fields that matter; serde fills the rest
/// with `None`. The wiremock tests pin the resulting wire shapes.
fn args<T: serde::de::DeserializeOwned>(fields: Value) -> T {
    serde_json::from_value(fields).expect("request structs deserialize from partial json")
}

fn err(e: impl Display) -> Error {
    Error::Forge(e.to_string())
}

/// A 409 from the fork endpoint means the fork already exists.
fn is_conflict(e: &ForgejoError) -> bool {
    matches!(
        e,
        ForgejoError::ApiError(a)
            if matches!(&a.kind, ApiErrorKind::Other(code) if code.as_u16() == 409)
    )
}

fn text(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn login(u: &Option<forgejo_api::structs::User>) -> String {
    u.as_ref().and_then(|u| u.login.clone()).unwrap_or_default()
}

fn own(repo: &RepoId) -> (&str, &str) {
    (&repo.owner, &repo.name)
}

/// A repo's `owner/name` taken from the API object, not assumed — Forgejo may
/// rename a fork on collision.
fn fork_id(repo: &forgejo_api::structs::Repository) -> Result<RepoId> {
    let owner = login(&repo.owner);
    let name = text(&repo.name);
    if owner.is_empty() || name.is_empty() {
        return Err(Error::Forge("fork response missing owner/name".into()));
    }
    Ok(RepoId { owner, name })
}

fn num(n: Option<i64>) -> u64 {
    n.unwrap_or_default() as u64
}

fn meta_repo(i: &Issue) -> Option<RepoId> {
    i.repository.as_ref()?.full_name.as_ref()?.parse().ok()
}

fn assigned_to(i: &Issue, me: &str) -> bool {
    i.assignees
        .iter()
        .flatten()
        .any(|u| u.login.as_deref() == Some(me))
}

fn reviewer_requested(p: &forgejo_api::structs::PullRequest, me: &str) -> bool {
    p.requested_reviewers
        .iter()
        .flatten()
        .any(|u| u.login.as_deref() == Some(me))
}

fn assignees(users: Option<&[forgejo_api::structs::User]>) -> Vec<String> {
    users
        .into_iter()
        .flatten()
        .filter_map(|u| u.login.clone())
        .collect()
}

/// The slice of a notification the reconciler actually reads. Deliberately
/// omits `subject.state`: Forgejo sets it to `merged` on a merged PR (and may
/// grow other values), which forgejo-api's two-variant `StateType` can't
/// decode — and `notify_get_list` decodes the whole list at once, so one such
/// entry sinks every reconcile. Serde ignores the unmodeled `state`, so a
/// merged-PR notification no longer fails the batch. Retargeted onto
/// forgejo-api's own request via `response_type`, so only the decoding changes.
#[derive(serde::Deserialize)]
struct Notif {
    id: Option<i64>,
    repository: Option<NotifRepo>,
    subject: Option<NotifSubject>,
}

#[derive(serde::Deserialize)]
struct NotifRepo {
    full_name: Option<String>,
}

#[derive(serde::Deserialize)]
struct NotifSubject {
    #[serde(rename = "type")]
    r#type: Option<String>,
    url: Option<String>,
    latest_comment_url: Option<String>,
}

/// Trailing path integer of an API URL (`…/issues/42` → 42); `None` for a
/// blank or unparseable URL.
fn trailing_id(url: &Option<String>) -> Option<u64> {
    Url::parse(url.as_deref()?)
        .ok()?
        .path_segments()?
        .next_back()?
        .parse()
        .ok()
}

fn clip(mut s: String) -> String {
    if s.len() > EXCERPT_MAX {
        let mut end = EXCERPT_MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Like `clip`, but keeps the end — CI failures live at the log's tail.
fn clip_tail(mut s: String) -> String {
    if s.len() > EXCERPT_MAX {
        let mut start = s.len() - EXCERPT_MAX;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        s.replace_range(..start, "");
    }
    s
}

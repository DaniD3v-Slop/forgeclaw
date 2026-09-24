//! Forgejo implementation of the operations exposed to OpenClaw.

mod webhook;

pub use webhook::webhook_events;

use std::fmt::Display;
use std::future::IntoFuture;
use std::sync::OnceLock;

use async_trait::async_trait;
use forgeclaw_core::{
    Error, Forge, IssueSummary, NewPr, RepoId, Result, Review, ScopedToken, Subject, ThreadKey,
    Verdict,
};
use forgejo_api::structs::{ActionRun, IssueListIssuesQuery, StateType};
use forgejo_api::{ApiErrorKind, Auth, ForgejoError};
use serde_json::{Value, json};
use url::Url;

const EXCERPT_MAX: usize = 64 * 1024;
const TOKEN_SCOPES: &[&str] = &["write:repository", "write:issue", "read:user"];
pub struct Forgejo {
    api: forgejo_api::Forgejo,
    url: Url,
    password: Option<String>,
    me: OnceLock<String>,
}

impl Forgejo {
    pub fn new(url: Url, token: &str, password: Option<String>) -> Result<Self> {
        Ok(Self {
            api: forgejo_api::Forgejo::new(Auth::Token(token), url.clone()).map_err(err)?,
            url,
            password,
            me: OnceLock::new(),
        })
    }

    async fn token_api(&self) -> Result<forgejo_api::Forgejo> {
        let username = self.me().await?;
        let password = self.password.as_deref().ok_or_else(|| {
            Error::Forge("the forge account password is unavailable for token management".into())
        })?;
        forgejo_api::Forgejo::new(
            Auth::Password {
                username: &username,
                password,
                mfa: None,
            },
            self.url.clone(),
        )
        .map_err(err)
    }

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

    async fn latest_run(&self, repo: &RepoId, pr: u64) -> Option<ActionRun> {
        let (owner, name) = own(repo);
        let runs = self
            .api
            .list_action_runs(owner, name, Default::default())
            .await
            .ok()?;
        runs.workflow_runs
            .into_iter()
            .flatten()
            .filter(|run| {
                run.event_payload
                    .as_deref()
                    .and_then(pr_from_payload)
                    .is_some_and(|number| number == pr)
            })
            .max_by_key(|run| run.id)
    }

    async fn failed_run_log(&self, repo: &RepoId, pr: u64) -> Option<String> {
        let run = self.latest_run(repo, pr).await?;
        if run.status.as_deref() != Some("failure") {
            return None;
        }
        let (owner, name) = own(repo);
        let jobs = self
            .api
            .list_action_run_jobs(owner, name, run.id?)
            .await
            .ok()?;
        let job = jobs
            .iter()
            .find(|job| job.status.as_deref() == Some("failure"))?;
        let log = self
            .api
            .repo_get_action_job_logs(owner, name, job.id?, Default::default())
            .await
            .ok()?;
        Some(clip_tail(log))
    }

    async fn reviews(&self, owner: &str, name: &str, pr: i64) -> Result<Vec<Value>> {
        let (_, reviews) = go(self.api.repo_list_pull_reviews(owner, name, pr)).await?;
        let mut result = Vec::new();
        for review in reviews {
            let Some(review_id) = review.id else {
                continue;
            };
            let comments = go(self
                .api
                .repo_get_pull_review_comments(owner, name, pr, review_id))
            .await?;
            let inline: Vec<Value> = comments
                .iter()
                .map(|comment| {
                    json!({
                        "id": comment.id,
                        "author": login(&comment.user),
                        "path": comment.path,
                        "line": comment.position,
                        "body": text(&comment.body),
                    })
                })
                .collect();
            let body = text(&review.body);
            if !body.is_empty() || !inline.is_empty() {
                result.push(json!({
                    "author": login(&review.user),
                    "state": review.state,
                    "body": body,
                    "comments": inline,
                }));
            }
        }
        Ok(result)
    }
}

impl Forgejo {
    pub async fn whoami(&self) -> Result<String> {
        self.me().await
    }

    pub async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId> {
        let me = self.me().await?;
        if repo.owner == me {
            return Ok(repo.clone());
        }
        let (owner, name) = own(repo);
        match self.api.create_fork(owner, name, args(json!({}))).await {
            Ok(fork) => fork_id(&fork),
            Err(error) if is_conflict(&error) => {
                let (_, forks) = go(self.api.list_forks(owner, name)).await?;
                forks
                    .iter()
                    .find(|fork| login(&fork.owner) == me)
                    .map(fork_id)
                    .unwrap_or_else(|| {
                        Err(Error::Forge(format!("no fork of {repo} owned by {me}")))
                    })
            }
            Err(error) => Err(err(error)),
        }
    }

    pub async fn context(&self, thread: &ThreadKey) -> Result<Value> {
        let (owner, name) = own(&thread.repo);
        let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
        let issue = go(self.api.issue_get_issue(owner, name, number as i64)).await?;
        let (_, comments) =
            go(self
                .api
                .issue_get_comments(owner, name, number as i64, Default::default()))
            .await?;
        let comments: Vec<Value> = comments
            .iter()
            .map(|comment| json!({"author": login(&comment.user), "body": text(&comment.body)}))
            .collect();
        let mut context = json!({
            "title": text(&issue.title),
            "body": text(&issue.body),
            "author": login(&issue.user),
            "state": issue.state,
            "url": issue.html_url,
            "comments": comments,
        });
        if let Subject::Pr(_) = thread.subject {
            let pull = go(self.api.repo_get_pull_request(owner, name, number as i64)).await?;
            let head_repo = pull.head.as_ref().and_then(|head| head.repo.as_ref());
            context["head_owner"] = head_repo
                .map(|repo| login(&repo.owner))
                .unwrap_or_default()
                .into();
            context["head_repo"] = head_repo
                .and_then(|repo| repo.full_name.clone())
                .unwrap_or_default()
                .into();
            context["head_branch"] = pull
                .head
                .as_ref()
                .and_then(|head| head.r#ref.clone())
                .unwrap_or_default()
                .into();
            context["base_branch"] = pull
                .base
                .as_ref()
                .and_then(|base| base.r#ref.clone())
                .unwrap_or_default()
                .into();
            let diff = self.api.repo_download_pull_diff_or_patch(
                owner,
                name,
                number as i64,
                "diff",
                Default::default(),
            );
            context["diff"] = clip(go(diff).await?).into();
            if let Some(log) = self.failed_run_log(&thread.repo, number).await {
                context["ci_log"] = log.into();
            }
            context["reviews"] = json!(self.reviews(owner, name, number as i64).await?);
        }
        Ok(context)
    }

    pub async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>> {
        let (owner, name) = own(repo);
        let options = IssueListIssuesQuery {
            q: Some(query.into()),
            ..Default::default()
        };
        let (_, mut issues) = go(self.api.issue_list_issues(owner, name, options)).await?;
        if issues.is_empty() && !query.is_empty() {
            let (_, listed) =
                go(self.api.issue_list_issues(owner, name, Default::default())).await?;
            let query = query.to_ascii_lowercase();
            issues = listed
                .into_iter()
                .filter(|issue| {
                    issue
                        .title
                        .as_deref()
                        .is_some_and(|title| title.to_ascii_lowercase().contains(&query))
                        || issue
                            .body
                            .as_deref()
                            .is_some_and(|body| body.to_ascii_lowercase().contains(&query))
                })
                .collect();
        }
        issues
            .iter()
            .map(|issue| {
                Ok(IssueSummary {
                    number: required_num(issue.number, "issue number")?,
                    title: text(&issue.title),
                    state: match issue.state {
                        Some(StateType::Closed) => "closed",
                        _ => "open",
                    }
                    .into(),
                    url: issue
                        .html_url
                        .as_ref()
                        .map(Url::to_string)
                        .unwrap_or_default(),
                })
            })
            .collect()
    }

    pub async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64> {
        let (owner, name) = own(repo);
        let base = text(&go(self.api.repo_get(owner, name)).await?.default_branch);
        let me = self.me().await?;
        let head = if repo.owner == me {
            pr.branch
        } else {
            format!("{me}:{}", pr.branch)
        };
        let options = args(json!({
            "title": pr.title,
            "body": pr.body,
            "head": head,
            "base": base,
        }));
        required_num(
            go(self.api.repo_create_pull_request(owner, name, options))
                .await?
                .number,
            "pull request number",
        )
    }

    pub async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64> {
        let (owner, name) = own(repo);
        let options = args(json!({"title": title, "body": body}));
        required_num(
            go(self.api.issue_create_issue(owner, name, options))
                .await?
                .number,
            "issue number",
        )
    }

    pub async fn comment(
        &self,
        thread: &ThreadKey,
        body: &str,
        reply_to: Option<u64>,
    ) -> Result<u64> {
        let (owner, name) = own(&thread.repo);
        let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
        let number = number as i64;
        let Some(target) = reply_to else {
            let options = args(json!({"body": body}));
            return required_num(
                go(self.api.issue_create_comment(owner, name, number, options))
                    .await?
                    .id,
                "comment id",
            );
        };
        let (_, reviews) = go(self.api.repo_list_pull_reviews(owner, name, number)).await?;
        for review_id in reviews.into_iter().filter_map(|review| review.id) {
            let comments = go(self
                .api
                .repo_get_pull_review_comments(owner, name, number, review_id))
            .await?;
            if let Some(comment) = comments
                .into_iter()
                .find(|comment| comment.id == Some(target as i64))
            {
                let options = json!({
                    "body": body,
                    "path": comment.path,
                    "new_position": comment.position.unwrap_or(0),
                    "old_position": comment.original_position.unwrap_or(0),
                });
                let reply = go(self
                    .api
                    .repo_create_pull_review_comment(owner, name, number, review_id, options))
                .await?;
                return required_num(reply.id, "review comment id");
            }
        }
        Err(Error::Forge(format!(
            "review comment {target} not found on {thread}"
        )))
    }

    pub async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()> {
        let (owner, name) = own(repo);
        let event = match review.verdict {
            Verdict::Approve => "APPROVED",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Comment => "COMMENT",
        };
        let comments: Vec<Value> = review
            .inline
            .iter()
            .map(|comment| {
                json!({
                    "body": comment.body,
                    "path": comment.path,
                    "new_position": comment.line,
                })
            })
            .collect();
        let options = args(json!({
            "body": review.summary,
            "event": event,
            "comments": comments,
        }));
        go(self
            .api
            .repo_create_pull_review(owner, name, pr as i64, options))
        .await?;
        Ok(())
    }
}

#[async_trait]
impl Forge for Forgejo {
    fn with_token(&self, token: &str) -> Result<std::sync::Arc<dyn Forge>> {
        Ok(std::sync::Arc::new(Forgejo::new(
            self.url.clone(),
            token,
            None,
        )?))
    }

    async fn whoami(&self) -> Result<String> {
        Forgejo::whoami(self).await
    }

    async fn context(&self, thread: &ThreadKey) -> Result<Value> {
        Forgejo::context(self, thread).await
    }

    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId> {
        Forgejo::ensure_fork(self, repo).await
    }

    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>> {
        Forgejo::search_issues(self, repo, query).await
    }

    async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64> {
        Forgejo::create_issue(self, repo, title, body).await
    }

    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64> {
        Forgejo::create_pr(self, repo, pr).await
    }

    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64> {
        Forgejo::comment(self, thread, body, reply_to).await
    }

    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()> {
        Forgejo::submit_review(self, repo, pr, review).await
    }

    async fn mint_token(&self, label: &str) -> Result<ScopedToken> {
        let username = self.me().await?;
        let options = args(json!({"name": label, "scopes": TOKEN_SCOPES}));
        let token = go(self
            .token_api()
            .await?
            .user_create_token(&username, options))
        .await?;
        let id = token
            .id
            .ok_or_else(|| Error::Forge("token response missing id".into()))?;
        let secret = token
            .sha1
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| Error::Forge("token response missing secret".into()))?;
        Ok(ScopedToken { id, secret })
    }

    async fn revoke_token(&self, token: &ScopedToken) -> Result<()> {
        let username = self.me().await?;
        go(self
            .token_api()
            .await?
            .user_delete_access_token(&username, &token.id.to_string()))
        .await
    }
}

async fn go<T>(
    request: impl IntoFuture<Output = std::result::Result<T, ForgejoError>>,
) -> Result<T> {
    request.await.map_err(err)
}

fn args<T: serde::de::DeserializeOwned>(fields: Value) -> T {
    serde_json::from_value(fields).expect("request structs deserialize from partial json")
}

fn err(error: impl Display) -> Error {
    Error::Forge(error.to_string())
}

fn is_conflict(error: &ForgejoError) -> bool {
    matches!(
        error,
        ForgejoError::ApiError(api)
            if matches!(&api.kind, ApiErrorKind::Other(code) if code.as_u16() == 409)
    )
}

fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

fn login(user: &Option<forgejo_api::structs::User>) -> String {
    user.as_ref()
        .and_then(|user| user.login.clone())
        .unwrap_or_default()
}

fn own(repo: &RepoId) -> (&str, &str) {
    (&repo.owner, &repo.name)
}

fn fork_id(repo: &forgejo_api::structs::Repository) -> Result<RepoId> {
    let owner = login(&repo.owner);
    let name = text(&repo.name);
    if owner.is_empty() || name.is_empty() {
        return Err(Error::Forge("fork response missing owner/name".into()));
    }
    format!("{owner}/{name}").parse()
}

fn required_num(number: Option<i64>, field: &str) -> Result<u64> {
    number
        .and_then(|number| u64::try_from(number).ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| Error::Forge(format!("response missing valid {field}")))
}

fn pr_from_payload(payload: &str) -> Option<u64> {
    serde_json::from_str::<Value>(payload)
        .ok()?
        .pointer("/pull_request/number")?
        .as_u64()
}

fn clip(mut value: String) -> String {
    if value.len() > EXCERPT_MAX {
        let mut end = EXCERPT_MAX;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

fn clip_tail(mut value: String) -> String {
    if value.len() > EXCERPT_MAX {
        let mut start = value.len() - EXCERPT_MAX;
        while !value.is_char_boundary(start) {
            start += 1;
        }
        value.replace_range(..start, "");
    }
    value
}

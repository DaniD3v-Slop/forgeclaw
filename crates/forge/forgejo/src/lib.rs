//! Forgejo implementation of the operations exposed to OpenClaw.

mod webhook;

pub use webhook::webhook_events;

use std::fmt::Display;
use std::future::IntoFuture;
use std::sync::OnceLock;

use async_trait::async_trait;
use forgeclaw_core::{
    DiffPage, Error, Forge, IssueSummary, NewPr, PrUpdate, RepoId, Result, Review, ScopedToken,
    Subject, ThreadKey, Verdict,
};
use forgejo_api::structs::{ActionRun, IssueListIssuesQuery, StateType};
use forgejo_api::{ApiErrorKind, Auth, ForgejoError};
use serde_json::{Value, json};
use url::Url;

const EXCERPT_MAX: usize = 8 * 1024;
const DIFF_PAGE_MAX: usize = 16 * 1024;
const BODY_PAGE_MAX: usize = 8 * 1024;
const CI_LOG_PAGE_MAX: usize = 8 * 1024;
const REVIEW_COMMENTS_PAGE: usize = 5;
const TOKEN_SCOPES: &[&str] = &["write:repository", "write:issue", "read:user"];
const TEMP_TOKEN_PREFIX: &str = "forgeclaw-temp-";
const TOKEN_PAGE_SIZE: u32 = 100;

fn temporary_token_name(name: &str) -> bool {
    let suffix = if let Some(suffix) = name.strip_prefix(TEMP_TOKEN_PREFIX) {
        [
            "create-pr-",
            "edit-pr-",
            "create-issue-",
            "checkout-",
            "push-",
        ]
        .iter()
        .find_map(|action| suffix.strip_prefix(action))
        .unwrap_or(suffix)
    } else if let Some(suffix) = name.strip_prefix("forgeclaw-") {
        suffix
    } else {
        return false;
    };
    suffix.len() == 36
        && suffix.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub struct Forgejo {
    api: forgejo_api::Forgejo,
    http: reqwest::Client,
    url: Url,
    password: Option<String>,
    me: OnceLock<String>,
}

impl Forgejo {
    pub fn new(url: Url, token: &str, password: Option<String>) -> Result<Self> {
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("token {token}"))
            .map_err(|_| Error::Forge("invalid forge token".into()))?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, authorization);
        Ok(Self {
            api: forgejo_api::Forgejo::new(Auth::Token(token), url.clone()).map_err(err)?,
            http: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .map_err(|error| Error::Forge(error.to_string()))?,
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

    /// Revoke temporary ForgeClaw tokens left by a previous daemon process.
    /// Call before this process begins accepting turns.
    pub async fn revoke_stale_tokens(&self) -> Result<usize> {
        let username = self.me().await?;
        let api = self.token_api().await?;
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut page = 1_u32;
        loop {
            let (headers, tokens) = go(api
                .user_get_tokens(&username)
                .page(page)
                .page_size(TOKEN_PAGE_SIZE))
            .await?;
            let count = tokens.len();
            for token in tokens {
                let id = token
                    .id
                    .ok_or_else(|| Error::Forge("listed token has no id".into()))?;
                if !seen.insert(id) {
                    return Err(Error::Forge("token list repeated an earlier page".into()));
                }
                if token.name.as_deref().is_some_and(temporary_token_name) {
                    ids.push(id);
                }
            }
            if headers
                .x_total_count
                .is_some_and(|total| seen.len() >= total.max(0) as usize)
            {
                break;
            }
            if count < TOKEN_PAGE_SIZE as usize {
                if headers
                    .x_total_count
                    .is_some_and(|total| seen.len() < total.max(0) as usize)
                {
                    return Err(Error::Forge("token list ended before total count".into()));
                }
                break;
            }
            page = page
                .checked_add(1)
                .ok_or_else(|| Error::Forge("too many token pages".into()))?;
        }
        for id in &ids {
            go(api.user_delete_access_token(&username, &id.to_string())).await?;
        }
        Ok(ids.len())
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

    async fn run_context(&self, repo: &RepoId, pr: u64) -> Option<Value> {
        let run = self.latest_run(repo, pr).await?;
        let mut context = json!({
            "id": run.id,
            "status": run.status,
            "url": run.html_url,
            "workflow": run.workflow_id,
            "jobs": [],
        });
        let Some(run_id) = run.id else {
            return Some(context);
        };
        let (owner, name) = own(repo);
        let Ok(jobs) = self.api.list_action_run_jobs(owner, name, run_id).await else {
            return Some(context);
        };
        let mut remaining = EXCERPT_MAX;
        let mut summaries = Vec::with_capacity(jobs.len());
        for job in jobs {
            let mut summary = json!({"id": job.id, "name": job.name, "status": job.status});
            if remaining > 0
                && matches!(job.status.as_deref(), Some("running" | "failure"))
                && let Some(id) = job.id
                && let Ok(log) = self
                    .api
                    .repo_get_action_job_logs(owner, name, id, Default::default())
                    .await
            {
                let log = clip_tail_to(log, remaining);
                remaining -= log.len();
                summary["log"] = log.into();
            }
            summaries.push(summary);
        }
        context["jobs"] = summaries.into();
        Some(context)
    }

    async fn reviews(&self, owner: &str, name: &str, pr: i64) -> Result<Vec<Value>> {
        let (_, reviews) = go(self.api.repo_list_pull_reviews(owner, name, pr)).await?;
        Ok(reviews
            .iter()
            .rev()
            .take(3)
            .map(|review| {
                json!({
                    "id": review.id,
                    "author": login(&review.user),
                    "state": review.state,
                    "body": excerpt(&text(&review.body), 2048),
                    "comments_count": review.comments_count,
                })
            })
            .collect())
    }

    pub async fn review_page(
        &self,
        thread: &ThreadKey,
        review_id: u64,
        offset: usize,
    ) -> Result<Value> {
        let Subject::Pr(number) = thread.subject else {
            return Err(Error::Forge(
                "review requires a pull request subject".into(),
            ));
        };
        let review_id =
            i64::try_from(review_id).map_err(|_| Error::Forge("review id is too large".into()))?;
        let (owner, name) = own(&thread.repo);
        let review = go(self
            .api
            .repo_get_pull_review(owner, name, number as i64, review_id))
        .await?;
        let comments =
            go(self
                .api
                .repo_get_pull_review_comments(owner, name, number as i64, review_id))
            .await?;
        let inline: Vec<Value> = comments
            .iter()
            .skip(offset)
            .take(REVIEW_COMMENTS_PAGE)
            .map(|comment| {
                let hunk = text(&comment.diff_hunk);
                let (hunk_old_start, hunk_new_start) = hunk_lines(&hunk);
                json!({
                    "id": comment.id,
                    "author": login(&comment.user),
                    "path": comment.path,
                    "hunk_old_start": hunk_old_start,
                    "hunk_new_start": hunk_new_start,
                    "diff_position": comment.position,
                    "body": text(&comment.body),
                    "resolved": comment.resolver.is_some(),
                    "diff_hunk": excerpt_tail(&hunk, 1200),
                })
            })
            .collect();
        Ok(json!({
            "id": review.id,
            "author": login(&review.user),
            "state": review.state,
            "body": text(&review.body),
            "comments": inline,
            "next_offset": (offset + REVIEW_COMMENTS_PAGE < comments.len()).then_some(offset + REVIEW_COMMENTS_PAGE),
        }))
    }

    pub async fn review_page_by_reviewer(
        &self,
        thread: &ThreadKey,
        reviewer: &str,
        offset: usize,
    ) -> Result<Value> {
        let Subject::Pr(number) = thread.subject else {
            return Err(Error::Forge(
                "review requires a pull request subject".into(),
            ));
        };
        let (owner, name) = own(&thread.repo);
        let (_, reviews) = go(self.api.repo_list_pull_reviews(owner, name, number as i64)).await?;
        let review_id = reviews
            .iter()
            .rev()
            .find(|review| login(&review.user).eq_ignore_ascii_case(reviewer))
            .and_then(|review| review.id)
            .ok_or_else(|| Error::Forge(format!("no review by {reviewer} on {thread}")))?;
        let review_id =
            u64::try_from(review_id).map_err(|_| Error::Forge("review id is invalid".into()))?;
        self.review_page(thread, review_id, offset).await
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
        let comment_count = comments.len();
        let comments: Vec<Value> = comments
            .iter().rev().take(3)
            .map(|comment| json!({"id": comment.id, "author": login(&comment.user), "body": excerpt(&text(&comment.body), 2048)}))
            .collect();
        let body = text(&issue.body);
        let mut context = json!({
            "title": text(&issue.title),
            "body": excerpt(&body, BODY_PAGE_MAX),
            "body_truncated": body.len() > BODY_PAGE_MAX,
            "author": login(&issue.user),
            "state": issue.state,
            "url": issue.html_url,
            "comments": comments,
            "comments_count": comment_count,
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
            if let Some(run) = self.latest_run(&thread.repo, number).await {
                context["ci_run"] = json!({"id": run.id, "status": run.status, "url": run.html_url, "workflow": run.workflow_id});
            }
            context["reviews"] = json!(self.reviews(owner, name, number as i64).await?);
        }
        Ok(context)
    }

    pub async fn body_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage> {
        let (owner, name) = own(&thread.repo);
        let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
        let issue = go(self.api.issue_get_issue(owner, name, number as i64)).await?;
        page(&text(&issue.body), offset, BODY_PAGE_MAX)
    }

    pub async fn comment_page(
        &self,
        thread: &ThreadKey,
        offset: usize,
        body_offset: usize,
    ) -> Result<Value> {
        let (owner, name) = own(&thread.repo);
        let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
        let (_, comments) =
            go(self
                .api
                .issue_get_comments(owner, name, number as i64, Default::default()))
            .await?;
        let comment = comments
            .iter()
            .rev()
            .nth(offset)
            .ok_or_else(|| Error::Forge("comment offset is outside the discussion".into()))?;
        let body = page(&text(&comment.body), body_offset, BODY_PAGE_MAX)?;
        Ok(json!({
            "id": comment.id,
            "author": login(&comment.user),
            "body": body.text,
            "next_body_offset": body.next_offset,
            "next_offset": (offset + 1 < comments.len()).then_some(offset + 1),
        }))
    }

    pub async fn ci_context(&self, thread: &ThreadKey) -> Result<Value> {
        let Subject::Pr(number) = thread.subject else {
            return Err(Error::Forge("CI requires a pull request subject".into()));
        };
        Ok(self
            .run_context(&thread.repo, number)
            .await
            .unwrap_or(Value::Null))
    }

    pub async fn ci_log_page(
        &self,
        thread: &ThreadKey,
        job_id: u64,
        offset: usize,
    ) -> Result<DiffPage> {
        let Subject::Pr(number) = thread.subject else {
            return Err(Error::Forge("CI requires a pull request subject".into()));
        };
        let run = self
            .latest_run(&thread.repo, number)
            .await
            .ok_or_else(|| Error::Forge("pull request has no CI run".into()))?;
        let run_id = run
            .id
            .ok_or_else(|| Error::Forge("CI run has no id".into()))?;
        let job_id =
            i64::try_from(job_id).map_err(|_| Error::Forge("job id is out of range".into()))?;
        let (owner, name) = own(&thread.repo);
        let jobs = go(self.api.list_action_run_jobs(owner, name, run_id)).await?;
        if !jobs.iter().any(|job| job.id == Some(job_id)) {
            return Err(Error::Forge(
                "job is not in the pull request's latest CI run".into(),
            ));
        }
        let log = go(self
            .api
            .repo_get_action_job_logs(owner, name, job_id, Default::default()))
        .await?;
        page(&log, offset, CI_LOG_PAGE_MAX)
    }

    pub async fn diff_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage> {
        let Subject::Pr(number) = thread.subject else {
            return Err(Error::Forge("diff requires a pull request subject".into()));
        };
        let (owner, name) = own(&thread.repo);
        let diff = go(self.api.repo_download_pull_diff_or_patch(
            owner,
            name,
            number as i64,
            "diff",
            Default::default(),
        ))
        .await?;
        page(&diff, offset, DIFF_PAGE_MAX)
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

    pub async fn edit_pr(&self, repo: &RepoId, number: u64, update: PrUpdate) -> Result<()> {
        let (owner, name) = own(repo);
        let options =
            args(serde_json::to_value(update).map_err(|error| Error::Forge(error.to_string()))?);
        go(self
            .api
            .repo_edit_pull_request(owner, name, number as i64, options))
        .await?;
        Ok(())
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

    pub async fn add_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        let (owner, name) = own(&thread.repo);
        let options = args(json!({"content": emoji}));
        if let Some(id) = comment_id {
            go(self
                .api
                .issue_post_comment_reaction(owner, name, id as i64, options))
            .await?;
        } else {
            let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
            go(self
                .api
                .issue_post_issue_reaction(owner, name, number as i64, options))
            .await?;
        }
        Ok(())
    }

    pub async fn remove_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        let (owner, name) = own(&thread.repo);
        let path = if let Some(id) = comment_id {
            format!("api/v1/repos/{owner}/{name}/issues/comments/{id}/reactions")
        } else {
            let (Subject::Issue(number) | Subject::Pr(number)) = thread.subject;
            format!("api/v1/repos/{owner}/{name}/issues/{number}/reactions")
        };
        let url = self.url.join(&path).map_err(err)?;
        let response = self
            .http
            .delete(url)
            .json(&json!({"content": emoji}))
            .send()
            .await
            .map_err(err)?;
        if !response.status().is_success() {
            return Err(Error::Forge(format!(
                "could not remove reaction on {thread}: HTTP {}",
                response.status()
            )));
        }
        Ok(())
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

    pub async fn resolve_review_comment(&self, thread: &ThreadKey, comment_id: u64) -> Result<()> {
        let Subject::Pr(pr) = thread.subject else {
            return Err(Error::Forge(
                "review comments require a pull request".into(),
            ));
        };
        let comment_id_i64 = i64::try_from(comment_id)
            .map_err(|_| Error::Forge("review comment id is too large".into()))?;
        let (owner, name) = own(&thread.repo);
        let (_, reviews) = go(self.api.repo_list_pull_reviews(owner, name, pr as i64)).await?;
        let mut found = false;
        for review_id in reviews.into_iter().filter_map(|review| review.id) {
            let comments = go(self
                .api
                .repo_get_pull_review_comments(owner, name, pr as i64, review_id))
            .await?;
            if comments
                .iter()
                .any(|comment| comment.id == Some(comment_id_i64))
            {
                found = true;
                break;
            }
        }
        if !found {
            return Err(Error::Forge(format!(
                "review comment {comment_id} not found on {thread}"
            )));
        }
        let url = self
            .url
            .join(&format!(
                "api/v1/repos/{owner}/{name}/pulls/comments/{comment_id}/resolve"
            ))
            .map_err(|error| Error::Forge(error.to_string()))?;
        let response = self
            .http
            .post(url)
            .send()
            .await
            .map_err(|error| Error::Forge(error.to_string()))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(Error::Forge(format!(
                "could not resolve review comment {comment_id}: HTTP {}",
                response.status()
            )));
        }
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

    async fn review_page(
        &self,
        thread: &ThreadKey,
        review_id: u64,
        offset: usize,
    ) -> Result<Value> {
        Forgejo::review_page(self, thread, review_id, offset).await
    }

    async fn review_page_by_reviewer(
        &self,
        thread: &ThreadKey,
        reviewer: &str,
        offset: usize,
    ) -> Result<Value> {
        Forgejo::review_page_by_reviewer(self, thread, reviewer, offset).await
    }

    async fn body_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage> {
        Forgejo::body_page(self, thread, offset).await
    }

    async fn comment_page(
        &self,
        thread: &ThreadKey,
        offset: usize,
        body_offset: usize,
    ) -> Result<Value> {
        Forgejo::comment_page(self, thread, offset, body_offset).await
    }

    async fn ci_context(&self, thread: &ThreadKey) -> Result<Value> {
        Forgejo::ci_context(self, thread).await
    }

    async fn ci_log_page(
        &self,
        thread: &ThreadKey,
        job_id: u64,
        offset: usize,
    ) -> Result<DiffPage> {
        Forgejo::ci_log_page(self, thread, job_id, offset).await
    }

    async fn diff_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage> {
        Forgejo::diff_page(self, thread, offset).await
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

    async fn edit_pr(&self, repo: &RepoId, number: u64, update: PrUpdate) -> Result<()> {
        Forgejo::edit_pr(self, repo, number, update).await
    }

    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64> {
        Forgejo::comment(self, thread, body, reply_to).await
    }

    async fn add_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        Forgejo::add_reaction(self, thread, comment_id, emoji).await
    }

    async fn remove_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        Forgejo::remove_reaction(self, thread, comment_id, emoji).await
    }

    async fn resolve_review_comment(&self, thread: &ThreadKey, comment_id: u64) -> Result<()> {
        Forgejo::resolve_review_comment(self, thread, comment_id).await
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

fn clip_tail_to(mut value: String, limit: usize) -> String {
    if value.len() > limit {
        let mut start = value.len() - limit;
        while !value.is_char_boundary(start) {
            start += 1;
        }
        value.replace_range(..start, "");
    }
    value
}

fn excerpt(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn excerpt_tail(value: &str, limit: usize) -> &str {
    let mut start = value.len().saturating_sub(limit);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

fn page(value: &str, offset: usize, limit: usize) -> Result<DiffPage> {
    if offset > value.len() || !value.is_char_boundary(offset) {
        return Err(Error::Forge(
            "offset is outside the text or not a UTF-8 boundary".into(),
        ));
    }
    let mut end = value.len().min(offset.saturating_add(limit));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Ok(DiffPage {
        text: value[offset..end].into(),
        next_offset: (end < value.len()).then_some(end),
    })
}

fn hunk_lines(hunk: &str) -> (Option<u64>, Option<u64>) {
    let Some(header) = hunk.lines().find(|line| line.starts_with("@@ ")) else {
        return (None, None);
    };
    let mut fields = header.split_whitespace();
    let _ = fields.next();
    let parse = |prefix: char, value: Option<&str>| {
        value
            .and_then(|value| value.strip_prefix(prefix))
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    (parse('-', fields.next()), parse('+', fields.next()))
}

#[cfg(test)]
mod token_name_tests {
    use super::temporary_token_name;

    #[test]
    fn matches_only_issued_token_names() {
        let id = "11111111-2222-3333-4444-555555555555";
        assert!(temporary_token_name(&format!("forgeclaw-{id}")));
        assert!(temporary_token_name(&format!("forgeclaw-temp-{id}")));
        assert!(temporary_token_name(&format!(
            "forgeclaw-temp-create-pr-{id}"
        )));
        assert!(!temporary_token_name("forgeclaw-smoke-disposable"));
        assert!(!temporary_token_name("forgeclaw-temp-someone-elses-token"));
    }
}

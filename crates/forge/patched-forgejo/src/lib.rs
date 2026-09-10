//! Patched-Forgejo backend: the stock Forgejo `Forge` impl plus the endpoints
//! only DaniD3v's fork adds. It composes [`forgeclaw_forgejo::Forgejo`] and
//! forwards every trait method to it, overriding `resolve_conversation` to call
//! the fork's `POST …/pulls/comments/{id}/resolve` route.
//!
//! The fork's other patch — the `mention` webhook — needs no code here: its
//! payload carries the same `issue`/`pull_request`/`repository` fields
//! [`forgeclaw_forgejo::webhook_thread`] already routes on, so a mention
//! delivery targets its thread through the shared decoder unchanged. The
//! backend exists for `resolve_conversation`; the mention event only has to be
//! subscribed on the webhook (a deploy concern).

use async_trait::async_trait;
use forgeclaw_core::{
    Error, Forge, ForgeEvent, IssueSummary, NewPr, PrPatch, RepoId, Result, Review, ScopedToken,
    ThreadKey,
};
use forgeclaw_forgejo::Forgejo;
use serde_json::Value;
use url::Url;

pub struct PatchedForgejo {
    inner: Forgejo,
    http: reqwest::Client,
    url: Url,
    /// The bot's access token, replayed on the raw resolve call the
    /// `forgejo-api` client can't reach.
    token: String,
}

impl PatchedForgejo {
    pub fn new(url: Url, token: &str, password: Option<String>) -> Result<Self> {
        Ok(Self {
            inner: Forgejo::new(url.clone(), token, password)?,
            http: reqwest::Client::new(),
            url,
            token: token.to_owned(),
        })
    }
}

#[async_trait]
impl Forge for PatchedForgejo {
    /// Mark the review conversation rooted at `comment_id` resolved, via the
    /// fork-only endpoint. No body; the fork returns 204.
    async fn resolve_conversation(&self, repo: &RepoId, comment_id: u64) -> Result<()> {
        let endpoint = format!(
            "{}/api/v1/repos/{}/{}/pulls/comments/{}/resolve",
            self.url.as_str().trim_end_matches('/'),
            repo.owner,
            repo.name,
            comment_id,
        );
        let resp = self
            .http
            .post(&endpoint)
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await
            .map_err(err)?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Error::Forge(format!(
                "resolve conversation {comment_id}: HTTP {}",
                resp.status()
            )))
        }
    }

    // ── Everything else is stock Forgejo ──────────────────────────────────
    async fn whoami(&self) -> Result<String> {
        self.inner.whoami().await
    }
    async fn context(&self, thread: &ThreadKey) -> Result<Value> {
        self.inner.context(thread).await
    }
    async fn repo_config(&self, repo: &RepoId) -> Result<Option<(String, String)>> {
        self.inner.repo_config(repo).await
    }
    async fn repo_file(&self, repo: &RepoId, path: &str) -> Result<Option<(String, String)>> {
        self.inner.repo_file(repo, path).await
    }
    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId> {
        self.inner.ensure_fork(repo).await
    }
    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>> {
        self.inner.search_issues(repo, query).await
    }
    async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64> {
        self.inner.create_issue(repo, title, body).await
    }
    async fn close_issue(&self, repo: &RepoId, number: u64, comment: Option<&str>) -> Result<()> {
        self.inner.close_issue(repo, number, comment).await
    }
    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64> {
        self.inner.create_pr(repo, pr).await
    }
    async fn update_pr(&self, repo: &RepoId, number: u64, patch: PrPatch) -> Result<()> {
        self.inner.update_pr(repo, number, patch).await
    }
    async fn add_dependency(&self, repo: &RepoId, blocked: u64, blocked_by: u64) -> Result<()> {
        self.inner.add_dependency(repo, blocked, blocked_by).await
    }
    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64> {
        self.inner.comment(thread, body, reply_to).await
    }
    async fn edit_comment(&self, repo: &RepoId, id: u64, body: &str) -> Result<()> {
        self.inner.edit_comment(repo, id, body).await
    }
    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()> {
        self.inner.submit_review(repo, pr, review).await
    }
    async fn mint_token(&self, label: &str) -> Result<ScopedToken> {
        self.inner.mint_token(label).await
    }
    async fn revoke_token(&self, token: &ScopedToken) -> Result<()> {
        self.inner.revoke_token(token).await
    }
    async fn resync(&self, me: &str) -> Result<Vec<ForgeEvent>> {
        self.inner.resync(me).await
    }
    async fn resync_thread(&self, me: &str, thread: &ThreadKey) -> Result<Vec<ForgeEvent>> {
        self.inner.resync_thread(me, thread).await
    }
    async fn closed_event(&self, thread: &ThreadKey) -> Result<Option<ForgeEvent>> {
        self.inner.closed_event(thread).await
    }
    async fn ack(&self, event: &ForgeEvent) -> Result<()> {
        self.inner.ack(event).await
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::Forge(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn repo() -> RepoId {
        "o/r".parse().unwrap()
    }

    #[tokio::test]
    async fn resolve_posts_to_the_fork_endpoint_with_token_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/o/r/pulls/comments/42/resolve"))
            .and(header("Authorization", "token s3cret"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let forge = PatchedForgejo::new(server.uri().parse().unwrap(), "s3cret", None).unwrap();
        forge.resolve_conversation(&repo(), 42).await.unwrap();
    }

    #[tokio::test]
    async fn resolve_surfaces_a_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let forge = PatchedForgejo::new(server.uri().parse().unwrap(), "s3cret", None).unwrap();
        let err = forge.resolve_conversation(&repo(), 7).await.unwrap_err();
        assert!(matches!(err, Error::Forge(m) if m.contains("404")));
    }
}

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

use crate::{IssueSummary, NewPr, PrUpdate, RepoId, Result, Review, ThreadKey};

/// Forge-neutral operations used by the daemon. Each forge lives in its own
/// adapter crate and implements this boundary.
#[async_trait]
pub trait Forge: Send + Sync {
    /// Create the same forge adapter authenticated with a task-scoped token.
    fn with_token(&self, token: &str) -> Result<Arc<dyn Forge>>;

    async fn whoami(&self) -> Result<String>;
    async fn context(&self, thread: &ThreadKey) -> Result<Value>;
    async fn review_page(&self, thread: &ThreadKey, review_id: u64, offset: usize)
    -> Result<Value>;
    async fn review_page_by_reviewer(
        &self,
        thread: &ThreadKey,
        reviewer: &str,
        offset: usize,
    ) -> Result<Value>;
    async fn body_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage>;
    async fn comment_page(
        &self,
        thread: &ThreadKey,
        offset: usize,
        body_offset: usize,
    ) -> Result<Value>;
    async fn ci_context(&self, thread: &ThreadKey) -> Result<Value>;
    async fn ci_log_page(&self, thread: &ThreadKey, job_id: u64, offset: usize)
    -> Result<DiffPage>;
    async fn diff_page(&self, thread: &ThreadKey, offset: usize) -> Result<DiffPage>;
    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId>;
    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>>;
    async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64>;
    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64>;
    async fn edit_pr(&self, repo: &RepoId, number: u64, update: PrUpdate) -> Result<()>;
    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64>;
    async fn add_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()>;
    async fn remove_reaction(
        &self,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()>;
    async fn resolve_review_comment(&self, thread: &ThreadKey, comment_id: u64) -> Result<()>;
    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()>;

    /// Create and revoke the credential attached to one routed agent turn.
    async fn mint_token(&self, label: &str) -> Result<ScopedToken>;
    async fn revoke_token(&self, token: &ScopedToken) -> Result<()>;
}

#[derive(Debug, Serialize)]
pub struct DiffPage {
    pub text: String,
    pub next_offset: Option<usize>,
}

#[derive(Clone)]
pub struct ScopedToken {
    pub id: i64,
    pub secret: String,
}

impl std::fmt::Debug for ScopedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScopedToken")
            .field("id", &self.id)
            .field("secret", &"[redacted]")
            .finish()
    }
}

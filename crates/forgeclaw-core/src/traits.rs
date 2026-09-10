use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{RepoId, Result, ThreadKey};

/// Everything the bot and the forge tool server need from a forge; one impl
/// per forge crate.
#[async_trait]
pub trait Forge: Send + Sync {
    /// The bot's own username — `@me` in trigger filters.
    async fn whoami(&self) -> Result<String>;

    /// Template variables for a subject (title, body, comments, diff,
    /// ci_log, url …), fetched bot-side before delivery.
    async fn context(&self, thread: &ThreadKey) -> Result<Value>;

    /// The bot-owned fork of `repo`, created if absent; `repo` unchanged when
    /// the bot already owns it. Lets the bot open PRs without write access to
    /// the upstream repo.
    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId>;

    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>>;
    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64>;
    /// Comment on the subject; `reply_to` targets a review thread.
    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64>;
    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()>;

    /// Fresh short-lived token for one task; revoked at task end.
    async fn mint_token(&self, label: &str) -> Result<ScopedToken>;
    async fn revoke_token(&self, token: &ScopedToken) -> Result<()>;
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct NewPr {
    pub title: String,
    pub body: String,
    /// Head branch; the base is always the repo's default branch.
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub verdict: Verdict,
    pub summary: String,
    pub inline: Vec<InlineComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct InlineComment {
    pub path: String,
    pub line: u64,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct ScopedToken {
    pub id: i64,
    pub secret: String,
}

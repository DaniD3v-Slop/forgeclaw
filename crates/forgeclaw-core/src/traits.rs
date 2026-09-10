use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, ForgeEvent, RepoId, Result, ThreadKey};

/// Everything the bot and the forge tool server need from a forge; one impl
/// per forge crate.
#[async_trait]
pub trait Forge: Send + Sync {
    /// The bot's own username — `@me` in trigger filters.
    async fn whoami(&self) -> Result<String>;

    /// Template variables for a subject (title, body, comments, diff,
    /// ci_log, url …), fetched bot-side before delivery.
    async fn context(&self, thread: &ThreadKey) -> Result<Value>;

    /// `.forgebot.toml` content and blob sha, from the default branch only —
    /// a PR must never reconfigure the bot reviewing it.
    async fn repo_config(&self, repo: &RepoId) -> Result<Option<(String, String)>>;

    /// A file's text content and blob sha from the repo's default branch, or
    /// `None` when absent. Used to detect `.forgebot/Containerfile` changes and
    /// rebuild the per-repo image. Default `None` suits fakes with no filesystem.
    async fn repo_file(&self, repo: &RepoId, path: &str) -> Result<Option<(String, String)>> {
        let _ = (repo, path);
        Ok(None)
    }

    /// The bot-owned fork of `repo`, created if absent; `repo` unchanged when
    /// the bot already owns it. Lets the bot open PRs without write access to
    /// the upstream repo.
    async fn ensure_fork(&self, repo: &RepoId) -> Result<RepoId>;

    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<IssueSummary>>;
    async fn create_issue(&self, repo: &RepoId, title: &str, body: &str) -> Result<u64>;
    async fn close_issue(&self, repo: &RepoId, number: u64, comment: Option<&str>) -> Result<()>;
    async fn create_pr(&self, repo: &RepoId, pr: NewPr) -> Result<u64>;
    async fn update_pr(&self, repo: &RepoId, number: u64, patch: PrPatch) -> Result<()>;
    /// Mark `blocked` (a PR or issue) as blocked by issue `blocked_by`,
    /// gating its merge until `blocked_by` closes.
    async fn add_dependency(&self, repo: &RepoId, blocked: u64, blocked_by: u64) -> Result<()>;
    /// Comment on the subject; `reply_to` targets a review thread.
    async fn comment(&self, thread: &ThreadKey, body: &str, reply_to: Option<u64>) -> Result<u64>;
    async fn edit_comment(&self, repo: &RepoId, id: u64, body: &str) -> Result<()>;
    async fn submit_review(&self, repo: &RepoId, pr: u64, review: Review) -> Result<()>;
    /// Mark the PR review conversation rooted at review comment `comment_id`
    /// resolved. A forge capability, not a universal one: the default reports
    /// it unsupported, and only a backend whose forge exposes the
    /// resolve-conversation API overrides it.
    async fn resolve_conversation(&self, _repo: &RepoId, _comment_id: u64) -> Result<()> {
        Err(Error::Forge(
            "this forge backend does not support resolving review conversations".into(),
        ))
    }

    /// Fresh short-lived token for one task; revoked at task end.
    async fn mint_token(&self, label: &str) -> Result<ScopedToken>;
    async fn revoke_token(&self, token: &ScopedToken) -> Result<()>;

    /// Level-triggered resync: events for everything currently awaiting the
    /// bot (unread notifications, assigned issues, review requests, failed
    /// CI on bot PRs, merged PRs referencing open bot issues).
    async fn resync(&self, me: &str) -> Result<Vec<ForgeEvent>>;
    /// [`resync`](Self::resync) scoped to one thread — the low-latency path a
    /// webhook poke triggers. Best-effort: an event whose forge state hasn't
    /// materialized yet (a comment not yet on the notification cursor) is
    /// simply absent, and the next full `resync` catches it.
    async fn resync_thread(&self, me: &str, thread: &ThreadKey) -> Result<Vec<ForgeEvent>>;
    /// A cheap single fetch: `Some(*.closed)` when the subject is closed (so
    /// its thread must retire), else `None`. The retirement correctness-net —
    /// the full-resync feeds are all open-state, so this is the only pass that
    /// revisits a closed subject whose close webhook was missed.
    async fn closed_event(&self, thread: &ThreadKey) -> Result<Option<ForgeEvent>>;
    /// Advance the resync cursor for an event (e.g. mark its notification
    /// read) — called only after its task is Done.
    async fn ack(&self, event: &ForgeEvent) -> Result<()>;
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

#[derive(Debug, Clone, Default)]
pub struct PrPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub draft: Option<bool>,
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

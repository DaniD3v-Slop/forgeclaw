//! Webhook signature verification plus the shared `ForgeEvent` builders. The
//! receiver no longer normalizes a full delivery into events — it verifies the
//! HMAC and extracts which thread changed ([`webhook_thread`]); the reconciler
//! derives the events by resyncing that thread. The builders stay: `resync`
//! and `resync_thread` synthesize events through them.

use std::collections::BTreeSet;

use forgeclaw_core::{Error, ForgeEvent, RepoId, Result, Subject, ThreadKey};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

/// Verify `X-Forgejo-Signature`: HMAC-SHA256 hex over the raw body, compared
/// in constant time.
pub(crate) fn verify_signature(secret: &str, signature_hex: &str, raw_body: &[u8]) -> Result<()> {
    let sig = hex::decode(signature_hex)
        .map_err(|_| Error::Forge("malformed webhook signature".into()))?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac takes any key length");
    mac.update(raw_body);
    mac.verify_slice(&sig)
        .map_err(|_| Error::Forge("webhook signature mismatch".into()))
}

fn event(
    repo: &RepoId,
    kind: &str,
    subject: Subject,
    actor: &str,
    event_id: String,
    payload: Value,
) -> ForgeEvent {
    ForgeEvent {
        repo: repo.clone(),
        kind: kind.into(),
        subject,
        actor: actor.into(),
        event_id,
        payload,
    }
}

/// Per-kind builders shared by resync and its targeted variant so both
/// synthesize byte-identical `(kind, subject, event_id, payload)` for the same
/// logical event — the idempotency key depends on it.
pub(crate) fn issue_assigned(
    repo: &RepoId,
    subject: Subject,
    assignee: &str,
    author: &str,
    title: &str,
    body: &str,
    actor: &str,
) -> ForgeEvent {
    let (Subject::Issue(number) | Subject::Pr(number)) = subject;
    event(
        repo,
        "issue.assigned",
        subject,
        actor,
        format!("assigned-{number}-{assignee}"),
        json!({"assignee": assignee, "author": author, "title": title, "body": body}),
    )
}

pub(crate) fn review_requested(
    repo: &RepoId,
    pr: u64,
    reviewer: &str,
    author: &str,
    title: &str,
    actor: &str,
) -> ForgeEvent {
    event(
        repo,
        "pull_request.review_requested",
        Subject::Pr(pr),
        actor,
        format!("revreq-{pr}-{reviewer}"),
        json!({"reviewer": reviewer, "author": author, "title": title}),
    )
}

pub(crate) fn changes_requested(
    repo: &RepoId,
    pr: u64,
    review: u64,
    author: &str,
    body: &str,
) -> ForgeEvent {
    event(
        repo,
        "pull_request.changes_requested",
        Subject::Pr(pr),
        author,
        format!("review-{review}"),
        json!({"reviewer": author, "body": body}),
    )
}

pub(crate) fn ci_run_completed(
    repo: &RepoId,
    pr: u64,
    pr_author: &str,
    conclusion: &str,
    workflow: &str,
    run: (u64, &str),
    actor: &str,
) -> ForgeEvent {
    let (run_id, run_url) = run;
    event(
        repo,
        "ci.run_completed",
        Subject::Pr(pr),
        actor,
        format!("run-{run_id}"),
        json!({"conclusion": conclusion, "pr_author": pr_author, "pr": pr,
               "workflow": workflow, "run_url": run_url}),
    )
}

pub(crate) fn referenced_pr_merged(
    repo: &RepoId,
    issue: u64,
    issue_author: &str,
    pr: u64,
    pr_title: &str,
    pr_url: &str,
    actor: &str,
) -> ForgeEvent {
    event(
        repo,
        "issue.referenced_pr_merged",
        Subject::Issue(issue),
        actor,
        format!("prmerged-{pr}-{issue}"),
        json!({"issue_author": issue_author, "pr": pr,
               "pr_title": pr_title, "pr_url": pr_url}),
    )
}

/// A bot-authored PR's own open event — the auto-review trigger. The paired
/// body-mention comment the live webhook once emitted is derived separately in
/// `resync_thread` from the PR's body state (`body_mention_event`).
pub(crate) fn pull_request_opened(
    repo: &RepoId,
    pr: u64,
    author: &str,
    title: &str,
    body: &str,
) -> ForgeEvent {
    event(
        repo,
        "pull_request.opened",
        Subject::Pr(pr),
        author,
        format!("opened-{pr}"),
        json!({"author": author, "title": title, "body": body}),
    )
}

/// A bot-authored PR whose every blocker has been resolved — the resume signal.
/// Forgejo fires no dependency-resolved webhook, so the periodic resync re-emits
/// this each pass; the `unblocked-{pr}` key makes it at-most-once per PR because
/// the store dedupes on it.
pub(crate) fn pull_request_unblocked(
    repo: &RepoId,
    pr: u64,
    author: &str,
    title: &str,
) -> ForgeEvent {
    event(
        repo,
        "pull_request.unblocked",
        Subject::Pr(pr),
        author,
        format!("unblocked-{pr}"),
        json!({"author": author, "title": title, "pr": pr}),
    )
}

/// A subject's own close event — the thread-retirement signal. For a PR the
/// `merged` flag distinguishes a merge from a plain close.
pub(crate) fn subject_closed(repo: &RepoId, subject: Subject, merged: bool) -> ForgeEvent {
    let (kind, n) = match subject {
        Subject::Issue(n) => ("issue.closed", n),
        Subject::Pr(n) => ("pull_request.closed", n),
    };
    event(
        repo,
        kind,
        subject,
        "",
        format!("closed-{n}"),
        json!({"merged": merged}),
    )
}

/// A comment-bearing unread notification as a `comment.created` event.
/// `actor`/`author` are the comment author; `extra` carries the path-specific
/// fields (`url`, `_notification`, `assignees`) merged over the shared core.
/// `key` is the idempotency id within the thread: the comment id.
pub(crate) fn comment_created(
    repo: &RepoId,
    subject: Subject,
    key: String,
    actor: &str,
    author: &str,
    body: &str,
    extra: Value,
) -> ForgeEvent {
    let mut payload = json!({"author": author, "body": body, "mentions": mentions(body)});
    let Value::Object(map) = extra else {
        unreachable!("comment extras are always an object")
    };
    payload
        .as_object_mut()
        .expect("payload is an object")
        .extend(map);
    event(repo, "comment.created", subject, actor, key, payload)
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Repo {
    full_name: String,
}

#[derive(Deserialize)]
struct Numbered {
    number: u64,
}

#[derive(Deserialize)]
struct Run {
    #[serde(default)]
    event_payload: String,
    repository: Option<Repo>,
}

/// The one payload envelope the receiver still parses: enough to name which
/// thread changed. Everything else is derived by resyncing that thread.
#[derive(Deserialize)]
struct Hook {
    issue: Option<Numbered>,
    pull_request: Option<Numbered>,
    run: Option<Run>,
    repository: Option<Repo>,
}

/// Verify the HMAC, then extract which thread the delivery touched.
///
/// `Ok(Some(key))` names a thread to resync; `Ok(None)` is a valid delivery
/// that names no thread we can target (the caller falls back to a full
/// resync); `Err` is a bad signature or malformed body — no poke.
///
/// A free function, not a `Forge` method: it reads no instance state, and the
/// mention/assign/comment payloads every Forgejo variant sends share this one
/// envelope — the patched backend routes its mention deliveries through it
/// unchanged.
pub fn webhook_thread(
    signature_hex: &str,
    secret: &str,
    raw_body: &[u8],
) -> Result<Option<ThreadKey>> {
    verify_signature(secret, signature_hex, raw_body)?;
    let h: Hook = serde_json::from_slice(raw_body)
        .map_err(|e| Error::Forge(format!("webhook payload: {e}")))?;
    // A PR shares the issue index space; `pull_request` present means the PR
    // thread even when an `issue` field also rides along.
    let subject = if let Some(p) = &h.pull_request {
        Some(Subject::Pr(p.number))
    } else if let Some(i) = &h.issue {
        Some(Subject::Issue(i.number))
    } else if let Some(run) = &h.run {
        pr_in_payload(&run.event_payload).map(|(pr, _)| Subject::Pr(pr))
    } else {
        None
    };
    // Ordinary Forgejo webhooks put the repository at the top level. Action
    // run webhooks put it inside `run` instead.
    let repo = h
        .repository
        .as_ref()
        .or_else(|| h.run.as_ref().and_then(|run| run.repository.as_ref()))
        .and_then(|r| r.full_name.parse().ok());
    Ok(subject
        .zip(repo)
        .map(|(subject, repo)| ThreadKey { repo, subject }))
}

/// True when position `i` sits directly after a word character — `a@b.c` or
/// `sha#9` must not count as a mention/reference.
fn mid_word(text: &str, i: usize) -> bool {
    text[..i]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
}

/// PR number and author from an action run's embedded trigger payload; `None`
/// when the run was not triggered by a pull request.
pub(crate) fn pr_in_payload(event_payload: &str) -> Option<(u64, String)> {
    #[derive(Deserialize)]
    struct Pr {
        number: u64,
        user: User,
    }
    #[derive(Deserialize)]
    struct RunTrigger {
        pull_request: Option<Pr>,
    }
    let h: RunTrigger = serde_json::from_str(event_payload).ok()?;
    let pr = h.pull_request?;
    Some((pr.number, pr.user.login))
}

/// Usernames `@`-mentioned in `text`, deduplicated and sorted.
pub(crate) fn mentions(text: &str) -> Vec<String> {
    let set: BTreeSet<&str> = text
        .match_indices('@')
        .filter_map(|(i, _)| {
            let rest = &text[i + 1..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
                .unwrap_or(rest.len());
            let name = rest[..end].trim_end_matches(['.', '-']);
            (!mid_word(text, i) && !name.is_empty()).then_some(name)
        })
        .collect();
    set.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn signature_roundtrip_and_rejections() {
        let body = br#"{"action":"opened"}"#;
        assert!(verify_signature("s3cret", &sign("s3cret", body), body).is_ok());
        assert!(verify_signature("s3cret", &sign("s3cret", body), b"tampered").is_err());
        assert!(verify_signature("other", &sign("s3cret", body), body).is_err());
        assert!(verify_signature("s3cret", "not-hex", body).is_err());
    }

    fn thread(payload: &Value) -> Option<ThreadKey> {
        let body = payload.to_string().into_bytes();
        webhook_thread(&sign("s", &body), "s", &body).unwrap()
    }

    fn repo() -> Value {
        json!({"full_name": "o/r"})
    }

    #[test]
    fn bad_signature_yields_err_no_poke() {
        let body = br#"{"issue":{"number":7},"repository":{"full_name":"o/r"}}"#;
        assert!(webhook_thread("deadbeef", "s", body).is_err());
    }

    #[test]
    fn issue_delivery_targets_the_issue_thread() {
        let payload = json!({"action": "assigned", "issue": {"number": 7},
                             "repository": repo()});
        assert_eq!(
            thread(&payload),
            Some(ThreadKey {
                repo: "o/r".parse().unwrap(),
                subject: Subject::Issue(7),
            })
        );
    }

    #[test]
    fn pull_request_wins_over_issue_field() {
        // Comment webhooks carry an `issue` field even for a PR; the
        // `pull_request` presence must decide the subject.
        let payload = json!({"action": "created", "issue": {"number": 4},
                             "pull_request": {"number": 4}, "repository": repo()});
        assert_eq!(thread(&payload).unwrap().subject, Subject::Pr(4));
    }

    #[test]
    fn action_run_targets_the_prs_thread() {
        let inner = json!({"pull_request": {"number": 5, "user": {"login": "bot"}}});
        let payload = json!({"action": "failure",
                             "run": {"event_payload": inner.to_string(),
                                     "repository": repo()}});
        assert_eq!(
            thread(&payload),
            Some(ThreadKey {
                repo: "o/r".parse().unwrap(),
                subject: Subject::Pr(5),
            })
        );
    }

    #[test]
    fn threadless_or_unroutable_deliveries_yield_none() {
        // A push-triggered run names no PR; a release names no thread at all.
        let push = json!({"action": "failure",
                          "run": {"event_payload": "{\"ref\":\"refs/heads/main\"}"},
                          "repository": repo()});
        assert_eq!(thread(&push), None);
        assert_eq!(thread(&json!({"action": "published"})), None);
    }

    #[test]
    fn mention_parsing() {
        assert_eq!(
            mentions("@bot fix, then ping @a-b.c (@bot again)"),
            ["a-b.c", "bot"]
        );
        assert_eq!(mentions("mail me a@b.c, half@"), Vec::<String>::new());
        assert_eq!(mentions("@bot."), ["bot"]);
    }
}

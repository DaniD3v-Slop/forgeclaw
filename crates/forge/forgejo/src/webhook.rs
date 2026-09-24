//! Verification and normalization of one Forgejo webhook delivery.

use std::collections::BTreeSet;

use forgeclaw_core::{Error, ForgeEvent, RepoId, Result, Subject};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

/// Verify and translate only the event carried by this delivery. Unsupported
/// webhook kinds are valid and produce no events.
pub fn webhook_events(signature: &str, secret: &str, body: &[u8]) -> Result<Vec<ForgeEvent>> {
    verify_signature(secret, signature, body)?;
    let hook: Value = serde_json::from_slice(body)
        .map_err(|error| Error::Forge(format!("webhook payload: {error}")))?;
    let repo: RepoId = hook
        .pointer("/repository/full_name")
        .or_else(|| hook.pointer("/run/repository/full_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Forge("webhook payload has no repository".into()))?
        .parse()?;
    let action = string(&hook, "/action");
    let mut events = Vec::new();

    if matches!(action.as_str(), "created" | "edited")
        && let Some(comment) = hook.get("comment")
        && let Some(subject) = subject(&hook)
    {
        let author = user(comment, "/user");
        let body = string(comment, "/body");
        let mut extra = json!({
            "url": comment.get("html_url"),
            "assignees": assignees(&hook),
        });
        if matches!(subject, Subject::Pr(_))
            && (comment.get("pull_request_review_id").is_some()
                || comment.get("path").is_some()
                || comment.get("position").is_some())
            && let Some(id) = number(comment, "/id")
        {
            extra["reply_to"] = id.into();
        }
        events.push(comment_created(&repo, subject, &author, &body, extra));
    }

    if action == "assigned"
        && let Some(subject) = subject(&hook)
        && let Some(item) = hook.get("pull_request").or_else(|| hook.get("issue"))
    {
        events.push(event(
            &repo,
            "issue.assigned",
            subject,
            json!({
                // Forgejo's assignment webhook does not identify the changed
                // assignee separately. The post-change issue/PR does contain
                // the complete list, which is sufficient for @me filtering.
                "assignees": assignees(&hook),
                "author": user(item, "/user"),
                "title": string(item, "/title"),
                "body": string(item, "/body"),
            }),
        ));
    }

    if action == "review_requested"
        && let Some(pr) = hook.get("pull_request")
        && let Some(pr_number) = number(pr, "/number")
    {
        let reviewer = user(&hook, "/requested_reviewer");
        events.push(event(
            &repo,
            "pull_request.review_requested",
            Subject::Pr(pr_number),
            json!({
                "reviewer": reviewer,
                "author": user(pr, "/user"),
                "title": string(pr, "/title"),
            }),
        ));
    }

    if let Some(review) = hook.get("review")
        && let Some(pr) = hook.get("pull_request")
        && let Some(pr_number) = number(pr, "/number")
    {
        let review_type = string(review, "/type");
        let reviewer = user(&hook, "/sender");
        let body = string(review, "/content");
        if review_type == "pull_request_review_rejected" {
            events.push(event(
                &repo,
                "pull_request.changes_requested",
                Subject::Pr(pr_number),
                json!({"reviewer": reviewer, "body": body}),
            ));
        } else if review_type == "pull_request_review_comment" {
            events.push(comment_created(
                &repo,
                Subject::Pr(pr_number),
                &reviewer,
                &body,
                json!({"assignees": assignees(&hook)}),
            ));
        }
    }

    if let Some(run) = hook.get("run")
        && let Some((pr, author)) = pr_in_payload(&string(run, "/event_payload"))
    {
        let conclusion = ["conclusion", "status"]
            .into_iter()
            .find_map(|field| run.get(field).and_then(Value::as_str))
            .unwrap_or(&action);
        events.push(event(
            &repo,
            "ci.run_completed",
            Subject::Pr(pr),
            json!({
                "conclusion": conclusion,
                "pr_author": author,
                "pr": pr,
                "workflow": run.get("workflow_id"),
                "run_url": run.get("html_url"),
            }),
        ));
    }

    if action == "opened"
        && let Some(pr) = hook.get("pull_request")
        && let Some(number) = number(pr, "/number")
    {
        let author = user(pr, "/user");
        events.push(event(
            &repo,
            "pull_request.opened",
            Subject::Pr(number),
            json!({
                "author": author,
                "title": string(pr, "/title"),
                "body": string(pr, "/body"),
            }),
        ));
    }

    // A mention in an issue or PR description has no comment object. Treat an
    // opened/edited description as the same conversational event as a comment.
    if matches!(action.as_str(), "opened" | "edited")
        && hook.get("comment").is_none()
        && let Some(subject) = subject(&hook)
        && let Some(item) = hook.get("pull_request").or_else(|| hook.get("issue"))
    {
        let body = string(item, "/body");
        if !mentions(&body).is_empty() {
            let author = user(item, "/user");
            events.push(comment_created(
                &repo,
                subject,
                &author,
                &body,
                json!({"assignees": assignees(&hook)}),
            ));
        }
    }

    Ok(events)
}

fn verify_signature(secret: &str, signature: &str, body: &[u8]) -> Result<()> {
    let signature =
        hex::decode(signature).map_err(|_| Error::Forge("malformed webhook signature".into()))?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac takes any key length");
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| Error::Forge("webhook signature mismatch".into()))
}

fn event(repo: &RepoId, kind: &str, subject: Subject, payload: Value) -> ForgeEvent {
    ForgeEvent {
        repo: repo.clone(),
        kind: kind.into(),
        subject,
        payload,
    }
}

fn comment_created(
    repo: &RepoId,
    subject: Subject,
    author: &str,
    body: &str,
    extra: Value,
) -> ForgeEvent {
    let mut payload = json!({"author": author, "body": body, "mentions": mentions(body)});
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.clone());
    }
    event(repo, "comment.created", subject, payload)
}

fn subject(hook: &Value) -> Option<Subject> {
    hook.pointer("/pull_request/number")
        .and_then(Value::as_u64)
        .map(Subject::Pr)
        .or_else(|| {
            hook.pointer("/issue/number")
                .and_then(Value::as_u64)
                .map(Subject::Issue)
        })
}

fn assignees(hook: &Value) -> Vec<String> {
    hook.get("pull_request")
        .or_else(|| hook.get("issue"))
        .and_then(|item| item.get("assignees"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| user(value, ""))
        .filter(|login| !login.is_empty())
        .collect()
}

fn user(value: &Value, pointer: &str) -> String {
    let value = if pointer.is_empty() {
        value
    } else {
        value.pointer(pointer).unwrap_or(&Value::Null)
    };
    value
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}

fn string(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}

fn number(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

fn pr_in_payload(payload: &str) -> Option<(u64, String)> {
    let payload: Value = serde_json::from_str(payload).ok()?;
    Some((
        payload.pointer("/pull_request/number")?.as_u64()?,
        user(&payload, "/pull_request/user"),
    ))
}

fn mentions(text: &str) -> Vec<String> {
    let mentions: BTreeSet<&str> = text
        .match_indices('@')
        .filter_map(|(index, _)| {
            let previous_is_word = text[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
            let rest = &text[index + 1..];
            let end = rest
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || "-_.".contains(character))
                })
                .unwrap_or(rest.len());
            let login = rest[..end].trim_end_matches(['.', '-']);
            (!previous_is_word && !login.is_empty()).then_some(login)
        })
        .collect();
    mentions.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn normalize(payload: Value) -> Vec<ForgeEvent> {
        let body = payload.to_string();
        webhook_events(&sign(body.as_bytes()), "secret", body.as_bytes()).unwrap()
    }

    #[test]
    fn comment_delivery_is_normalized_without_a_resync() {
        let events = normalize(json!({
            "action": "created",
            "repository": {"full_name": "o/r"},
            "sender": {"login": "alice"},
            "issue": {"number": 7, "assignees": [{"login": "bot"}]},
            "comment": {"id": 42, "body": "hi @bot", "user": {"login": "alice"}}
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "comment.created");
        assert_eq!(events[0].payload["mentions"], json!(["bot"]));
    }

    #[test]
    fn edited_comments_use_the_existing_mention_rules() {
        for body in ["hi @bot", "mention removed"] {
            for item in ["issue", "pull_request"] {
                let events = normalize(json!({
                    "action": "edited",
                    "repository": {"full_name": "o/r"},
                    (item): {"number": 7},
                    "comment": {"id": 42, "body": body, "user": {"login": "alice"}}
                }));
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].kind, "comment.created");
                assert_eq!(
                    events[0].subject,
                    if item == "issue" {
                        Subject::Issue(7)
                    } else {
                        Subject::Pr(7)
                    }
                );
                assert_eq!(events[0].payload["author"], "alice");
                assert_eq!(events[0].payload["body"], body);
                assert_eq!(
                    events[0].payload["mentions"],
                    if body == "hi @bot" {
                        json!(["bot"])
                    } else {
                        json!([])
                    }
                );
            }
        }
    }

    #[test]
    fn edited_inline_comment_preserves_reply_target() {
        let events = normalize(json!({
            "action": "edited", "repository": {"full_name": "o/r"},
            "pull_request": {"number": 7},
            "comment": {"id": 42, "path": "README.md", "body": "hi @bot", "user": {"login": "alice"}}
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["reply_to"], 42);
    }

    #[test]
    fn deleted_comment_does_not_trigger_a_turn() {
        assert!(
            normalize(json!({
                "action": "deleted", "repository": {"full_name": "o/r"},
                "issue": {"number": 7},
                "comment": {"id": 42, "body": "hi @bot", "user": {"login": "alice"}}
            }))
            .is_empty()
        );
    }

    #[test]
    fn assignment_delivery_exposes_the_post_change_assignees() {
        let events = normalize(json!({
            "action": "assigned",
            "repository": {"full_name": "o/r"},
            "sender": {"login": "alice"},
            "issue": {
                "number": 7,
                "title": "work",
                "user": {"login": "alice"},
                "assignees": [{"login": "bot"}]
            }
        }));
        assert_eq!(events[0].kind, "issue.assigned");
        assert_eq!(events[0].payload["assignees"], json!(["bot"]));
    }

    #[test]
    fn forgejo_rejected_review_uses_its_actual_payload_shape() {
        let events = normalize(json!({
            "action": "reviewed",
            "repository": {"full_name": "o/r"},
            "sender": {"login": "reviewer"},
            "pull_request": {"number": 8, "user": {"login": "alice"}},
            "review": {
                "type": "pull_request_review_rejected",
                "content": "please fix this"
            }
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "pull_request.changes_requested");
        assert_eq!(events[0].payload["reviewer"], "reviewer");
        assert_eq!(events[0].payload["body"], "please fix this");
    }

    #[test]
    fn forgejo_review_comment_is_a_conversation_event() {
        let events = normalize(json!({
            "action": "reviewed",
            "repository": {"full_name": "o/r"},
            "sender": {"login": "reviewer"},
            "pull_request": {"number": 8, "user": {"login": "alice"}},
            "review": {
                "type": "pull_request_review_comment",
                "content": "hi @bot"
            }
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "comment.created");
        assert_eq!(events[0].payload["mentions"], json!(["bot"]));
    }

    #[test]
    fn unsupported_delivery_is_ignored() {
        assert!(
            normalize(json!({
                "action": "published",
                "repository": {"full_name": "o/r"}
            }))
            .is_empty()
        );
    }

    #[test]
    fn signature_is_required() {
        assert!(webhook_events("deadbeef", "secret", b"{}").is_err());
    }

    #[test]
    fn mention_parsing_ignores_email_addresses() {
        assert_eq!(mentions("@bot and @a-b.c; not a@b.c"), ["a-b.c", "bot"]);
    }
}

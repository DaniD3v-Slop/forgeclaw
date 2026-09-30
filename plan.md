# ForgeClaw

ForgeClaw turns selected Forgejo webhooks into OpenClaw teammate turns.

## Architecture

```text
Forgejo webhook
  -> Rust daemon: verify, normalize, filter, grant one subject
  -> OpenClaw session
  -> forge_* plugin tool
  -> Rust daemon
  -> Forgejo API
```

After verifying and translating the signed payload, the HTTP handler persists
the delivery in a SQLite outbox before returning `202 Accepted`. The daemon
retries failed deliveries after a restart. A successful subject turn is marked
complete before the next subject in the same delivery runs, so a later failure
does not repeat it. A crash between a successful turn and that mark can still
repeat the turn; delivery is at least once.

The stable session key is:

```text
agent:main:<forge>/<owner>/<repo>#<issue-or-pr>
```

During the turn, the daemon grants that exact session permission to write only
the triggering subject. The Forgejo credential remains inside the daemon. The
JavaScript plugin forwards the trusted OpenClaw session key in an authenticated
request; session identity is never a model-controlled tool argument. Ad-hoc
sessions may read but cannot write.

Temporary Forgejo tokens left by a crashed daemon are revoked on startup.

## Supported webhook triggers

- created issue and pull-request comments, including review comments;
- issue assignment;
- pull-request review requests;
- requested-changes reviews;
- completed CI runs associated with a pull request;
- newly opened pull requests;
- mentions in newly opened or edited issue and pull-request descriptions.

Events that require polling or reconstructing historical forge state are not
supported.

## Tools

- `forge_read`
- `forge_read_review`
- `forge_read_body`
- `forge_read_comment`
- `forge_read_ci`
- `forge_read_diff`
- `forge_search_issues`
- `forge_comment`
- `forge_resolve_review_comment`
- `forge_create_issue`
- `forge_create_pr`
- `forge_edit_pr`
- `forge_submit_review`
- `forge_checkout`
- `forge_push`

The trigger editor can preview draft rules against a sample event without
starting an agent turn. Its outbox status shows pending deliveries and the last
failure; operators can retry pending deliveries after fixing the cause.

All forge behavior and authorization remain in Rust. The OpenClaw plugin only
registers these tools, forwards calls, and serves the trigger editor.

## Local deployment

`deploy/compose.yaml` runs Forgejo, OpenClaw, and the daemon. It seeds the
OpenClaw configuration on a new volume. The Forgejo Actions runner is optional
under the `ci` profile. See `deploy/README.md` for first-start credentials and
webhook setup.

## Verification

Before completion:

1. Run `cargo fmt --all -- --check`.
2. Run `cargo clippy --all-targets -- -D warnings`.
3. Run `cargo test`.
4. Validate plugin syntax and Compose rendering.
5. Start the local stack and verify its health endpoints.
6. Deliver a signed live Forgejo webhook and verify exactly one OpenClaw turn.
7. Verify an ungranted read succeeds and cross-subject writes fail.

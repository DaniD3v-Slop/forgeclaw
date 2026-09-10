# ForgeClaw implementation plan

## Goal

ForgeClaw provides teammate-style Forgejo automation through OpenClaw while keeping the
system small:

- OpenClaw owns sessions, queueing, model execution, and the agent workspace.
- One Rust daemon owns webhook handling, deterministic triggers, authorization grants, and
  all forge operations.
- One thin OpenClaw JavaScript plugin registers native forge tools and forwards the trusted
  OpenClaw session identity to the Rust daemon. It also serves the browser-only trigger
  editor registered as an OpenClaw Control UI tab.

Forgejo is the compiled and end-to-end-tested backend. Other forges can be added behind the
`Forge` trait.

## Constraints

- Forge writes are authorized for one exact issue or pull request and one OpenClaw session.
- Sessions without a matching grant are read-only.
- The agent cannot provide or override its own session identity.
- JavaScript is integration glue only. Authorization, prompts, trigger rules, webhook
  verification, and forge behavior remain in Rust.
- Credentials and generated tokens stay outside the repository.
- Completion requires format, lint, unit tests, and live Compose end-to-end tests.

## Architecture

```text
Forgejo webhook
      |
      v
Rust daemon: verify -> normalize -> trigger -> mint scoped token/grant
      |                                      |
      | starts an OpenClaw turn              | authorizes exact session + subject
      v                                      v
OpenClaw session -> native forge_* tool -> JS plugin -> POST /tools/call -> Rust daemon
                         trusted context.sessionKey ----^              -> Forgejo API
```

The JavaScript plugin receives `context.sessionKey` from OpenClaw's trusted tool factory
context. It sends that value in `X-ForgeClaw-Session-Key`; it never takes session identity
from model-controlled arguments. The authenticated daemon maps the session to its in-memory
grant and rejects cross-subject writes. Read operations remain available without a grant.

This avoids the unsupported MCP metadata/SQLite workaround and removes the extra session
initialization turn. A new forge thread can use tools on its first real turn.

## Components

### Rust forge layer

`forgeclaw-core::Forge` defines subject context, issue and pull-request operations, review
operations, repository checkout/push, scoped-token lifecycle, webhook normalization, and
resynchronization. `forgeclaw-forgejo` implements the trait against Forgejo.

### Rust daemon

The daemon exposes:

- `POST /webhook` for Forgejo webhooks.
- `POST /tools/call` for authenticated calls from the OpenClaw plugin.
- `GET /healthz` for local readiness checks.

For an engaged event it derives a stable key:

```text
agent:main:forgeclaw:<forge>/<owner>/<repo>#<issue-or-pr>
```

It mints a short-lived Forgejo token, stores a grant for that exact session and subject,
submits the real event prompt to OpenClaw, and revokes the token when the turn ends.

### OpenClaw plugin

The installable plugin contains the config schema, teammate skill, a ForgeClaw Control UI
page, and seven native tools:

- `forge_read`
- `forge_search_issues`
- `forge_comment`
- `forge_create_pr`
- `forge_submit_review`
- `forge_checkout`
- `forge_push`

The plugin performs no forge authorization logic. Its runtime jobs are adapting OpenClaw
tool calls to the daemon's authenticated HTTP endpoint, attaching the trusted session key,
and serving the gateway-protected browser editor. The editor persists validated trigger
configuration through OpenClaw's config API; the Rust daemon reloads those rules before
handling each webhook.

### Compose playground

`deploy/compose.yaml` starts an isolated Forgejo, runner, OpenClaw gateway, workspace volume,
and ForgeClaw daemon. The gateway's browser origins include both loopback forms of its
published port. Secrets are supplied through an external environment file.

## Trigger behavior

The reference configuration handles:

- mentioned issue and PR comments;
- issue assignment to the bot;
- pull-request review requests;
- requested changes;
- failed CI runs on bot work.

The editor exposes every actionable event the Forgejo adapter emits: those five defaults,
referenced-PR merges, unblocked pull requests, and newly opened pull requests. Rules can be
enabled or disabled and filters retain the previous configuration model: conditions inside
a group are ANDed, alternative groups are ORed, and `is not` maps to a negated pattern.
Transport addresses and credential environment-variable names remain under Advanced rather
than appearing in the everyday trigger editor.

The Rust gate decides whether to engage. The skill decides the appropriate teammate action:
answer directly, implement on a fork and open a PR, submit a review, update an existing
branch, or repair failed CI. Inline review replies use the review-comment ID exposed by
`forge_read` as `forge_comment.reply_to`.

## Verification bar

Before completion:

1. Run `cargo fmt --all`.
2. Run `cargo clippy --all-targets -- -D warnings`.
3. Run `cargo test`.
4. Validate the plugin syntax and OpenClaw config/plugin load.
5. Start the isolated Compose stack and verify all health endpoints and cold-restart
   persistence.
6. Prove an ungranted read succeeds, a missing daemon credential is rejected, and a
   cross-subject write is rejected.
7. On live Forgejo, verify:
   - a brand-new thread acts on its first turn with no setup message;
   - a mentioned Q&A comment receives one direct answer;
   - assignment creates and pushes bot work and opens a PR;
   - a change request updates that existing branch;
   - an inline-review reply is stored in the same review thread;
   - a review request produces one submitted verdict;
   - failed CI triggers repair behavior.
8. In a real browser, verify the ForgeClaw sidebar page, all event/filter controls,
   add-disable-save-delete behavior, CSRF rejection, and a saved rule driving a live event.
9. Inspect final service logs and repository status, then distribute broad edits to their
   mutable Jujutsu ancestors with `jj absorb`.

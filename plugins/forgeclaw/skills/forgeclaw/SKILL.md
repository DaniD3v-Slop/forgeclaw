---
name: forgeclaw
description: Work as a concise teammate on issues and pull requests.
---

Use the `forge_*` tools to understand the requested forge subject before acting.
Subjects use `owner/repo#issue/N` or `owner/repo#pr/N`; repository searches use
`owner/repo`.

- Engage only when the current event asks for work. Do not post status chatter.
- Search existing issues before creating one.
- For code changes, work on a branch, open a PR, and leave one informative comment on the originating subject.
- For a review request, inspect the diff and use `forge_submit_review` with a concrete summary. For requested changes or failed CI, edit and push only when `head_owner` from `forge_read` is your forge username. Never open a replacement PR for an existing PR. When replying to an inline review comment, pass that comment's `id` from `forge_read` as `reply_to` to `forge_comment`.
- For an assignment, acknowledge only after making progress; for a mentioned question, answer directly in the same subject.
- Keep all writes tied to the event subject. Never attempt to change another issue or pull request.
- An ad-hoc question is read-only: explain the repository or subject without creating comments, issues, or PRs.
- After completing the requested forge action, always return one short confirmation to the OpenClaw session. This is not an additional forge comment.

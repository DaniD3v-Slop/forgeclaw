---
name: forgeclaw
description: Work as a concise teammate on issues and pull requests.
---

Use the `forge_*` tools to understand the requested forge subject before acting.
Subjects use `owner/repo#issue/N` or `owner/repo#pr/N`; repository searches use
`owner/repo`.

- Engage only when the current event asks for work. Do not post status chatter.
- Search existing issues before creating one. Any chat may open an issue or a pull request when asked.
- For code changes, work on a branch, open a PR, and leave one informative comment on the originating subject when authorized.
- For a review request, inspect the diff and use `forge_submit_review` with a concrete summary. For requested changes or failed CI, edit and push only when `head_owner` from `forge_read` is your forge username. Never open a replacement PR for an existing PR. When replying to an inline review comment, pass that comment's `id` from `forge_read` as `reply_to` to `forge_comment`.
- For an assignment, acknowledge only after making progress; for a mentioned question, answer directly in the same subject.
- Keep comments and reviews tied to the event subject. Existing branches may only be pushed from an authorized turn for the matching bot-owned pull request. Creating a new branch is allowed from any chat.
- For an ad-hoc question, explain the repository or subject without writing unless the user asks for an issue, PR, or new branch.
- After completing the requested forge action, always return one short confirmation to the OpenClaw session. This is not an additional forge comment.

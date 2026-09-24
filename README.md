# ForgeClaw

ForgeClaw turns selected Forgejo events into OpenClaw agent sessions. A Rust daemon verifies signed webhooks and handles Forgejo operations; an OpenClaw plugin exposes the agent tools and trigger editor.

The image is published from `main` to `ghcr.io/danid3v-slop/forgeclaw`. It contains the daemon and plugin on top of OpenClaw `2026.9.5-browser`. Use an immutable `sha-<commit>` tag or image digest for deployment. The plugin files are at `/opt/forgeclaw/plugin`.

See [deploy/README.md](deploy/README.md) for the local Compose setup.

## A triggered turn has a read-only checkout

If a Forgejo event starts an agent turn, `forge_read` works, but `forge_checkout`
returns an `-readonly` path and `forge_comment` reports `write is not authorized
for this subject`, check the repository spelling in the webhook and the OpenClaw
session key. Older ForgeClaw images kept the webhook's mixed-case owner/repository
name in the write grant, while OpenClaw lowercased the session key. The exact
match failed even during the authorized turn. This was fixed in
[`5990a400`](https://github.com/DaniD3v-Slop/forgeclaw/commit/5990a40081c9ba33ac2c410ebaebb1d94d0a06f0).

Deploy an image containing that fix to both the OpenClaw gateway and ForgeClaw
daemon, then retrigger the Forgejo event. Verify that the turn receives a
writable checkout and can write only to its issue or pull request. A later
message in the same OpenClaw chat is read-only after the event turn ends; that
is the expected grant lifetime.

Comment creation and edits both use the `comment.created` trigger rules. Adding
`@forgeclaw` in an edit can start a turn; editing a comment that still mentions
the bot can start another. Deleted comments are ignored.

Any chat can open an issue or pull request and create a branch in the bot fork.
Updating an existing branch requires an authorized turn for its matching
bot-owned pull request.

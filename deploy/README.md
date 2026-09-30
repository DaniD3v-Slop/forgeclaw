# Local deployment

Copy `.env.example` to `.env` and set `OPENCLAW_GATEWAY_TOKEN` and
`FORGECLAW_MCP_AUTHORIZATION` to independently generated random values. Keep
that file untracked.

On a new Forgejo volume, start Forgejo alone first:

```sh
podman compose up -d forgejo
```

Create the bot account in the local Forgejo instance and create an access token
with `read:repository`, `read:issue`, and `read:user` scopes. Register a
webhook pointing to `http://forgeclaw:3080/webhook`. Put the bot password,
access token, and webhook secret in the corresponding `.env` fields. The
password is used only to mint and revoke a scoped token for each routed turn.
Forgejo must allow the 🧑‍🍳 and 🍳 reactions for turn status; the local Compose
service enables both.
Then start the base stack:

```sh
podman compose up -d
```

Verified webhook deliveries are saved to
`/home/node/.openclaw/forgeclaw-outbox.sqlite` in the persistent `openclaw_data`
volume before ForgeClaw responds with `202`. Failed deliveries are retried
automatically, including after a daemon restart. If storage is unavailable,
ForgeClaw responds with `503` so Forgejo can redeliver. Processing is at least
once: a crash after an OpenClaw turn succeeds but before its completion is
recorded can repeat that turn.

The OpenClaw configuration and ForgeClaw plugin path are seeded automatically
on the first start. ForgeClaw does not add a permanent Control UI navigation
item. Open its occasional-use trigger editor at
`http://127.0.0.1:18790/plugins/forgeclaw/`.

To run Forgejo Actions too, generate an instance-level runner registration
token if the runner should serve every repository (including repositories
created later). A repository-level token restricts the runner to that one
repository. Set `FORGEJO_RUNNER_REGISTRATION_TOKEN` and enable the optional
profile:

```sh
podman compose --profile ci up -d
```

Forgejo may hold workflow runs from forked pull requests for manual approval.
Approve those runs in the repository's Actions UI when you trust the proposed
workflow changes.

## Mock OpenClaw turn

With the local stack and bot credentials above running, use `./test-mock.sh`.
It starts a separate OpenClaw gateway and ForgeClaw daemon whose model provider
is a local deterministic HTTP server. The check creates a disposable public
Forgejo repository, sends a signed comment webhook, verifies the model was
called and the comment changes from 🧑‍🍳 to 🍳, then deletes the repository.
The normal gateway's model configuration and sessions are not changed. No model
API key is needed for this test.

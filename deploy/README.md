# Local deployment

Copy `.env.example` to `.env` and set `OPENCLAW_GATEWAY_TOKEN` and
`FORGECLAW_MCP_AUTHORIZATION` to independently generated random values. Keep
that file untracked.

On a new Forgejo volume, start Forgejo alone first:

```sh
podman compose up -d forgejo
```

Create the bot account in the local Forgejo instance and create an access token
with `write:repository`, `write:issue`, and `read:user` scopes. Register a
webhook pointing to `http://forgeclaw:3080/webhook`. Put the access token and
webhook secret in the corresponding `.env` fields.
Then start the base stack:

```sh
podman compose up -d
```

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

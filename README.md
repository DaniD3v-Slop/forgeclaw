# ForgeClaw

ForgeClaw turns selected Forgejo events into OpenClaw agent sessions. A Rust daemon verifies signed webhooks and handles Forgejo operations; an OpenClaw plugin exposes the agent tools and trigger editor.

The image is published from `main` to `ghcr.io/danid3v-slop/forgeclaw`. It contains the daemon and plugin on top of OpenClaw `2026.9.5-browser`. Use an immutable `sha-<commit>` tag or image digest for deployment. The plugin files are at `/opt/forgeclaw/plugin`.

See [deploy/README.md](deploy/README.md) for the local Compose setup.

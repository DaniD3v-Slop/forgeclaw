#!/bin/sh
set -eu
cd "$(dirname "$0")"
podman compose -f compose.yaml -f compose.mock.yaml --profile mock up -d mock-model openclaw-mock-gateway forgeclaw-mock

# The mock daemon has its own persistent OpenClaw device identity. Bootstrap its
# pairing request before sending the webhook; the mock gateway is isolated.
podman exec forgeclaw-forgeclaw-mock-1 node openclaw.mjs gateway call sessions.create --params '{"key":"agent:main:forgeclaw-mock:pairing","category":"ForgeClaw"}' --json >/dev/null 2>&1 || :
request_id=$(podman exec forgeclaw-openclaw-mock-gateway-1 node openclaw.mjs devices list --json | podman exec -i forgeclaw-openclaw-mock-gateway-1 node -e '
  let input = "";
  process.stdin.on("data", chunk => input += chunk);
  process.stdin.on("end", () => {
    const pending = JSON.parse(input).pending;
    if (pending.length > 1 || pending.some(item => item.clientId !== "cli" || item.role !== "operator" || !item.scopes.includes("operator.write"))) {
      throw new Error("unexpected mock gateway pairing request");
    }
    if (pending.length) process.stdout.write(pending[0].requestId);
  });
')
if [ -n "$request_id" ]; then
  podman exec forgeclaw-openclaw-mock-gateway-1 node openclaw.mjs devices approve "$request_id" >/dev/null
fi
podman compose -f compose.yaml -f compose.mock.yaml --profile mock run --rm mock-check

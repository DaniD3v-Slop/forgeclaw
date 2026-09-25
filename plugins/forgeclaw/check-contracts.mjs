import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const root = new URL("./", import.meta.url);
const manifest = JSON.parse(readFileSync(new URL("openclaw.plugin.json", root), "utf8"));
const source = readFileSync(new URL("index.js", root), "utf8");
const definitions = source.match(/const tools = \[([\s\S]*?)\n\];/)?.[1];
assert.ok(definitions, "tool definitions not found");
const names = [...definitions.matchAll(/name: "(forge_[a-z_]+)"/g)].map((match) => match[1]);
assert.deepEqual(manifest.contracts.tools, names, "plugin tool contract is out of sync");

import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";

const UI_ROOT = "/plugins/forgeclaw";
const UI_WRITE_PATH = "/api/forgeclaw/config";
const AUTHORIZATION_CACHE = Symbol.for("forgeclaw.authorization-cache");
const UI_CSRF = randomBytes(24).toString("hex");
const UI_PAGE = readFileSync(new URL("./ui.html", import.meta.url), "utf8").replaceAll(
  "__FORGECLAW_CSRF__",
  UI_CSRF,
);
const MAX_CONFIG_BODY = 64 * 1024;
const EVENT_FIELDS = {
  "comment.created": new Set(["mentions", "assignees", "author", "body"]),
  "issue.assigned": new Set(["assignees", "author"]),
  "pull_request.review_requested": new Set(["reviewer", "author"]),
  "pull_request.changes_requested": new Set(["reviewer", "body"]),
  "ci.run_completed": new Set(["conclusion", "pr_author", "workflow"]),
  "pull_request.opened": new Set(["author"]),
};

const string = { type: "string" };
const subject = {
  type: "string",
  description: "Exact forge subject: owner/repo#issue/N or owner/repo#pr/N.",
};
const repository = {
  type: "string",
  description: "Forge repository in owner/repo form.",
};

const tools = [
  {
    name: "forge_read",
    label: "Read forge subject",
    description: "Read a forge issue or pull request.",
    parameters: object({ subject }, ["subject"]),
  },
  {
    name: "forge_search_issues",
    label: "Search forge issues",
    description: "Search issues and pull requests in a repository.",
    parameters: object({ repo: repository, query: string }, ["repo", "query"]),
  },
  {
    name: "forge_comment",
    label: "Comment on forge subject",
    description:
      "Comment on the authorized issue or pull request. When answering an inline review comment, pass its id from forge_read as reply_to.",
    parameters: object(
      { subject, body: string, reply_to: { type: "integer" } },
      ["subject", "body"],
    ),
  },
  {
    name: "forge_create_issue",
    label: "Create issue",
    description: "Open an issue in a repository.",
    parameters: object({ repo: repository, title: string, body: string }, ["repo", "title", "body"]),
  },
  {
    name: "forge_create_pr",
    label: "Create pull request",
    description: "Open a pull request from an existing branch in a repository.",
    parameters: object(
      { repo: repository, title: string, body: string, branch: string },
      ["repo", "title", "body", "branch"],
    ),
  },
  {
    name: "forge_submit_review",
    label: "Submit pull request review",
    description:
      "Submit an approve, request_changes, or comment review on the authorized pull request.",
    parameters: object(
      {
        subject,
        verdict: { type: "string", enum: ["approve", "request_changes", "comment"] },
        summary: string,
      },
      ["subject", "verdict", "summary"],
    ),
  },
  {
    name: "forge_checkout",
    label: "Check out forge repository",
    description:
      "Clone the bot fork into the shared agent workspace for a forge subject.",
    parameters: object({ subject }, ["subject"]),
  },
  {
    name: "forge_push",
    label: "Push forge branch",
    description: "Create a new branch from the subject checkout. Updating an existing branch requires authority over its pull request.",
    parameters: object({ subject, branch: string }, ["subject", "branch"]),
  },
];

function object(properties, required) {
  return { type: "object", additionalProperties: false, properties, required };
}

function takeAuthorization(name) {
  let cache = globalThis[AUTHORIZATION_CACHE];
  if (!(cache instanceof Map)) {
    cache = new Map();
    Object.defineProperty(globalThis, AUTHORIZATION_CACHE, { value: cache });
  }
  const current = process.env[name];
  if (current) {
    cache.set(name, current);
  }
  delete process.env[name];
  return cache.get(name);
}

function createTool(definition, context, config, authorization) {
  return {
    ...definition,
    resultContentSource: "network",
    async execute(_toolCallId, arguments_, signal) {
      if (!authorization) {
        throw new Error(`${config.authorization_env} is not set in the OpenClaw gateway`);
      }
      const headers = {
        Authorization: authorization,
        "Content-Type": "application/json",
      };
      if (context.sessionKey) {
        headers["X-ForgeClaw-Session-Key"] = context.sessionKey;
      }
      const response = await fetch(`${config.daemon_url.replace(/\/$/, "")}/tools/call`, {
        method: "POST",
        headers,
        body: JSON.stringify({ name: definition.name, arguments: arguments_ }),
        signal,
      });
      const payload = await response.json();
      if (!response.ok) {
        throw new Error(payload.error ?? `ForgeClaw daemon returned HTTP ${response.status}`);
      }
      return payload;
    },
  };
}

function json(res, status, value, cors = false) {
  res.writeHead(status, {
    "Content-Type": "application/json; charset=utf-8",
    "Cache-Control": "no-store",
    ...(cors
      ? {
          "Access-Control-Allow-Origin": "null",
          "Access-Control-Allow-Credentials": "true",
          Vary: "Origin",
        }
      : {}),
  });
  res.end(JSON.stringify(value));
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > MAX_CONFIG_BODY) {
        reject(new Error("configuration payload is too large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function validateTriggers(value) {
  if (!Array.isArray(value) || value.length > 64) {
    throw new Error("triggers must be an array with at most 64 entries");
  }
  return value.map((trigger, index) => {
    if (!trigger || typeof trigger !== "object" || Array.isArray(trigger)) {
      throw new Error(`trigger ${index + 1} must be an object`);
    }
    const keys = Object.keys(trigger);
    if (keys.some((key) => !["on", "enabled", "filter"].includes(key))) {
      throw new Error(`trigger ${index + 1} contains an unknown field`);
    }
    if (typeof trigger.on !== "string" || !EVENT_FIELDS[trigger.on]) {
      throw new Error(`trigger ${index + 1} has an invalid event`);
    }
    if (trigger.enabled !== undefined && typeof trigger.enabled !== "boolean") {
      throw new Error(`trigger ${index + 1} has an invalid enabled value`);
    }
    const clauses = trigger.filter === undefined
      ? []
      : Array.isArray(trigger.filter) ? trigger.filter : [trigger.filter];
    if (clauses.length > 16) {
      throw new Error(`trigger ${index + 1} has too many alternative filter groups`);
    }
    for (const clause of clauses) {
      if (!clause || typeof clause !== "object" || Array.isArray(clause)) {
        throw new Error(`trigger ${index + 1} has an invalid filter group`);
      }
      if (Object.keys(clause).length === 0) {
        throw new Error(`trigger ${index + 1} has an empty filter group`);
      }
      if (Object.keys(clause).length > 16) {
        throw new Error(`trigger ${index + 1} has too many filter conditions`);
      }
      for (const [field, pattern] of Object.entries(clause)) {
        if (
          !EVENT_FIELDS[trigger.on].has(field) ||
          typeof pattern !== "string" ||
          !pattern ||
          pattern === "!"
        ) {
          throw new Error(`trigger ${index + 1} has an invalid filter condition`);
        }
      }
    }
    return {
      on: trigger.on,
      ...(trigger.enabled === false ? { enabled: false } : {}),
      ...(clauses.length === 0
        ? {}
        : { filter: Array.isArray(trigger.filter) ? clauses : clauses[0] }),
    };
  });
}

function createUiHandler(api) {
  return async (req, res) => {
    const url = new URL(req.url ?? "/", "http://gateway.invalid");
    const path = url.pathname.replace(/\/$/, "");
    const isWriteApi = path === UI_WRITE_PATH;
    const fromPluginFrame = req.headers.origin === "null";
    if (req.method === "GET" && path === UI_ROOT) {
      const config = api.runtime.config.current();
      const triggers = config.plugins?.entries?.forgeclaw?.config?.trigger ?? [];
      const initialTriggers = JSON.stringify(triggers)
        .replaceAll("&", "\\u0026")
        .replaceAll("<", "\\u003c")
        .replaceAll(">", "\\u003e");
      res.writeHead(200, {
        "Content-Type": "text/html; charset=utf-8",
        "Cache-Control": "no-store",
        "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'self'",
      });
      res.end(UI_PAGE.replace("__FORGECLAW_TRIGGERS__", initialTriggers));
      return;
    }
    if (req.method === "POST" && isWriteApi) {
      try {
        const body = JSON.parse(await readBody(req));
        if (body?.csrf !== UI_CSRF) {
          json(res, 403, { error: "invalid ForgeClaw page token" }, fromPluginFrame);
          return;
        }
        const triggers = validateTriggers(body?.triggers);
        await api.runtime.config.mutateConfigFile({
          afterWrite: { mode: "auto" },
          mutate: (draft) => {
            const entry = draft.plugins?.entries?.forgeclaw;
            if (!entry?.config) {
              throw new Error("ForgeClaw plugin configuration is missing");
            }
            entry.config.trigger = triggers;
          },
        });
        json(res, 200, { triggers }, fromPluginFrame);
      } catch (error) {
        json(
          res,
          400,
          { error: error instanceof Error ? error.message : String(error) },
          fromPluginFrame,
        );
      }
      return;
    }
    json(res, 404, { error: "not found" });
  };
}

export default definePluginEntry({
  id: "forgeclaw",
  name: "Forgeclaw",
  description: "Forge automation configuration and teammate skill.",
  register(api) {
    const config = api.pluginConfig;
    // Capture the bridge credential before agent turns can spawn commands,
    // then remove it from the environment inherited by those commands.
    const authorization = takeAuthorization(config.authorization_env);
    api.registerHttpRoute({
      path: `${UI_ROOT}/`,
      auth: "gateway",
      match: "prefix",
      handler: createUiHandler(api),
    });
    api.registerHttpRoute({
      path: UI_WRITE_PATH,
      auth: "plugin",
      match: "exact",
      handler: createUiHandler(api),
    });
    for (const definition of tools) {
      api.registerTool((context) => createTool(definition, context, config, authorization), {
        name: definition.name,
      });
    }
  },
});

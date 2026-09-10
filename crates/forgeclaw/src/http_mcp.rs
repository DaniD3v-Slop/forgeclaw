use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use forgeclaw_core::{Forge, NewPr, RepoId, Review, ThreadKey, Verdict};
use forgeclaw_forgejo::Forgejo;
use serde_json::{Value, json};
use tokio::process::Command;
use url::Url;

use crate::mcp::McpAuthorizer;

/// State for the stateless Streamable HTTP MCP endpoint.
///
/// The read client token is never used for mutations. Each write client is
/// constructed from the scoped token supplied by [`McpAuthorizer`].
pub struct McpServer {
    forge_url: Url,
    read_token: String,
    authorization: Option<String>,
    authorizer: Arc<McpAuthorizer>,
    workspace: PathBuf,
}

impl McpServer {
    pub fn new(
        forge_url: Url,
        read_token: impl Into<String>,
        authorization: Option<String>,
        authorizer: Arc<McpAuthorizer>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            forge_url,
            read_token: read_token.into(),
            authorization,
            authorizer,
            workspace,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/mcp", post(call))
            .with_state(Arc::new(self))
    }

    fn read_forge(&self) -> Result<Forgejo, String> {
        Forgejo::new(self.forge_url.clone(), &self.read_token, None)
            .map_err(|error| error.to_string())
    }

    fn write_forge(&self, token: &str) -> Result<Forgejo, String> {
        Forgejo::new(self.forge_url.clone(), token, None).map_err(|error| error.to_string())
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.authorization else {
            return true;
        };
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(expected)
    }

    fn checkout_path(&self, thread: &ThreadKey, writable: bool) -> PathBuf {
        let subject = thread.subject.to_string().replace('/', "-");
        let directory = if writable {
            subject
        } else {
            format!("{subject}-readonly")
        };
        self.workspace
            .join(&thread.repo.owner)
            .join(&thread.repo.name)
            .join(directory)
    }

    fn clone_url(&self, repo: &RepoId) -> Result<Url, String> {
        self.forge_url
            .join(&format!("{repo}.git"))
            .map_err(|error| error.to_string())
    }
}

async fn call(
    State(server): State<Arc<McpServer>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    if !server.authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "forgeclaw", "version": env!("CARGO_PKG_VERSION")}
        })),
        Some("tools/list") => Ok(json!({"tools": tools()})),
        Some("tools/call") => {
            tool_call(&server, request.get("params").unwrap_or(&Value::Null)).await
        }
        Some(method) => Err(format!("unsupported MCP method: {method}")),
        None => Err("MCP request has no method".into()),
    };
    match result {
        Ok(result) => Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response(),
        Err(message) => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32602, "message": message}
        }))
        .into_response(),
    }
}

async fn tool_call(server: &McpServer, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call has no name")?;
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let text = match name {
        "forge_read" => {
            let thread = subject(arguments)?;
            let context = server
                .read_forge()?
                .context(&thread)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string(&context).map_err(|error| error.to_string())?
        }
        "forge_search_issues" => {
            let repo = repo(arguments)?;
            let query = string(arguments, "query")?;
            let issues = server
                .read_forge()?
                .search_issues(&repo, query)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string(&issues).map_err(|error| error.to_string())?
        }
        "forge_comment" => {
            let thread = subject(arguments)?;
            let body = string(arguments, "body")?;
            let reply_to = arguments.get("reply_to").and_then(Value::as_u64);
            let token = server
                .authorizer
                .write_token(params, &thread)
                .map_err(str::to_owned)?;
            let id = server
                .write_forge(&token.secret)?
                .comment(&thread, body, reply_to)
                .await
                .map_err(|error| error.to_string())?;
            format!("posted comment #{id}")
        }
        "forge_create_pr" => {
            let thread = subject(arguments)?;
            let title = string(arguments, "title")?;
            let body = string(arguments, "body")?;
            let branch = string(arguments, "branch")?;
            let token = server
                .authorizer
                .write_token(params, &thread)
                .map_err(str::to_owned)?;
            let id = server
                .write_forge(&token.secret)?
                .create_pr(
                    &thread.repo,
                    NewPr {
                        title: title.into(),
                        body: body.into(),
                        branch: branch.into(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            format!("opened PR #{id}")
        }
        "forge_submit_review" => {
            let thread = subject(arguments)?;
            let verdict = string(arguments, "verdict").and_then(|value| {
                serde_json::from_value::<Verdict>(Value::String(value.into()))
                    .map_err(|error| error.to_string())
            })?;
            let summary = string(arguments, "summary")?;
            let forgeclaw_core::Subject::Pr(pr) = thread.subject else {
                return Err("reviews require a pull request subject".into());
            };
            let token = server
                .authorizer
                .write_token(params, &thread)
                .map_err(str::to_owned)?;
            server
                .write_forge(&token.secret)?
                .submit_review(
                    &thread.repo,
                    pr,
                    Review {
                        verdict,
                        summary: summary.into(),
                        inline: Vec::new(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            "submitted review".into()
        }
        "forge_checkout" => {
            let thread = subject(arguments)?;
            let token = server.authorizer.authorized_token(params, &thread);
            let path = server.checkout_path(&thread, token.is_some());
            if !path.join(".git").is_dir() {
                let repo = match &token {
                    Some(token) => server
                        .write_forge(&token.secret)?
                        .ensure_fork(&thread.repo)
                        .await
                        .map_err(|error| error.to_string())?,
                    None => thread.repo.clone(),
                };
                clone(
                    &server.clone_url(&repo)?,
                    token.as_ref().map(|token| token.secret.as_str()),
                    &path,
                )
                .await?;
            }
            path.display().to_string()
        }
        "forge_push" => {
            let thread = subject(arguments)?;
            let branch = string(arguments, "branch")?;
            valid_branch(branch)?;
            let token = server
                .authorizer
                .write_token(params, &thread)
                .map_err(str::to_owned)?;
            let path = server.checkout_path(&thread, true);
            if !path.join(".git").is_dir() {
                return Err("checkout the authorized subject before pushing".into());
            }
            git(
                &path,
                &token.secret,
                ["push", "origin", &format!("HEAD:refs/heads/{branch}")],
            )
            .await?;
            format!("pushed branch {branch}")
        }
        other => return Err(format!("unknown forge tool: {other}")),
    };
    Ok(json!({"content": [{"type": "text", "text": text}]}))
}

async fn clone(url: &Url, token: Option<&str>, path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("checkout path has no parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| error.to_string())?;
    let mut command = Command::new("git");
    command.args([
        "clone",
        "--quiet",
        url.as_str(),
        path.to_str().ok_or("invalid checkout path")?,
    ]);
    run_git(command, token).await
}

async fn git<const N: usize>(path: &Path, token: &str, args: [&str; N]) -> Result<(), String> {
    let mut command = Command::new("git");
    command.current_dir(path).args(args);
    run_git(command, Some(token)).await
}

async fn run_git(mut command: Command, token: Option<&str>) -> Result<(), String> {
    // The auth header is process-local and is never stored in the checkout's
    // remote URL or config, so the agent cannot recover the scoped token.
    if let Some(token) = token {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: token {token}"),
            );
    }
    let status = command.status().await.map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "git operation failed".into())
}

fn valid_branch(branch: &str) -> Result<(), String> {
    (!branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c)))
    .then_some(())
    .ok_or_else(|| "invalid branch name".into())
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field: {field}"))
}

fn repo(arguments: &Value) -> Result<RepoId, String> {
    string(arguments, "repo")?
        .parse::<RepoId>()
        .map_err(|error| error.to_string())
}

fn subject(arguments: &Value) -> Result<ThreadKey, String> {
    string(arguments, "subject")?
        .parse::<ThreadKey>()
        .map_err(|error| error.to_string())
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "forge_read",
            "Read a forge issue or pull request.",
            json!({"subject": {"type": "string"}}),
            vec!["subject"],
        ),
        tool(
            "forge_search_issues",
            "Search issues and pull requests in a repository.",
            json!({"repo": {"type": "string"}, "query": {"type": "string"}}),
            vec!["repo", "query"],
        ),
        tool(
            "forge_comment",
            "Comment on the authorized issue or pull request.",
            json!({"subject": {"type": "string"}, "body": {"type": "string"}, "reply_to": {"type": "integer"}}),
            vec!["subject", "body"],
        ),
        tool(
            "forge_create_pr",
            "Open a pull request from an existing branch for the authorized subject.",
            json!({"subject": {"type": "string"}, "title": {"type": "string"}, "body": {"type": "string"}, "branch": {"type": "string"}}),
            vec!["subject", "title", "body", "branch"],
        ),
        tool(
            "forge_submit_review",
            "Submit an approve, request_changes, or comment review on the authorized pull request.",
            json!({"subject": {"type": "string"}, "verdict": {"type": "string", "enum": ["approve", "request_changes", "comment"]}, "summary": {"type": "string"}}),
            vec!["subject", "verdict", "summary"],
        ),
        tool(
            "forge_checkout",
            "Clone a repository into the shared agent workspace. An authorized turn receives its bot fork; other sessions receive a source checkout.",
            json!({"subject": {"type": "string"}}),
            vec!["subject"],
        ),
        tool(
            "forge_push",
            "Push the current shared checkout as a branch for the authorized subject.",
            json!({"subject": {"type": "string"}, "branch": {"type": "string"}}),
            vec!["subject", "branch"],
        ),
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required}
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn tools_have_no_undeclared_write_path() {
        let names = tools()
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "forge_read".to_owned(),
                "forge_search_issues".to_owned(),
                "forge_comment".to_owned(),
                "forge_create_pr".to_owned(),
                "forge_submit_review".to_owned(),
                "forge_checkout".to_owned(),
                "forge_push".to_owned()
            ]
        );
    }

    #[test]
    fn parser_rejects_bad_subjects() {
        assert!(subject(&json!({"subject": "not-a-thread"})).is_err());
    }

    #[test]
    fn branches_cannot_become_git_options_or_paths() {
        assert!(valid_branch("forgeclaw/fix-1").is_ok());
        assert!(valid_branch("--upload-pack=nope").is_err());
        assert!(valid_branch("../../etc").is_err());
    }

    #[tokio::test]
    async fn checkout_and_server_side_push_keep_credentials_out_of_git_config() {
        let root = tempdir().unwrap();
        let origin = root.path().join("origin.git");
        let checkout = root.path().join("checkout");
        assert!(
            StdCommand::new("git")
                .args(["init", "--bare", origin.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let url = Url::from_file_path(&origin).unwrap();
        clone(&url, None, &checkout).await.unwrap();
        assert!(
            StdCommand::new("git")
                .current_dir(&checkout)
                .args(["config", "user.email", "test@example.invalid"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            StdCommand::new("git")
                .current_dir(&checkout)
                .args(["config", "user.name", "Forgeclaw test"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(checkout.join("README.md"), "test\n").unwrap();
        assert!(
            StdCommand::new("git")
                .current_dir(&checkout)
                .args(["add", "README.md"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            StdCommand::new("git")
                .current_dir(&checkout)
                .args(["commit", "-m", "test"])
                .status()
                .unwrap()
                .success()
        );
        git(
            &checkout,
            "scoped-token-must-not-persist",
            ["push", "origin", "HEAD:feature"],
        )
        .await
        .unwrap();
        let config = std::fs::read_to_string(checkout.join(".git/config")).unwrap();
        assert!(!config.contains("scoped-token-must-not-persist"));
    }
}

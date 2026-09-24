use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use forgeclaw_core::{Forge, NewPr, RepoId, Review, ThreadKey, Verdict};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::process::Command;
use url::Url;

use crate::grants::{GrantStore, SessionKey};

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_token_label(action: &str) -> String {
    format!(
        "chat-{action}-{}-{}",
        std::process::id(),
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// State for the authenticated HTTP bridge used by the OpenClaw plugin tools.
///
/// The forge credential stays inside this daemon. Session grants authorize
/// writes, while credentialed Git operations run only in daemon-owned paths.
pub struct ToolServer {
    forge_url: Url,
    forge: Arc<dyn Forge>,
    read_token: String,
    authorization: Option<String>,
    grants: Arc<GrantStore>,
    workspace: PathBuf,
}

impl ToolServer {
    pub fn new(
        forge_url: Url,
        forge: Arc<dyn Forge>,
        read_token: String,
        authorization: Option<String>,
        grants: Arc<GrantStore>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            forge_url,
            forge,
            read_token,
            authorization,
            grants,
            workspace,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/tools/call", post(call))
            .with_state(Arc::new(self))
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
    State(server): State<Arc<ToolServer>>,
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
    let session = headers
        .get("x-forgeclaw-session-key")
        .and_then(|value| value.to_str().ok());
    let name = request.get("name").and_then(Value::as_str);
    let arguments = request.get("arguments").unwrap_or(&Value::Null);
    let result = match name {
        Some(name) => tool_call(&server, name, arguments, session).await,
        None => Err("tool request has no name".into()),
    };
    match result {
        Ok(result) => Json(result).into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response(),
    }
}

async fn tool_call(
    server: &ToolServer,
    name: &str,
    arguments: &Value,
    session: Option<&str>,
) -> Result<Value, String> {
    let text = match name {
        "forge_read" => {
            let thread = subject(arguments)?;
            let context = server
                .forge
                .context(&thread)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string(&context).map_err(|error| error.to_string())?
        }
        "forge_search_issues" => {
            let repo = repo(arguments)?;
            let query = string(arguments, "query")?;
            let issues = server
                .forge
                .search_issues(&repo, query)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string(&issues).map_err(|error| error.to_string())?
        }
        "forge_comment" => {
            let thread = subject(arguments)?;
            let body = string(arguments, "body")?;
            let reply_to = arguments.get("reply_to").and_then(Value::as_u64);
            let token = require_write(server, session, &thread)?;
            let id = server
                .forge
                .with_token(&token.secret)
                .map_err(|error| error.to_string())?
                .comment(&thread, body, reply_to)
                .await
                .map_err(|error| error.to_string())?;
            format!("posted comment #{id}")
        }
        "forge_create_pr" => {
            let repo = repo(arguments)?;
            let title = string(arguments, "title")?;
            let body = string(arguments, "body")?;
            let branch = string(arguments, "branch")?;
            valid_branch(branch)?;
            let token = server
                .forge
                .mint_token(&temporary_token_label("create-pr"))
                .await
                .map_err(|error| error.to_string())?;
            let result = async {
                server
                    .forge
                    .with_token(&token.secret)
                    .map_err(|error| error.to_string())?
                    .create_pr(
                        &repo,
                        NewPr {
                            title: title.into(),
                            body: body.into(),
                            branch: branch.into(),
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            server
                .forge
                .revoke_token(&token)
                .await
                .map_err(|error| error.to_string())?;
            let id = result?;
            format!("opened PR #{id}")
        }
        "forge_create_issue" => {
            let repo = repo(arguments)?;
            let title = string(arguments, "title")?;
            let body = string(arguments, "body")?;
            let token = server
                .forge
                .mint_token(&temporary_token_label("create-issue"))
                .await
                .map_err(|error| error.to_string())?;
            let result = async {
                server
                    .forge
                    .with_token(&token.secret)
                    .map_err(|error| error.to_string())?
                    .create_issue(&repo, title, body)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            server
                .forge
                .revoke_token(&token)
                .await
                .map_err(|error| error.to_string())?;
            format!("opened issue #{}", result?)
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
            let token = require_write(server, session, &thread)?;
            server
                .forge
                .with_token(&token.secret)
                .map_err(|error| error.to_string())?
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
            let token = authorized_token(server, session, &thread);
            let path = server.checkout_path(&thread, true);
            let temporary = if token.is_none() {
                Some(
                    server
                        .forge
                        .mint_token(&temporary_token_label("checkout"))
                        .await
                        .map_err(|error| error.to_string())?,
                )
            } else {
                None
            };
            let credential = token
                .as_ref()
                .or(temporary.as_ref())
                .expect("checkout has token");
            let result = async {
                server
                    .forge
                    .with_token(&credential.secret)
                    .map_err(|error| error.to_string())?
                    .ensure_fork(&thread.repo)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            if let Some(temporary) = temporary {
                server
                    .forge
                    .revoke_token(&temporary)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            let repo = result?;
            sync_checkout(
                &server.clone_url(&repo)?,
                Some(server.read_token.as_str()),
                &path,
            )
            .await?
        }
        "forge_push" => {
            let thread = subject(arguments)?;
            let branch = string(arguments, "branch")?;
            valid_branch(branch)?;
            let path = server.checkout_path(&thread, true);
            if !path.join(".git").is_dir() {
                return Err("checkout the subject before pushing".into());
            }
            let temporary = server
                .forge
                .mint_token(&temporary_token_label("push"))
                .await
                .map_err(|error| error.to_string())?;
            let result = async {
                let fork = server
                    .forge
                    .with_token(&temporary.secret)
                    .map_err(|error| error.to_string())?
                    .ensure_fork(&thread.repo)
                    .await
                    .map_err(|error| error.to_string())?;
                let url = server.clone_url(&fork)?;
                let exists = remote_branch_exists(&url, &temporary.secret, branch).await?;
                if exists {
                    require_write(server, session, &thread)?;
                    require_bot_owned_pr(server, &thread, branch).await?;
                }
                push_from_clean_repo(&path, &url, &temporary.secret, branch, !exists).await
            }
            .await;
            server
                .forge
                .revoke_token(&temporary)
                .await
                .map_err(|error| error.to_string())?;
            result?;
            format!("pushed branch {branch}")
        }
        other => return Err(format!("unknown forge tool: {other}")),
    };
    Ok(json!({"content": [{"type": "text", "text": text}]}))
}

async fn require_bot_owned_pr(
    server: &ToolServer,
    thread: &ThreadKey,
    branch: &str,
) -> Result<(), String> {
    if !matches!(thread.subject, forgeclaw_core::Subject::Pr(_)) {
        return Err("existing branches require an authorized pull request subject".into());
    }
    let context = server
        .forge
        .context(thread)
        .await
        .map_err(|error| error.to_string())?;
    let bot = server
        .forge
        .whoami()
        .await
        .map_err(|error| error.to_string())?;
    require_head_owner(&context, &bot, branch)
}

fn require_head_owner(context: &Value, bot: &str, branch: &str) -> Result<(), String> {
    let owner = context
        .get("head_owner")
        .and_then(Value::as_str)
        .filter(|owner| !owner.is_empty())
        .ok_or("pull request context has no head owner")?;
    if owner != bot {
        return Err(format!(
            "cannot push to this pull request: its head branch is owned by {owner}, not {bot}"
        ));
    }
    if context.get("head_branch").and_then(Value::as_str) != Some(branch) {
        return Err(
            "cannot push to this pull request: branch does not match its head branch".into(),
        );
    }
    Ok(())
}

fn authorized_token(
    server: &ToolServer,
    session: Option<&str>,
    thread: &ThreadKey,
) -> Option<forgeclaw_core::ScopedToken> {
    server
        .grants
        .authorized_token(&SessionKey::new(session.unwrap_or_default()), thread)
}

fn require_write(
    server: &ToolServer,
    session: Option<&str>,
    thread: &ThreadKey,
) -> Result<forgeclaw_core::ScopedToken, String> {
    authorized_token(server, session, thread).ok_or_else(|| {
        if session.is_some() {
            "write is not authorized for this subject".into()
        } else {
            "missing OpenClaw session identity".into()
        }
    })
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

async fn sync_checkout(url: &Url, token: Option<&str>, path: &Path) -> Result<String, String> {
    if !path.join(".git").is_dir() {
        clone(url, token, path).await?;
        return Ok(path.display().to_string());
    }

    let mirror = tempdir().map_err(|error| error.to_string())?;
    let mirror_path = mirror.path().join("remote.git");
    let mut network_clone = Command::new("git");
    network_clone.args([
        "clone",
        "--mirror",
        "--quiet",
        url.as_str(),
        mirror_path.to_str().ok_or("invalid temporary path")?,
    ]);
    run_git(network_clone, token).await?;

    let mut fetch = Command::new("git");
    fetch.current_dir(path).args([
        "fetch",
        "--quiet",
        mirror_path.to_str().ok_or("invalid temporary path")?,
        "+refs/heads/*:refs/remotes/origin/*",
    ]);
    run_git(fetch, None).await?;

    let clean = git_output(path, ["status", "--porcelain"])
        .await?
        .is_empty();
    let branch = git_output(path, ["branch", "--show-current"]).await?;
    if clean && !branch.is_empty() {
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let mut exists = Command::new("git");
        exists
            .current_dir(path)
            .args(["show-ref", "--verify", "--quiet", &remote_ref]);
        scrub_git_environment(&mut exists);
        if exists
            .status()
            .await
            .map_err(|error| error.to_string())?
            .success()
        {
            let mut merge = Command::new("git");
            merge
                .current_dir(path)
                .args(["merge", "--ff-only", "--quiet", &remote_ref]);
            run_git(merge, None).await?;
        }
    }
    let warning = (!clean).then_some(
        "\nThe checkout has existing changes; remote refs were refreshed without modifying them.",
    );
    Ok(format!("{}{}", path.display(), warning.unwrap_or_default()))
}

async fn push_from_clean_repo(
    checkout: &Path,
    url: &Url,
    token: &str,
    branch: &str,
    create_only: bool,
) -> Result<(), String> {
    let staging = tempdir().map_err(|error| error.to_string())?;
    let bare = staging.path().join("push.git");
    let mut init = Command::new("git");
    init.args([
        "init",
        "--bare",
        "--quiet",
        bare.to_str().ok_or("invalid temporary path")?,
    ]);
    run_git(init, None).await?;

    let mut fetch = Command::new("git");
    fetch.args([
        "--git-dir",
        bare.to_str().ok_or("invalid temporary path")?,
        "fetch",
        "--quiet",
        checkout.to_str().ok_or("invalid checkout path")?,
        "HEAD",
    ]);
    run_git(fetch, None).await?;

    let mut push = Command::new("git");
    push.args([
        "--git-dir",
        bare.to_str().ok_or("invalid temporary path")?,
        "push",
        "--no-verify",
    ]);
    if create_only {
        push.arg(format!("--force-with-lease=refs/heads/{branch}:"));
    }
    push.args([url.as_str(), &format!("FETCH_HEAD:refs/heads/{branch}")]);
    run_git(push, Some(token)).await
}

async fn remote_branch_exists(url: &Url, token: &str, branch: &str) -> Result<bool, String> {
    let mut command = Command::new("git");
    command.args([
        "ls-remote",
        "--exit-code",
        "--heads",
        url.as_str(),
        &format!("refs/heads/{branch}"),
    ]);
    scrub_git_environment(&mut command);
    command
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: token {token}"),
        );
    let status = command.status().await.map_err(|error| error.to_string())?;
    match status.code() {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        _ => Err("cannot check remote branch".into()),
    }
}

async fn run_git(mut command: Command, token: Option<&str>) -> Result<(), String> {
    // Git may inspect agent-controlled repository configuration. Never let
    // those subprocesses inherit the daemon's permanent forge credentials.
    scrub_git_environment(&mut command);
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

async fn git_output<const N: usize>(path: &Path, args: [&str; N]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.current_dir(path).args(args);
    scrub_git_environment(&mut command);
    let output = command.output().await.map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("git operation failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn scrub_git_environment(command: &mut Command) {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    command
        .env_clear()
        .env("PATH", path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
}

fn valid_branch(branch: &str) -> Result<(), String> {
    (!branch.is_empty()
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.ends_with('.')
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
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

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use tempfile::tempdir;

    use super::*;

    fn git(path: &Path, args: &[&str]) {
        assert!(
            StdCommand::new("git")
                .current_dir(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn parser_rejects_bad_subjects() {
        assert!(subject(&json!({"subject": "not-a-thread"})).is_err());
    }

    #[test]
    fn only_bot_owned_pull_request_heads_are_writable() {
        assert!(
            require_head_owner(
                &json!({"head_owner": "bot", "head_branch": "fix"}),
                "bot",
                "fix"
            )
            .is_ok()
        );
        assert_eq!(
            require_head_owner(
                &json!({"head_owner": "alice", "head_branch": "fix"}),
                "bot",
                "fix"
            )
            .unwrap_err(),
            "cannot push to this pull request: its head branch is owned by alice, not bot"
        );
        assert!(
            require_head_owner(
                &json!({"head_owner": "bot", "head_branch": "fix"}),
                "bot",
                "other"
            )
            .is_err()
        );
        assert!(require_head_owner(&json!({}), "bot", "fix").is_err());
    }

    #[test]
    fn branches_cannot_become_git_options_or_paths() {
        assert!(valid_branch("forgeclaw/fix-1").is_ok());
        assert!(valid_branch("--upload-pack=nope").is_err());
        assert!(valid_branch("../../etc").is_err());
    }

    #[tokio::test]
    async fn git_environment_drops_daemon_secrets() {
        let mut command = Command::new("env");
        command.env("FORGECLAW_FORGE_TOKEN", "permanent-secret");
        scrub_git_environment(&mut command);
        let output = command.output().await.unwrap();
        let environment = String::from_utf8(output.stdout).unwrap();
        assert!(!environment.contains("FORGECLAW_FORGE_TOKEN"));
        assert!(!environment.contains("permanent-secret"));
    }

    #[tokio::test]
    async fn existing_clean_checkout_fast_forwards() {
        let root = tempdir().unwrap();
        let origin = root.path().join("origin.git");
        let seed = root.path().join("seed");
        let checkout = root.path().join("checkout");
        git(root.path(), &["init", "--bare", origin.to_str().unwrap()]);
        git(root.path(), &["init", "-b", "main", seed.to_str().unwrap()]);
        git(&seed, &["config", "user.email", "test@example.invalid"]);
        git(&seed, &["config", "user.name", "Forgeclaw test"]);
        std::fs::write(seed.join("README.md"), "one\n").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-m", "one"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-u", "origin", "main"]);
        git(
            root.path(),
            &[
                "--git-dir",
                origin.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        );

        let url = Url::from_file_path(&origin).unwrap();
        sync_checkout(&url, Some("dummy-token"), &checkout)
            .await
            .unwrap();
        std::fs::write(seed.join("README.md"), "two\n").unwrap();
        git(&seed, &["commit", "-am", "two"]);
        git(&seed, &["push"]);

        sync_checkout(&url, Some("dummy-token"), &checkout)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("README.md")).unwrap(),
            "two\n"
        );
    }

    #[tokio::test]
    async fn push_ignores_agent_controlled_hooks_and_remotes() {
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
        clone(&url, Some("dummy-token"), &checkout).await.unwrap();
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
        let hook = checkout.join(".git/hooks/pre-push");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf '%s' \"$GIT_CONFIG_VALUE_0\" > \"$GIT_DIR/credential-leak\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
            std::fs::set_permissions(&hook, permissions).unwrap();
        }
        assert!(
            StdCommand::new("git")
                .current_dir(&checkout)
                .args([
                    "remote",
                    "set-url",
                    "origin",
                    "https://attacker.invalid/repo"
                ])
                .status()
                .unwrap()
                .success()
        );
        push_from_clean_repo(&checkout, &url, "dummy-token", "feature", true)
            .await
            .unwrap();
        assert!(
            remote_branch_exists(&url, "dummy-token", "feature")
                .await
                .unwrap()
        );
        assert!(
            !remote_branch_exists(&url, "dummy-token", "other")
                .await
                .unwrap()
        );
        std::fs::write(checkout.join("README.md"), "changed\n").unwrap();
        git(&checkout, &["commit", "-am", "second"]);
        assert!(
            push_from_clean_repo(&checkout, &url, "dummy-token", "feature", true)
                .await
                .is_err()
        );
        let config = std::fs::read_to_string(checkout.join(".git/config")).unwrap();
        assert!(!config.contains("dummy-token"));
        assert!(!checkout.join(".git/credential-leak").exists());
        assert!(
            StdCommand::new("git")
                .args([
                    "--git-dir",
                    origin.to_str().unwrap(),
                    "show-ref",
                    "--verify",
                    "refs/heads/feature",
                ])
                .status()
                .unwrap()
                .success()
        );
    }
}

use std::collections::BTreeMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router, response::IntoResponse};
use forgeclaw::grants::GrantStore;
use forgeclaw::http_tools::ToolServer;
use forgeclaw::outbox::Outbox;
use forgeclaw::router::{OpenClawCli, Router as ForgeRouter, TriggerRule, WebhookForge};
use forgeclaw_core::{Forge, ForgeEvent, Result, ScopedToken, ThreadKey};
use forgeclaw_forgejo::{Forgejo, webhook_events};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use url::Url;

#[derive(Deserialize)]
struct OpenClawConfig {
    plugins: PluginConfig,
}

impl OpenClawConfig {
    fn forgeclaw(self) -> Result<Option<DaemonConfig>> {
        let config = self
            .plugins
            .entries
            .into_iter()
            .find_map(|(id, entry)| (id == "forgeclaw").then_some(entry.config).flatten());
        config
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| forgeclaw_core::Error::Config(error.to_string()))
            })
            .transpose()
    }
}

#[derive(Deserialize)]
struct PluginConfig {
    entries: BTreeMap<String, PluginEntry>,
}

#[derive(Deserialize)]
struct PluginEntry {
    config: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonConfig {
    listen: SocketAddr,
    #[serde(rename = "daemon_url")]
    _daemon_url: Url,
    forge: ForgeConfig,
    workspace: PathBuf,
    #[serde(default = "default_grant_ttl")]
    grant_ttl_secs: u64,
    #[serde(default)]
    trigger: Vec<TriggerRule>,
    /// Environment variable containing the bearer value accepted by the
    /// plugin-tool bridge.
    /// The secret itself is never written to OpenClaw config.
    authorization_env: String,
}

fn default_grant_ttl() -> u64 {
    900
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeConfig {
    url: Url,
    token_env: String,
    webhook_secret_env: String,
    password_env: String,
}

struct ForgejoWebhook {
    forge: Arc<Forgejo>,
}

#[async_trait]
impl WebhookForge for ForgejoWebhook {
    async fn whoami(&self) -> Result<String> {
        self.forge.whoami().await
    }

    async fn mint_token(&self, label: &str) -> Result<ScopedToken> {
        self.forge.mint_token(label).await
    }

    async fn revoke_token(&self, token: &ScopedToken) -> Result<()> {
        self.forge.revoke_token(token).await
    }

    async fn add_reaction(
        &self,
        token: &ScopedToken,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        self.forge
            .with_token(&token.secret)?
            .add_reaction(thread, comment_id, emoji)
            .await
    }

    async fn remove_reaction(
        &self,
        token: &ScopedToken,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()> {
        self.forge
            .with_token(&token.secret)?
            .remove_reaction(thread, comment_id, emoji)
            .await
    }
}

type AppRouter = ForgeRouter<ForgejoWebhook, OpenClawCli>;

#[derive(Clone)]
struct WebhookState {
    router: Arc<AppRouter>,
    config_path: PathBuf,
    secret: Arc<str>,
    outbox: Arc<Outbox>,
    wake: Arc<Notify>,
    authorization: Arc<str>,
}

impl WebhookState {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(self.authorization.as_ref())
    }
}

#[derive(Deserialize)]
struct PreviewRequest {
    triggers: Vec<TriggerRule>,
    event: ForgeEvent,
}

async fn preview(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Json(request): Json<PreviewRequest>,
) -> impl IntoResponse {
    if !state.authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    match state
        .router
        .preview(&request.triggers, &request.event)
        .await
    {
        Ok(matches) => (StatusCode::OK, Json(json!({"matches": matches}))).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn outbox_status(State(state): State<WebhookState>, headers: HeaderMap) -> impl IntoResponse {
    if !state.authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    match state.outbox.status() {
        Ok(status) => (StatusCode::OK, Json(json!(status))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn retry_outbox(State(state): State<WebhookState>, headers: HeaderMap) -> impl IntoResponse {
    if !state.authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    match state.outbox.retry_now() {
        Ok(retried) => {
            state.wake.notify_one();
            (StatusCode::OK, Json(json!({"retried": retried}))).into_response()
        }
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(signature) = headers
        .get("x-forgejo-signature")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let events = match webhook_events(&signature, &state.secret, &body) {
        Ok(events) => events,
        Err(error) => {
            eprintln!("webhook rejected: {error}");
            return StatusCode::UNAUTHORIZED;
        }
    };
    if events.is_empty() {
        return StatusCode::NO_CONTENT;
    }
    // The signature identifies the signed payload across Forgejo redeliveries.
    // A 202 means the normalized events have reached durable storage.
    if let Err(error) = state.outbox.enqueue(&signature, &events) {
        eprintln!("webhook could not be queued: {error}");
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    state.wake.notify_one();
    StatusCode::ACCEPTED
}

async fn drain_outbox(state: WebhookState) {
    loop {
        match state.outbox.ready() {
            Ok(Some(pending)) => {
                let result = async {
                    let config: OpenClawConfig =
                        serde_json::from_slice(&std::fs::read(&state.config_path)?)
                            .map_err(|error| forgeclaw_core::Error::Config(error.to_string()))?;
                    let rules = config
                        .forgeclaw()?
                        .ok_or_else(|| {
                            forgeclaw_core::Error::Config(
                                "plugins.entries.forgeclaw.config is required".into(),
                            )
                        })?
                        .trigger;
                    TriggerRule::validate_all(&rules)?;
                    state.router.replace_rules(rules).await;
                    state
                        .router
                        .deliver_with(pending.events, |thread| {
                            state
                                .outbox
                                .complete_subject(&pending.id, &thread.to_string())
                        })
                        .await
                }
                .await;
                let saved = match result {
                    Ok(_) => state.outbox.complete(&pending.id),
                    Err(error) => {
                        eprintln!("webhook routing failed; retrying: {error}");
                        state
                            .outbox
                            .retry(&pending.id, pending.attempts, &error.to_string())
                    }
                };
                if let Err(error) = saved {
                    eprintln!("webhook outbox update failed: {error}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
            Ok(None) => {
                tokio::select! {
                    () = state.wake.notified() => {},
                    () = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
            }
            Err(error) => {
                eprintln!("webhook outbox read failed: {error}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = env::var("OPENCLAW_CONFIG_PATH")
        .unwrap_or_else(|_| "/home/node/.openclaw/openclaw.json".into());
    let config: OpenClawConfig = serde_json::from_slice(&std::fs::read(&config_path)?)
        .map_err(|error| forgeclaw_core::Error::Config(error.to_string()))?;
    let daemon = config.forgeclaw()?.ok_or_else(|| {
        forgeclaw_core::Error::Config("plugins.entries.forgeclaw.config is required".into())
    })?;
    let read_token = required_env(&daemon.forge.token_env)?;
    let secret = required_env(&daemon.forge.webhook_secret_env)?;
    let password = required_env(&daemon.forge.password_env)?;
    let authorization = required_env(&daemon.authorization_env)?;
    TriggerRule::validate_all(&daemon.trigger)?;
    let forge = Arc::new(Forgejo::new(
        daemon.forge.url.clone(),
        &read_token,
        Some(password),
    )?);
    let revoked = forge.revoke_stale_tokens().await?;
    if revoked > 0 {
        eprintln!("revoked {revoked} stale ForgeClaw access tokens");
    }
    let grants = Arc::new(GrantStore::default());
    let router = Arc::new(ForgeRouter::new(
        ForgejoWebhook {
            forge: forge.clone(),
        },
        OpenClawCli::new(
            "openclaw",
            vec![
                daemon.forge.token_env.clone(),
                daemon.forge.password_env.clone(),
                daemon.forge.webhook_secret_env.clone(),
                daemon.authorization_env.clone(),
            ],
        ),
        "forgejo",
        daemon.trigger,
        grants.clone(),
        Duration::from_secs(daemon.grant_ttl_secs),
    ));
    let outbox_path = env::var("FORGECLAW_OUTBOX_PATH")
        .unwrap_or_else(|_| "/home/node/.openclaw/forgeclaw-outbox.sqlite".into());
    let outbox = Arc::new(Outbox::open(outbox_path)?);
    let wake = Arc::new(Notify::new());
    let webhook_state = WebhookState {
        router,
        config_path: config_path.into(),
        secret: secret.into(),
        outbox,
        wake,
        authorization: authorization.clone().into(),
    };
    tokio::spawn(drain_outbox(webhook_state.clone()));
    let tools = ToolServer::new(
        daemon.forge.url,
        forge,
        read_token,
        Some(authorization),
        grants,
        daemon.workspace,
    )
    .router();
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/webhook", post(webhook))
        .route("/triggers/preview", post(preview))
        .route("/outbox/status", get(outbox_status))
        .route("/outbox/retry", post(retry_outbox))
        .with_state(webhook_state)
        .merge(tools);
    let listener = tokio::net::TcpListener::bind(daemon.listen).await?;
    axum::serve(listener, app)
        .await
        .map_err(forgeclaw_core::Error::Io)
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| forgeclaw_core::Error::Config(format!("{name} is not set")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use forgeclaw_core::Subject;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn test_state(server: &MockServer, outbox_path: PathBuf) -> WebhookState {
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "forgeclaw"})))
            .mount(server)
            .await;
        let forge = Arc::new(Forgejo::new(server.uri().parse().unwrap(), "test", None).unwrap());
        let router = Arc::new(ForgeRouter::new(
            ForgejoWebhook { forge },
            OpenClawCli::new("unused", vec![]),
            "forgejo",
            vec![],
            Arc::new(GrantStore::default()),
            Duration::from_secs(30),
        ));
        WebhookState {
            router,
            config_path: PathBuf::new(),
            secret: "secret".into(),
            outbox: Arc::new(Outbox::open(outbox_path).unwrap()),
            wake: Arc::new(Notify::new()),
            authorization: "test-authorization".into(),
        }
    }

    fn authorized_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "test-authorization".parse().unwrap());
        headers
    }

    async fn body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn outbox_controls_require_auth_and_retry_failed_work() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let state = test_state(&server, dir.path().join("outbox.sqlite")).await;
        let event = ForgeEvent {
            repo: "owner/repo".parse().unwrap(),
            kind: "comment.created".into(),
            subject: Subject::Issue(1),
            payload: json!({"body": "hello"}),
        };
        state.outbox.enqueue("delivery", &[event]).unwrap();
        state
            .outbox
            .retry("delivery", 0, "gateway unavailable")
            .unwrap();

        let response = outbox_status(State(state.clone()), HeaderMap::new())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = outbox_status(State(state.clone()), authorized_headers())
            .await
            .into_response();
        let status = body(response).await;
        assert_eq!(status["pending"], 1);
        assert_eq!(status["last_error"], "gateway unavailable");
        assert!(status["oldest_pending_ms"].as_i64().is_some());
        let response = retry_outbox(State(state.clone()), authorized_headers())
            .await
            .into_response();
        assert_eq!(body(response).await["retried"], 1);
        assert!(state.outbox.ready().unwrap().is_some());
    }

    #[tokio::test]
    async fn preview_uses_draft_rules_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let state = test_state(&server, dir.path().join("outbox.sqlite")).await;
        let request: PreviewRequest = serde_json::from_value(json!({
            "triggers": [
                {"on": "comment.created", "filter": {"mentions": "@me"}},
                {"on": "comment.created", "filter": {"author": "@me"}}
            ],
            "event": {
                "repo": {"owner": "owner", "name": "repo"},
                "kind": "comment.created",
                "subject": {"Issue": 1},
                "payload": {"mentions": ["forgeclaw"], "author": "alice"}
            }
        }))
        .unwrap();
        let response = preview(State(state.clone()), HeaderMap::new(), Json(request))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let request = serde_json::from_value(json!({
            "triggers": [{"on": "comment.created", "filter": {"mentions": "@me"}}],
            "event": {"repo": {"owner": "owner", "name": "repo"}, "kind": "comment.created", "subject": {"Issue": 1}, "payload": {"mentions": ["forgeclaw"]}}
        })).unwrap();
        let response = preview(State(state.clone()), authorized_headers(), Json(request))
            .await
            .into_response();
        assert_eq!(body(response).await["matches"], json!([0]));
        assert!(state.outbox.ready().unwrap().is_none());
    }

    #[tokio::test]
    async fn signed_webhook_is_durable_before_acceptance_and_redelivery_is_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let state = test_state(&server, dir.path().join("outbox.sqlite")).await;
        let body = Bytes::from_static(br#"{"action":"created","repository":{"full_name":"owner/repo"},"issue":{"number":1},"comment":{"id":7,"body":"@forgeclaw help","user":{"login":"alice"}}}"#);
        assert_eq!(
            webhook(State(state.clone()), HeaderMap::new(), body.clone())
                .await
                .into_response()
                .status(),
            StatusCode::UNAUTHORIZED,
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forgejo-signature",
            "99ad648a51c4536bc80e46f4730afe1eb1ea4f490035cf363f8386f0593fec85"
                .parse()
                .unwrap(),
        );
        for _ in 0..2 {
            assert_eq!(
                webhook(State(state.clone()), headers.clone(), body.clone())
                    .await
                    .into_response()
                    .status(),
                StatusCode::ACCEPTED,
            );
        }
        let pending = state.outbox.ready().unwrap().unwrap();
        assert_eq!(pending.events.len(), 1);
        assert_eq!(pending.events[0].thread().to_string(), "owner/repo#issue/1");
        assert_eq!(state.outbox.status().unwrap().pending, 1);
    }

    #[tokio::test]
    async fn outbox_worker_recovers_after_invalid_configuration_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let mut state = test_state(&server, dir.path().join("outbox.sqlite")).await;
        state.config_path = dir.path().join("openclaw.json");
        std::fs::write(&state.config_path, "invalid JSON").unwrap();
        state
            .outbox
            .enqueue(
                "delivery",
                &[ForgeEvent {
                    repo: "owner/repo".parse().unwrap(),
                    kind: "comment.created".into(),
                    subject: Subject::Issue(1),
                    payload: json!({"body": "hello"}),
                }],
            )
            .unwrap();
        let worker = tokio::spawn(drain_outbox(state.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.outbox.status().unwrap().last_error.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.outbox.status().unwrap().pending, 1);

        std::fs::write(&state.config_path, json!({"plugins": {"entries": {"forgeclaw": {"config": {
            "listen": "127.0.0.1:3080",
            "daemon_url": "http://127.0.0.1:3080",
            "workspace": "/tmp",
            "authorization_env": "TEST_AUTH",
            "forge": {"url": "http://127.0.0.1:3000", "token_env": "TOKEN", "webhook_secret_env": "SECRET", "password_env": "PASSWORD"},
            "trigger": []
        }}}}}).to_string()).unwrap();
        assert_eq!(state.outbox.retry_now().unwrap(), 1);
        state.wake.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.outbox.status().unwrap().pending == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(state.outbox.status().unwrap().last_error.is_none());
        worker.abort();
    }

    #[test]
    fn unrelated_plugins_do_not_need_forgeclaw_config() {
        let config: OpenClawConfig = serde_json::from_value(serde_json::json!({
            "plugins": {"entries": {
                "another-plugin": {"enabled": true},
                "device-pair": {"config": {"publicUrl": "wss://example.invalid"}},
                "forgeclaw": {"config": {
                    "listen": "127.0.0.1:3080",
                    "daemon_url": "http://forgeclaw:3080",
                    "workspace": "/tmp/workspace",
                    "authorization_env": "AUTHORIZATION",
                    "forge": {
                        "url": "http://forgejo:3000/",
                        "token_env": "TOKEN",
                        "webhook_secret_env": "SECRET",
                        "password_env": "LEGACY_PASSWORD"
                    }
                }}
            }}
        }))
        .unwrap();

        assert!(config.plugins.entries["another-plugin"].config.is_none());
        assert!(config.plugins.entries["device-pair"].config.is_some());
        assert!(config.plugins.entries["forgeclaw"].config.is_some());
        assert!(config.forgeclaw().unwrap().is_some());
    }
}

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
use axum::{Router, response::IntoResponse};
use forgeclaw::grants::GrantStore;
use forgeclaw::http_tools::ToolServer;
use forgeclaw::router::{OpenClawCli, Router as ForgeRouter, TriggerRule, WebhookForge};
use forgeclaw_core::{Forge, Result, ScopedToken};
use forgeclaw_forgejo::{Forgejo, webhook_events};
use serde::Deserialize;
use serde_json::Value;
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
}

type AppRouter = ForgeRouter<ForgejoWebhook, OpenClawCli>;

#[derive(Clone)]
struct WebhookState {
    router: Arc<AppRouter>,
    config_path: PathBuf,
    secret: Arc<str>,
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
    tokio::spawn(async move {
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
            state.router.deliver(events).await
        }
        .await;
        if let Err(error) = result {
            eprintln!("webhook routing failed: {error}");
        }
    });
    StatusCode::ACCEPTED
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
    let authorization = Some(required_env(&daemon.authorization_env)?);
    TriggerRule::validate_all(&daemon.trigger)?;
    let forge = Arc::new(Forgejo::new(
        daemon.forge.url.clone(),
        &read_token,
        Some(password),
    )?);
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
    let tools = ToolServer::new(
        daemon.forge.url,
        forge,
        authorization,
        grants,
        daemon.workspace,
    )
    .router();
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/webhook", post(webhook))
        .with_state(WebhookState {
            router,
            config_path: config_path.into(),
            secret: secret.into(),
        })
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

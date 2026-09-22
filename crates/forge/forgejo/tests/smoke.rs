//! Nightly smoke test against a real Forgejo instance — catches OpenAPI
//! drift that wiremock fixtures can't. Ignored by default; the nightly
//! workflow runs it with `-- --ignored` after bootstrapping the instance.
//!
//! Env: `FORGE_URL`, `FORGE_TOKEN`, `FORGE_REPO` (owner/name, must exist), and
//! `FORGE_PASSWORD` for the token-lifecycle test.

use forgeclaw_core::{Forge, RepoId};
use forgeclaw_forgejo::Forgejo;

#[tokio::test]
#[ignore = "needs a real Forgejo: FORGE_URL, FORGE_TOKEN, FORGE_REPO"]
async fn read_against_real_forgejo() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    let forge = Forgejo::new(var("FORGE_URL").parse().unwrap(), &var("FORGE_TOKEN"), None).unwrap();
    let repo: RepoId = var("FORGE_REPO").parse().unwrap();

    let me = forge.whoami().await.unwrap();
    assert!(!me.is_empty(), "whoami returned an empty login");

    forge.search_issues(&repo, "").await.unwrap();
}

#[tokio::test]
#[ignore = "needs a real Forgejo: FORGE_URL, FORGE_TOKEN, FORGE_PASSWORD"]
async fn scoped_token_lifecycle_against_real_forgejo() {
    let var = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} not set"));
    let forge = Forgejo::new(
        var("FORGE_URL").parse().unwrap(),
        &var("FORGE_TOKEN"),
        Some(var("FORGE_PASSWORD")),
    )
    .unwrap();

    let token = forge
        .mint_token("forgeclaw-smoke-disposable")
        .await
        .unwrap();
    forge.revoke_token(&token).await.unwrap();
}

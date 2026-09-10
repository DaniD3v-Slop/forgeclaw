//! Nightly smoke test against a real Forgejo instance — catches OpenAPI
//! drift that wiremock fixtures can't. Ignored by default; the nightly
//! workflow runs it with `-- --ignored` after bootstrapping the instance.
//!
//! Env: `FORGE_URL`, `FORGE_TOKEN`, `FORGE_REPO` (owner/name, must exist).

use forgeclaw_core::RepoId;
use forgeclaw_forgejo::Forgejo;

#[tokio::test]
#[ignore = "needs a real Forgejo: FORGE_URL, FORGE_TOKEN, FORGE_REPO"]
async fn read_against_real_forgejo() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    let forge = Forgejo::new(
        var("FORGE_URL").parse().unwrap(),
        &var("FORGE_TOKEN"),
        std::env::var("FORGE_PASSWORD").ok(),
    )
    .unwrap();
    let repo: RepoId = var("FORGE_REPO").parse().unwrap();

    let me = forge.whoami().await.unwrap();
    assert!(!me.is_empty(), "whoami returned an empty login");

    forge.search_issues(&repo, "").await.unwrap();
}

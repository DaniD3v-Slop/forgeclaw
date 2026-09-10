//! Nightly smoke test against a real Forgejo instance — catches OpenAPI
//! drift that wiremock fixtures can't. Ignored by default; the nightly
//! workflow runs it with `-- --ignored` after bootstrapping the instance.
//!
//! Env: `FORGE_URL`, `FORGE_TOKEN`, `FORGE_REPO` (owner/name, must exist).

use std::time::{SystemTime, UNIX_EPOCH};

use forgeclaw_core::{Forge, RepoId, Subject, ThreadKey};
use forgeclaw_forgejo::Forgejo;

#[tokio::test]
#[ignore = "needs a real Forgejo: FORGE_URL, FORGE_TOKEN, FORGE_REPO"]
async fn issue_lifecycle_against_real_forgejo() {
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

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let title = format!("smoke {}", stamp.as_secs());
    let number = forge
        .create_issue(&repo, &title, "opened by the nightly smoke test")
        .await
        .unwrap();

    let thread = ThreadKey {
        repo: repo.clone(),
        subject: Subject::Issue(number),
    };
    forge.comment(&thread, "smoke comment", None).await.unwrap();

    let hits = forge.search_issues(&repo, &title).await.unwrap();
    assert!(
        hits.iter().any(|i| i.number == number),
        "created issue not found via search: {hits:?}"
    );

    forge
        .close_issue(&repo, number, Some("smoke test done"))
        .await
        .unwrap();
    // The issue list defaults to open issues, so a closed issue vanishes.
    let open = forge.search_issues(&repo, &title).await.unwrap();
    assert!(
        !open.iter().any(|i| i.number == number),
        "issue {number} still open after close"
    );
}

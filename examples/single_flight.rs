//! `Dedup<K, V>`: a standalone single-flight primitive for any key and value.
//!
//! Run with: `cargo run --example single_flight`
//!
//! Nothing here is about LLMs: a burst of requests for the same user profile
//! hits the "database" once, and an access-token refresh triggered by many
//! tasks at once happens once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_prompt_orchestrator::Dedup;

#[derive(Clone, Debug)]
struct Profile {
    id: u64,
    name: String,
}

async fn load_profile(db_hits: &AtomicUsize, id: u64) -> Result<Profile, String> {
    db_hits.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(Profile {
        id,
        name: format!("user-{id}"),
    })
}

#[tokio::main]
async fn main() {
    // 1. Coalesce a cache-miss stampede.
    let profiles: Dedup<u64, Result<Profile, String>> = Dedup::new();
    let db_hits = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for i in 0..100u64 {
        let profiles = profiles.clone();
        let db_hits = db_hits.clone();
        let id = i % 3; // 100 requests over 3 distinct users
        tasks.push(tokio::spawn(async move {
            profiles
                .run(id, || async move { load_profile(&db_hits, id).await })
                .await
        }));
    }
    for t in tasks {
        if let Ok(Ok(p)) = t.await {
            debug_assert_eq!(p.name, format!("user-{}", p.id));
        }
    }
    let stats = profiles.stats();
    println!(
        "100 lookups over 3 users -> {} database hit(s) ({} coalesced)",
        db_hits.load(Ordering::SeqCst),
        stats.coalesced
    );

    // 2. One token refresh, however many tasks notice the token expired.
    let refresh: Dedup<&'static str, String> = Dedup::new();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let results = futures::future::join_all((0..10).map(|_| {
        let refreshes = refreshes.clone();
        refresh.run("oauth-token", move || async move {
            refreshes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            "token-abc123".to_string()
        })
    }))
    .await;
    println!(
        "10 tasks needed a fresh token -> {} refresh call(s), all got {:?}",
        refreshes.load(Ordering::SeqCst),
        results[0]
    );
}

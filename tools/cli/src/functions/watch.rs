//! `functions dev --watch`: file events (`notify`) coalesced until 150 ms pass without one, then
//! only the functions whose inputs changed are rebuilt and deployed.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// How long the files must be quiet before a redeploy.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// The next batch of changed paths: waits for one, then takes more until `quiet` passes without
/// another. `None` once the sender is gone.
pub async fn next_batch(
    rx: &mut UnboundedReceiver<PathBuf>,
    quiet: Duration,
) -> Option<HashSet<PathBuf>> {
    let mut batch = HashSet::from([rx.recv().await?]);
    while let Ok(Some(p)) = tokio::time::timeout(quiet, rx.recv()).await {
        batch.insert(p);
    }
    Some(batch)
}

/// Whether a batch touches a function built from `inputs`.
pub fn affects(batch: &HashSet<PathBuf>, inputs: &BTreeSet<PathBuf>) -> bool {
    inputs.iter().any(|i| batch.contains(i))
}

/// A recursive watcher on `root` sending each changed path (canonical, so it matches the
/// inputs, which are made canonical too); keep it alive while watching.
pub fn watch(
    root: &std::path::Path,
) -> notify::Result<(notify::RecommendedWatcher, UnboundedReceiver<PathBuf>)> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let build = root.join(".chelabase");
    let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        let Ok(ev) = ev else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        for p in ev.paths {
            if !p.starts_with(&build) {
                let _ = tx.send(p.canonicalize().unwrap_or(p));
            }
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn events_within_the_window_coalesce_into_one_batch() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = tokio::spawn(async move {
            for (i, wait) in [0u64, 50, 50, 50].into_iter().enumerate() {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                tx.send(PathBuf::from(format!("/p/{}", i % 3))).unwrap();
            }
            // Past the window: a second batch.
            tokio::time::sleep(Duration::from_millis(400)).await;
            tx.send(PathBuf::from("/p/late")).unwrap();
        });
        let start = Instant::now();
        let first = next_batch(&mut rx, DEBOUNCE).await.unwrap();
        let took = start.elapsed();
        assert_eq!(first.len(), 3, "{first:?}");
        assert!(
            took >= Duration::from_millis(150 + 150),
            "fires only after 150 ms quiet: {took:?}"
        );
        assert!(
            took < Duration::from_millis(400 + 150),
            "before the late event: {took:?}"
        );
        let second = next_batch(&mut rx, DEBOUNCE).await.unwrap();
        assert_eq!(second, HashSet::from([PathBuf::from("/p/late")]));
        sender.await.unwrap();
        assert_eq!(
            next_batch(&mut rx, DEBOUNCE).await,
            None,
            "the sender is gone"
        );
    }

    #[test]
    fn only_functions_whose_inputs_changed_are_affected() {
        let batch = HashSet::from([PathBuf::from("/app/functions/a/index.ts")]);
        let a = BTreeSet::from([
            PathBuf::from("/app/functions/a/index.ts"),
            PathBuf::from("/app/lib.ts"),
        ]);
        let b = BTreeSet::from([PathBuf::from("/app/functions/b/index.ts")]);
        assert!(affects(&batch, &a));
        assert!(!affects(&batch, &b));
    }
}

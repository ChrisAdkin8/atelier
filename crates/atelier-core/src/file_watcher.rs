//! §14 concurrent-edit detection — per-session filesystem watcher.
//!
//! Spec §14 "Concurrent edits":
//!
//! > File-watcher (fsevents / inotify) detects external edits to files in
//! > the agent's read set. Harness queues the next tool-call dispatch
//! > (does not cancel the current one) and surfaces a modal …
//!
//! This module owns the filesystem-watcher side of that contract. The
//! dispatcher feeds it the agent's read-set (paths touched by
//! `read_file`, `list_dir`, `grep`, `ast_grep`); the watcher debounces
//! raw `notify` events, intersects them against the read-set, and
//! emits [`crate::session::Event::FilesChanged`] on the session bus.
//!
//! The modal flow itself (Reload / Wait / Pause options + 5-minute
//! auto-pause) lives in the runner (`atelier-cli/src/runner.rs`):
//! the watcher is the *signal*, not the *policy*. Keeping the two
//! separate means tests can drive the modal logic by hand-emitting
//! `FilesChanged` without booting `notify`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event as NotifyEvent, EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;
use tokio::sync::{broadcast, mpsc};

use crate::session::Event;
use crate::time::now_rfc3339;

/// PROVISIONAL — debounce window before an external-edit burst surfaces
/// as one `FilesChanged` event. A modern IDE save (vim's swap-then-rename,
/// VS Code's atomic-write, JetBrains' periodic auto-save) typically
/// emits a handful of `notify` events within tens of milliseconds; the
/// agent only cares about the aggregate "the files changed" signal.
pub const FILE_WATCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// Caller-facing handle on a per-session [`FileWatcher`]. Cloneable; the
/// shared inner state is wrapped in an `Arc<Mutex<_>>` so the dispatcher
/// (which adds paths after each `read_file` / `list_dir` / `grep` /
/// `ast_grep` call) and the watcher's drain task can both touch the
/// read-set without coordination.
///
/// A no-op handle ([`FileWatcherHandle::disabled`]) lets callers opt out
/// of watcher integration without sprinkling `if let Some(w)` at every
/// dispatch site. The disabled handle's `track` is a cheap return.
#[derive(Clone)]
pub struct FileWatcherHandle {
    inner: Option<Arc<FileWatcherInner>>,
}

struct FileWatcherInner {
    /// The paths the agent has read this session. Membership test on
    /// every raw `notify` event — only edits inside this set surface
    /// as `FilesChanged`.
    ///
    /// Stored as canonical (post-`canonicalize`) `PathBuf`s so a path
    /// added under one alias (e.g. `./src/main.rs`) compares equal to
    /// the watcher's reported path under another (`/abs/src/main.rs`).
    read_set: Mutex<HashSet<PathBuf>>,
    /// Channel feeding raw `notify` events from the OS thread into the
    /// async drain task. Bounded so a burst doesn't grow without
    /// limit; debouncing is the consumer's job.
    raw_tx: mpsc::Sender<NotifyEvent>,
    /// Held to keep the OS watcher alive for the handle's lifetime.
    /// Dropping the handle (and its inner `Arc`) tears the watcher down.
    _watcher: Mutex<notify::RecommendedWatcher>,
}

impl FileWatcherHandle {
    /// No-op handle. `track` is a cheap return; nothing is watched and
    /// no events ever fire. Used by:
    ///   * tests that don't care about the watcher,
    ///   * the `--non-interactive` mode's `ConcurrentEditPolicy::AutoReload`
    ///     path when the runner has chosen *not* to wire a watcher
    ///     (the policy still applies if a watcher *is* attached).
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// `true` if this handle is the no-op variant.
    pub fn is_disabled(&self) -> bool {
        self.inner.is_none()
    }

    /// Add `path` to the read-set. Idempotent; calling repeatedly with
    /// the same path is a hash lookup.
    ///
    /// Path canonicalisation runs here so the cross-comparison with
    /// `notify` events is apples-to-apples. A non-existent path
    /// canonicalises to itself (the call is no-op-ish for the watcher
    /// since `notify::watch` would refuse a non-existent path anyway).
    pub fn track(&self, path: &Path) {
        let Some(inner) = &self.inner else {
            return;
        };
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut watcher = inner._watcher.lock();
        // `notify` rejects double-watching with WatchAlreadyExists on
        // some backends; ignore that. Other failures we trace —
        // missing-file is not a test failure (e.g., `grep` over a
        // pattern that didn't hit a real file). Watch the directory
        // rather than the file when the path doesn't exist yet, so a
        // later "the file appeared" event still surfaces.
        let watch_target = if canonical.exists() {
            canonical.clone()
        } else {
            canonical
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| canonical.clone())
        };
        // `RecursiveMode::NonRecursive` is enough: read_file targets a
        // single file; list_dir targets a directory the user already
        // chose. Following symlinks is the OS watcher's call.
        let _ = watcher.watch(&watch_target, RecursiveMode::NonRecursive);
        drop(watcher);
        // Membership is on the canonical (or as-passed) path the
        // dispatcher will see again next call. We store the canonical
        // form so cross-aliases match.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        inner.read_set.lock().insert(canonical);
    }

    /// Test-only: snapshot the current read-set. Returns paths in sorted
    /// order so tests get a deterministic comparison.
    #[doc(hidden)]
    pub fn read_set_snapshot(&self) -> Vec<PathBuf> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        let mut v: Vec<_> = inner.read_set.lock().iter().cloned().collect();
        v.sort();
        v
    }

    /// Internal: feed a synthetic raw event into the debouncer. Test-only
    /// (no `#[cfg(test)]` because integration tests in another crate
    /// drive it too). Returns false when the watcher has shut down.
    #[doc(hidden)]
    pub async fn inject_raw_event(&self, ev: NotifyEvent) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        inner.raw_tx.send(ev).await.is_ok()
    }
}

/// Errors a [`FileWatcher`] can return at construction.
#[derive(Debug, thiserror::Error)]
pub enum FileWatcherError {
    #[error("notify watcher init failed: {0}")]
    Notify(#[from] notify::Error),
}

/// Spawn a per-session file watcher. Returns a [`FileWatcherHandle`]
/// the dispatcher uses to add paths via [`FileWatcherHandle::track`].
///
/// `bus` is the session's broadcast sender — the watcher posts
/// `Event::FilesChanged` on it after each debounced burst. The drain
/// task exits when `bus` has no live `Sender` clones (i.e. session
/// teardown).
pub fn spawn(
    bus: broadcast::Sender<Event>,
    debounce: Duration,
) -> Result<FileWatcherHandle, FileWatcherError> {
    let (raw_tx, mut raw_rx) = mpsc::channel::<NotifyEvent>(256);
    let raw_tx_clone = raw_tx.clone();

    // The `notify` watcher runs its callback on a dedicated OS thread;
    // the callback hands events to our async drain task via the mpsc
    // channel above. `try_send` (not `send`) avoids blocking the OS
    // thread when the drain is slow; dropping a raw event is acceptable
    // because the drain coalesces bursts anyway.
    let watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        if let Ok(ev) = res {
            if matches!(
                ev.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                let _ = raw_tx_clone.try_send(ev);
            }
        }
    })?;

    let read_set = Mutex::new(HashSet::new());
    let inner = Arc::new(FileWatcherInner {
        read_set,
        raw_tx,
        _watcher: Mutex::new(watcher),
    });

    // Spawn the drain task. Holds a weak clone of `inner` so it
    // doesn't keep the watcher alive after the handle is dropped.
    let inner_for_task = Arc::downgrade(&inner);
    tokio::spawn(async move {
        // Burst accumulator. We collect raw events until either the
        // channel idles for `debounce` or the buffer hits a generous
        // soft cap — then flush.
        let mut pending: Vec<PathBuf> = Vec::new();
        loop {
            // Wait for the first event (or shutdown).
            let first = tokio::select! {
                ev = raw_rx.recv() => ev,
            };
            let Some(first) = first else {
                break;
            };
            pending.extend(first.paths);

            // Accumulate further events within the debounce window.
            let deadline = tokio::time::Instant::now() + debounce;
            loop {
                let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
                let next = tokio::time::timeout(timeout, raw_rx.recv()).await;
                match next {
                    Ok(Some(ev)) => pending.extend(ev.paths),
                    Ok(None) => return, // channel closed; drain exits
                    Err(_) => break,    // debounce expired
                }
            }

            // Filter through the read-set.
            let Some(inner) = inner_for_task.upgrade() else {
                return;
            };
            let read_set = inner.read_set.lock();
            let mut hits: Vec<PathBuf> = pending
                .drain(..)
                .filter(|p| {
                    // Compare on canonical form when possible.
                    let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                    read_set.contains(&canon) || read_set.contains(p)
                })
                .collect();
            drop(read_set);
            // Deduplicate while preserving first-seen order.
            let mut seen = HashSet::with_capacity(hits.len());
            hits.retain(|p| seen.insert(p.clone()));

            if hits.is_empty() {
                continue;
            }
            // best-effort send; no subscribers is OK (the on-disk
            // session is the source of truth per §14).
            let _ = bus.send(Event::FilesChanged {
                paths: hits,
                observed_at: now_rfc3339(),
            });
        }
    });

    Ok(FileWatcherHandle { inner: Some(inner) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind};

    #[test]
    fn disabled_handle_is_no_op() {
        let h = FileWatcherHandle::disabled();
        assert!(h.is_disabled());
        h.track(Path::new("/nonexistent"));
        assert!(h.read_set_snapshot().is_empty());
    }

    #[tokio::test]
    async fn track_adds_path_to_read_set() {
        let (tx, _rx) = broadcast::channel(8);
        let td = tempfile::TempDir::new().unwrap();
        let f = td.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let h = spawn(tx, FILE_WATCH_DEBOUNCE).expect("spawn");
        h.track(&f);
        let snap = h.read_set_snapshot();
        assert_eq!(snap.len(), 1);
    }

    #[tokio::test]
    async fn external_edit_to_tracked_file_emits_files_changed() {
        let (tx, mut rx) = broadcast::channel::<Event>(8);
        let td = tempfile::TempDir::new().unwrap();
        let f = td.path().join("watched.txt");
        std::fs::write(&f, b"start").unwrap();
        let h = spawn(tx, Duration::from_millis(50)).expect("spawn");
        h.track(&f);

        // Inject a synthetic raw event to avoid relying on the OS
        // backend's timing in CI.
        let canon = f.canonicalize().unwrap_or(f.clone());
        h.inject_raw_event(NotifyEvent {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![canon],
            attrs: notify::event::EventAttributes::new(),
        })
        .await;

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        let ev = received.expect("timed out").expect("recv");
        match ev {
            Event::FilesChanged { paths, .. } => {
                assert!(
                    paths
                        .iter()
                        .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("watched.txt")),
                    "FilesChanged should carry the tracked file, got {paths:?}"
                );
            }
            other => panic!("expected FilesChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_edit_outside_read_set_is_silent() {
        let (tx, mut rx) = broadcast::channel::<Event>(8);
        let td = tempfile::TempDir::new().unwrap();
        let f = td.path().join("untracked.txt");
        std::fs::write(&f, b"start").unwrap();
        let h = spawn(tx, Duration::from_millis(50)).expect("spawn");
        // No `h.track(&f)` — untracked.

        h.inject_raw_event(NotifyEvent {
            kind: EventKind::Create(CreateKind::Any),
            paths: vec![f.clone()],
            attrs: notify::event::EventAttributes::new(),
        })
        .await;

        let received = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        assert!(
            received.is_err(),
            "untracked path should not emit FilesChanged: got {received:?}"
        );
    }

    #[tokio::test]
    async fn bursts_within_debounce_window_coalesce_to_one_event() {
        let (tx, mut rx) = broadcast::channel::<Event>(8);
        let td = tempfile::TempDir::new().unwrap();
        let f = td.path().join("burst.txt");
        std::fs::write(&f, b"x").unwrap();
        let h = spawn(tx, Duration::from_millis(80)).expect("spawn");
        h.track(&f);

        let canon = f.canonicalize().unwrap_or(f.clone());
        // Fire 4 raw events back-to-back — well inside the 80ms window.
        for _ in 0..4 {
            h.inject_raw_event(NotifyEvent {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![canon.clone()],
                attrs: notify::event::EventAttributes::new(),
            })
            .await;
        }

        // First flush within the debounce + a margin.
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first recv timeout")
            .expect("first recv");
        assert!(matches!(first, Event::FilesChanged { .. }));

        // No further events for at least 2× debounce.
        let extra = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            extra.is_err(),
            "second FilesChanged should not fire — got {extra:?}"
        );
    }
}

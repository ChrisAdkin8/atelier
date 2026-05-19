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

use crate::session::{try_emit, Event};
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
    /// v60.29 H12: canonicalisation runs once, outside any `parking_lot`
    /// mutex. The two locks below (`_watcher` and `read_set`) only ever
    /// hold the lock long enough to call into `notify` and insert into
    /// the set — no syscalls, no allocations beyond the set insert. A
    /// path that fails to canonicalise (non-existent or permission
    /// denied) falls back to the as-passed `PathBuf`; `notify::watch`
    /// would refuse a non-existent target anyway.
    pub fn track(&self, path: &Path) {
        let Some(inner) = &self.inner else {
            return;
        };
        // Single canonicalize call (was duplicated pre-v60.29 at the
        // top + bottom of this function). Result is shared between the
        // `notify::watch` target derivation and the `read_set` insert.
        let canonical = canonicalize_for_track(path);
        let exists = canonical.exists();
        let watch_target = if exists {
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
        {
            let mut watcher = inner._watcher.lock();
            let _ = watcher.watch(&watch_target, RecursiveMode::NonRecursive);
        }
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

/// v60.29 H12: single canonicalize point used by both `track()` and the
/// notify-worker filter. Production behaviour is `path.canonicalize()`
/// with a fallback to the raw `PathBuf`; tests can inject a per-call
/// delay (see `contention_tests`) to assert the call happens outside
/// the watcher's `parking_lot::Mutex`es.
fn canonicalize_for_track(path: &Path) -> PathBuf {
    #[cfg(test)]
    contention_tests::maybe_inject_slow_canonicalize();
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
            // v60.29 H12: canonicalize *before* taking the membership
            // lock. The pre-v60.29 path canonicalised inside the
            // critical section; under 32-way contention against a slow
            // fs that pushed P99 wait time to ~100ms × N.
            let candidates: Vec<(PathBuf, PathBuf)> = pending
                .drain(..)
                .map(|p| {
                    let canon = canonicalize_for_track(&p);
                    (canon, p)
                })
                .collect();
            let read_set = inner.read_set.lock();
            let mut hits: Vec<PathBuf> = candidates
                .into_iter()
                .filter(|(canon, raw)| read_set.contains(canon) || read_set.contains(raw))
                .map(|(canon, _)| canon)
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
            let _ = try_emit(
                &bus,
                Event::FilesChanged {
                    paths: hits,
                    observed_at: now_rfc3339(),
                },
            );
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

#[cfg(test)]
mod contention_tests {
    //! v60.29 H12 — assert canonicalize runs outside the
    //! `parking_lot::Mutex` critical sections in [`super::track`].
    //!
    //! The test arms a thread-local that makes
    //! [`super::canonicalize_for_track`] sleep 100ms per call. It then
    //! spawns 32 parallel `track()` invocations. If canonicalize were
    //! still inside the lock, every call would serialise on the mutex
    //! and total wall-clock would be ≈ 32 × 100ms. With H12's hoisting
    //! every thread can canonicalize in parallel and only the (~µs)
    //! lock-insert sections serialise; the per-call P99 stays well
    //! under 5ms past the unavoidable 100ms canonicalize itself.

    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;
    use std::time::Instant;

    thread_local! {
        static SLOW_CANONICALIZE: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn maybe_inject_slow_canonicalize() {
        if SLOW_CANONICALIZE.with(|c| c.get()) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn arm_slow_canonicalize() {
        SLOW_CANONICALIZE.with(|c| c.set(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn track_canonicalize_runs_outside_lock() {
        let (tx, _rx) = broadcast::channel::<Event>(8);
        let td = tempfile::TempDir::new().unwrap();
        let f = td.path().join("hot.txt");
        std::fs::write(&f, b"x").unwrap();
        let h = spawn(tx, FILE_WATCH_DEBOUNCE).expect("spawn");
        let h = Arc::new(h);

        // Arm the slow-canonicalize hook from every worker thread by
        // spawning a setter as the first thing each task does.
        let mut handles = Vec::new();
        let mut waits_ms: Vec<u128> = Vec::new();
        let (latency_tx, mut latency_rx) = tokio::sync::mpsc::unbounded_channel::<u128>();
        for _ in 0..32 {
            let hh = h.clone();
            let fp = f.clone();
            let lt = latency_tx.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                arm_slow_canonicalize();
                let started = Instant::now();
                hh.track(&fp);
                let elapsed = started.elapsed().as_millis();
                let _ = lt.send(elapsed);
            }));
        }
        drop(latency_tx);

        let overall_start = Instant::now();
        for h in handles {
            h.await.unwrap();
        }
        let overall_elapsed = overall_start.elapsed();

        while let Some(ms) = latency_rx.recv().await {
            waits_ms.push(ms);
        }
        waits_ms.sort_unstable();

        // Sanity: the hook actually fired — every track() should have
        // taken at least 100ms (its own canonicalize) but well under
        // 32 × 100ms (which would mean serialised through the lock).
        let p99 = waits_ms[(waits_ms.len() * 99 / 100).min(waits_ms.len() - 1)];
        assert!(
            p99 >= 100,
            "slow-canonicalize hook did not fire (p99 = {p99}ms)"
        );
        assert!(
            overall_elapsed < Duration::from_millis(1500),
            "32 parallel tracks took {overall_elapsed:?}; would be ≥ 3.2s if serialised on the lock"
        );
        // Per-call: under 8 worker threads the worst case is
        // ceil(32/8) × 100ms = 400ms even with perfect parallelism,
        // far below the pre-H12 serialised behaviour. We just assert
        // we're nowhere near the serialised bound.
        assert!(
            p99 < 800,
            "p99 per-track latency {p99}ms suggests canonicalize is still inside the lock"
        );
    }
}

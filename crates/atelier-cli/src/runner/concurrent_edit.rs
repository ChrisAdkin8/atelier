//! §14 concurrent-edit resolver phase.
//!
//! Kept out of `runner.rs` so the run loop can stay focused on phase
//! sequencing while the headless/modal resolver policy is tested in isolation.

use atelier_core::dispatcher::ConcurrentEditPolicy;
use atelier_core::session::{try_emit, ConcurrentEditOutcome, Event};

/// Subscribe to the session bus and resolve `FilesChanged` events according to
/// the runner's concurrent-edit policy.
pub(super) fn spawn_concurrent_edit_resolver(
    bus: tokio::sync::broadcast::Sender<Event>,
    policy: ConcurrentEditPolicy,
    pause_timeout: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::FilesChanged { .. }) => match policy {
                    ConcurrentEditPolicy::AutoReload => {
                        let _ = try_emit(
                            &bus,
                            Event::FilesChangedAcknowledged {
                                outcome: ConcurrentEditOutcome::AutoReload,
                            },
                        );
                    }
                    ConcurrentEditPolicy::Modal => {
                        let mut local_rx = bus.subscribe();
                        let timer = tokio::time::sleep(pause_timeout);
                        tokio::pin!(timer);
                        loop {
                            tokio::select! {
                                _ = &mut timer => {
                                    let _ = try_emit(
                                        &bus,
                                        Event::FilesChangedAcknowledged {
                                            outcome: ConcurrentEditOutcome::PauseTimedOut,
                                        },
                                    );
                                    break;
                                }
                                ev = local_rx.recv() => match ev {
                                    Ok(Event::FilesChangedAcknowledged { .. }) => break,
                                    Ok(Event::Shutdown) | Err(_) => return,
                                    _ => continue,
                                },
                            }
                        }
                    }
                },
                Ok(Event::Shutdown) | Err(_) => return,
                _ => continue,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_reload_policy_acknowledges_files_changed() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(16);
        let _task = spawn_concurrent_edit_resolver(
            bus.clone(),
            ConcurrentEditPolicy::AutoReload,
            std::time::Duration::from_secs(60),
        );

        try_emit(
            &bus,
            Event::FilesChanged {
                paths: vec!["src/lib.rs".into()],
                observed_at: "2026-05-21T00:00:00Z".into(),
            },
        )
        .unwrap();

        loop {
            match rx.recv().await.unwrap() {
                Event::FilesChangedAcknowledged { outcome } => {
                    assert_eq!(outcome, ConcurrentEditOutcome::AutoReload);
                    break;
                }
                Event::FilesChanged { .. } => continue,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn modal_policy_times_out_to_pause() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(16);
        let _task = spawn_concurrent_edit_resolver(
            bus.clone(),
            ConcurrentEditPolicy::Modal,
            std::time::Duration::from_millis(10),
        );

        try_emit(
            &bus,
            Event::FilesChanged {
                paths: vec!["src/lib.rs".into()],
                observed_at: "2026-05-21T00:00:00Z".into(),
            },
        )
        .unwrap();

        loop {
            match rx.recv().await.unwrap() {
                Event::FilesChangedAcknowledged { outcome } => {
                    assert_eq!(outcome, ConcurrentEditOutcome::PauseTimedOut);
                    break;
                }
                Event::FilesChanged { .. } => continue,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }
}

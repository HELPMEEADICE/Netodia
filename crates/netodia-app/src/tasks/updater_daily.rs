//! Daily auto-check loop.
//!
//! Cadence:
//!
//! - **30 s after launch**: one-shot first check (gives the runtime time
//!   to settle and avoids racing the launch folder scan for
//!   network I/O).
//! - **Then every 6 h**: re-arm via `tokio::time::sleep`. Not
//!   `tokio::time::interval` — `interval` fires every tick instantly
//!   after a laptop wake from sleep, which would burst-check.
//! - **The auto-check switch is read per tick**, not at the spawn — the
//!   welcome card and Settings ▸ Updates both offer it mid-session, and a
//!   boot-time decision leaves either describing a task the user can no
//!   longer start or stop. Toggling it also cuts the sleep short
//!   (`AppState::auto_check_changed`), or the answer would land whenever
//!   the loop next happened to wake.
//! - **24 h elapsed gate** inside each tick reads
//!   `settings.updates.last_check_unix`; if less than a day has passed
//!   the tick logs "skipped" and re-sleeps. Lets the loop survive a
//!   suspend/resume mid-cycle without spurious double-checks.
//! - **Failure backoff**: repeated failures lengthen the cadence along
//!   [`BACKOFF_LADDER`], which argues its own steps, and the next
//!   successful check resets it. Mitigates flaky-network / firewall
//!   thrash that would otherwise re-fire every 6 h.
//!
//! Each iteration that passes those gates is [`run_check`], the same check
//! the Settings button runs, framed by the panel's checking state. A failure
//! is logged and nothing more: a background check stays quiet.

use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::watch;

use crate::services::settings;
use crate::services::updater::{PanelPaint, UpdaterEvent, run_check};
use crate::state::AppState;
use crate::tasks::TaskSpawner;
use netodia_core::error::describe;

const STARTUP_DELAY: Duration = Duration::from_secs(30);
const NORMAL_CADENCE: Duration = Duration::from_hours(6);
const ONE_DAY_SECS: i64 = 24 * 60 * 60;

/// Exponential backoff schedule after consecutive failures. Indexed by
/// `consecutive_failures.saturating_sub(1)` — first failure stays at
/// the normal 6h cadence, second waits 12h, third 24h, fourth+ tops
/// out at the 7d ceiling. Recovers immediately on the next successful
/// check (counter resets, `pick_next_delay` returns `NORMAL_CADENCE`).
///
/// Why exponential instead of a single 7d jump: transient failures
/// (DNS hiccup, captive portal, weekend outage) shouldn't punish the
/// user for a full week — 12h and 24h give the network time to recover
/// without thrashing 6h-after-6h-after-6h.
const BACKOFF_LADDER: &[Duration] = &[
    Duration::from_hours(12),
    Duration::from_hours(24),
    Duration::from_hours(7 * 24), // cap
];

/// Spawn the daily updater loop on the shared `TaskSpawner`. The loop
/// exits cleanly when the shutdown token fires.
///
/// `event_tx` carries notification-worthy events (the toast push that
/// fires for newly-available versions). State writes that don't need a
/// toast — `is-checking` flips, `up-to-date` repaint — go to `paint`.
pub fn spawn(
    spawner: &TaskSpawner,
    state: AppState,
    paint: impl Fn(PanelPaint) + Send + Sync + 'static,
    event_tx: watch::Sender<Option<UpdaterEvent>>,
) {
    let mut auto_check = state.auto_check_changed.subscribe();

    spawner.spawn_cancellable(move |shutdown| async move {
        // Startup grace period — gives the launch scan + DB
        // pre-fetch room to settle before we add network I/O.
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(STARTUP_DELAY) => {}
        }

        loop {
            run_one_iteration(&state, &paint, &event_tx).await;

            let delay = pick_next_delay(&state);
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                // Cuts a sleep the switch has outlived. Nothing here decides anything: the
                // iteration re-reads the file, and `needs_check`'s 24 h floor is what stops a
                // toggled switch turning into a request — a failed check stamps `last_check_unix`
                // on its way out, so even a bounced switch under a live backoff cannot hammer.
                woken = auto_check.changed() => {
                    if woken.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(delay) => {}
            }
        }
    });
}

async fn run_one_iteration(
    state: &AppState,
    paint: &impl Fn(PanelPaint),
    event_tx: &watch::Sender<Option<UpdaterEvent>>,
) {
    let snapshot = match settings::read_settings(&state.paths) {
        Ok(s) => s.updates,
        Err(e) => {
            log::warn!("updater_daily: read_settings failed: {}", describe(&e));
            return;
        }
    };

    // Ahead of the elapsed gate: a disabled check owes no network I/O and no log line
    // per 6 h either.
    if !snapshot.auto_check_enabled {
        return;
    }

    if !needs_check(snapshot.last_check_unix) {
        log::info!(
            "updater_daily: check skipped — last check {}s ago (< 24h)",
            elapsed_secs(snapshot.last_check_unix)
        );
        return;
    }

    log::info!("updater_daily: checking for updates");
    paint(PanelPaint::CheckStarted);
    let checked = run_check(state.http_client(), &state.paths).await;
    paint(PanelPaint::CheckEnded);

    match checked {
        Ok(finding) => finding.deliver(paint, event_tx),
        Err(e) => log::warn!("updater_daily: check failed: {}", describe(&e)),
    }
}

fn pick_next_delay(state: &AppState) -> Duration {
    // A disabled loop records no failures, so the ladder describes nothing while the switch is
    // off. The UI toggle cuts the sleep itself; this is the floor for a `settings.json` edited
    // underneath us, which bumps nothing and would otherwise wait out a stale 7 d step.
    let count = settings::read_settings(&state.paths)
        .ok()
        .filter(|s| s.updates.auto_check_enabled)
        .map_or(0, |s| s.updates.consecutive_failures);
    let delay = backoff_delay_for(count);
    if count >= 2 {
        log::info!(
            "updater_daily: {count} consecutive failures — backing off to {}h cadence",
            delay.as_secs() / 3600
        );
    }
    delay
}

/// Pure helper: maps a consecutive-failure count to the next sleep
/// duration. Extracted from [`pick_next_delay`] so the backoff ladder
/// can be unit-tested without touching settings I/O.
fn backoff_delay_for(count: u8) -> Duration {
    if count <= 1 {
        // 0 = healthy; 1 = single hiccup, stay at normal cadence.
        return NORMAL_CADENCE;
    }
    // 2nd failure → ladder[0] = 12h; 3rd → ladder[1] = 24h;
    // 4th+ → ladder[last] = 7d cap.
    let idx = (count as usize).saturating_sub(2).min(BACKOFF_LADDER.len() - 1);
    BACKOFF_LADDER[idx]
}

fn needs_check(last_check_unix: i64) -> bool {
    if last_check_unix <= 0 {
        return true;
    }
    elapsed_secs(last_check_unix) >= ONE_DAY_SECS
}

fn elapsed_secs(last_check_unix: i64) -> i64 {
    let now = Timestamp::now().as_second();
    now.saturating_sub(last_check_unix)
}

#[cfg(test)]
#[path = "tests/updater_daily_tests.rs"]
mod tests;

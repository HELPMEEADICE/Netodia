//! One update check, start to finish, for both callers: the daily task and the Settings button.
//!
//! Fetching, judging a skip, caching the asset and recording the result sit here so the two can't
//! drift apart. What stays with each caller is its cadence and how loudly it fails.

use jiff::Timestamp;
use tokio::sync::watch;

use netodia_core::config::Paths;
use netodia_core::error::{AppResult, describe};

use super::asset_cache;
use super::check::{Checked, Verdict, check_for_update, recheck_for_update};
use super::event::UpdaterEvent;
use super::github::RELEASES_BASE;
use super::install::installed_version;
use super::manifest::LatestManifest;
use super::version::is_upgrade;
use crate::services::settings::{self, UpdateFlags};

/// A write the Settings → Updates panel owes, painted by whoever holds the window: this crate
/// doesn't name the Slint global it lands in.
#[derive(Debug)]
pub enum PanelPaint {
    CheckStarted,
    CheckEnded,
    UpToDate,
    Available { version: String, notes_short: String, critical: bool },
}

/// What a check found, once recorded.
#[derive(Debug)]
pub enum Finding {
    UpToDate,
    /// A manifest this build can't act on in-app: a schema it can't read, or no asset for this
    /// platform. The panel keeps whatever it showed.
    NotInstallable,
    Available {
        version: String,
        notes_short: String,
        critical: bool,
        notify: bool,
    },
}

impl Finding {
    /// Paints the panel and raises the toast this finding owes.
    pub fn deliver(
        self,
        paint: &impl Fn(PanelPaint),
        event_tx: &watch::Sender<Option<UpdaterEvent>>,
    ) {
        match self {
            Self::UpToDate => paint(PanelPaint::UpToDate),
            Self::NotInstallable => {}
            Self::Available { version, notes_short, critical, notify } => {
                paint(PanelPaint::Available {
                    version: version.clone(),
                    notes_short: notes_short.clone(),
                    critical,
                });
                if notify {
                    let _ = event_tx.send(Some(UpdaterEvent::Available {
                        version,
                        notes_short,
                        critical,
                    }));
                }
            }
        }
    }
}

/// Checks the published manifest against this build and records the answer in `settings.json`.
///
/// # Errors
///
/// The fetch's or the manifest's failure, recorded first as a failed check so the daily backoff
/// counts it. Failing to *record* a check is logged rather than returned: what it found is still
/// true.
pub async fn run_check(http: &reqwest::Client, paths: &Paths) -> AppResult<Finding> {
    let flags = match settings::read_settings(paths) {
        Ok(settings) => settings.updates,
        Err(e) => {
            log::warn!("updater: update state unreadable, checking without it: {}", describe(&e));
            UpdateFlags::default()
        }
    };

    let current = installed_version();
    let fetched = match revalidation_etag(&flags, &current) {
        Some(etag) => recheck_for_update(http, RELEASES_BASE, etag, &current).await,
        None => check_for_update(http, RELEASES_BASE, &current).await.map(Some),
    };

    let now = Timestamp::now().as_second();
    match fetched {
        Ok(Some(checked)) => Ok(settle(paths, &flags.skipped_release, checked, now)),
        Ok(None) => {
            log::info!("updater: 304 Not Modified");
            record(paths, |updates| updates.record_not_modified(now));
            Ok(Finding::UpToDate)
        }
        Err(e) => {
            record(paths, |updates| updates.record_failure(now));
            Err(e)
        }
    }
}

/// The stored tag, wherever a `304` against it can only mean "nothing newer": the manifest it
/// names offered `current_version` no upgrade. Anything else is read in full, since a `304`
/// carries no manifest and this process may no longer hold the one the tag stood for.
fn revalidation_etag<'a>(flags: &'a UpdateFlags, current_version: &str) -> Option<&'a str> {
    if flags.last_manifest_etag.is_empty() {
        return None;
    }
    // An unreadable stored version can't vouch for the tag beside it either.
    match is_upgrade(current_version, &flags.last_known_release) {
        Ok(false) => Some(&flags.last_manifest_etag),
        Ok(true) | Err(_) => None,
    }
}

fn settle(paths: &Paths, skipped_release: &str, checked: Checked, now: i64) -> Finding {
    let Checked { manifest, etag, verdict } = checked;
    let LatestManifest { manifest_schema_version, version, critical, notes_short, .. } = manifest;

    let mut spent_skip = None;
    let finding = match verdict {
        Verdict::UpToDate => {
            log::info!("updater: up to date");
            Finding::UpToDate
        }
        Verdict::NoAssetForTarget => {
            log::info!("updater: manifest has no asset for current target");
            Finding::NotInstallable
        }
        Verdict::UnsupportedSchema => {
            log::info!("updater: unsupported manifest schema {manifest_schema_version}");
            Finding::NotInstallable
        }
        Verdict::Available(asset) => {
            log::info!(
                "updater: update available: {version}{}",
                if critical { " (critical)" } else { "" }
            );
            // Kept so an Install click can proceed even if its own re-fetch fails.
            asset_cache::store(version.clone(), asset);
            let skip = skip_verdict(skipped_release, &version, critical);
            if skip.clear_skip {
                spent_skip = Some(skipped_release.to_owned());
            }
            Finding::Available {
                version: version.clone(),
                notes_short,
                critical,
                notify: skip.notify,
            }
        }
    };

    record(paths, |updates| {
        updates.record_fetch(now, version, etag);
        if let Some(spent) = &spent_skip {
            updates.forget_skip(spent);
        }
    });
    finding
}

/// Writes what a check found as one change to the file. A failed write is logged rather than
/// returned: the next check simply runs sooner, `last_check_unix` not having moved.
fn record(paths: &Paths, apply: impl FnOnce(&mut UpdateFlags)) {
    if let Err(e) = settings::mutate_settings(paths, |settings| apply(&mut settings.updates)) {
        log::warn!("updater: recording the check failed: {}", describe(&e));
    }
}

/// What the stored "skip this version" means once a manifest names a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkipVerdict {
    /// Raise the "update available" event.
    notify: bool,
    /// Drop the stored skip — it names a version the manifest has moved past, or one semver
    /// cannot read at all.
    clear_skip: bool,
}

/// The one rule here with a security consequence: a release the publisher flagged critical must
/// surface even where the user has muted it.
fn skip_verdict(skipped_release: &str, version: &str, critical: bool) -> SkipVerdict {
    let unmuted = SkipVerdict { notify: true, clear_skip: false };
    if skipped_release.is_empty() {
        return unmuted;
    }

    match is_upgrade(skipped_release, version) {
        // Strictly newer than what was skipped, so the skip is spent.
        Ok(true) => SkipVerdict { notify: true, clear_skip: true },
        Ok(false) => SkipVerdict { notify: critical, clear_skip: false },
        Err(e) => {
            log::warn!(
                "updater: stored skipped_release {skipped_release:?} not valid semver \
                 ({}); clearing rather than muting every future notification",
                describe(&e)
            );
            SkipVerdict { notify: true, clear_skip: true }
        }
    }
}

#[cfg(test)]
#[path = "tests/run_tests.rs"]
mod tests;

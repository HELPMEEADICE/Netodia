---
paths:
  - crates/netodia-app/src/services/updater/**/*.rs
  - crates/netodia-platform/src/services/platform/install_kind/**/*.rs
  - crates/netodia-platform/src/services/platform/desktop_integration.rs
  - crates/netodia-app/src/tasks/updater_daily.rs
  - crates/netodia-views/src/ui/settings/updater_settings.rs
  - crates/netodia/wix/main.wxs
  - scripts/build-latest-json.py
  - scripts/build-appimage.sh
  - scripts/build-rpm.sh
  - scripts/build-tarball.sh
  - scripts/install-linux.sh
  - .github/workflows/release*.yml
  - .github/actions/build-manifest/action.yml
  - .github/actions/minisign-sign/action.yml
---

# In-app updater, packaging and release

The trust boundary is the GitHub repo — a manifest the publisher signed is trusted content, so the
threat model stops at transport and integrity, not at a hostile manifest.
`docs/adr/` argues it, including what the line costs and why the mitigations against a hostile
manifest are not worth building.

`main()`'s `--version` literal-first branch is the one piece of this that lives in the root
`CLAUDE.md`, because it is a forward-compat contract older clients depend on and you can break it
from `main.rs` without ever opening this file.

## Install methods

- **Two gates decide whether the updater exists, and they are not the same question.**
  `updater::is_available()` is the outer one: false on a source build, because `target/` belongs to
  cargo and a swapped-in release would be older than the tree above it and gone at the next build.
  It stops `updater_daily::spawn` in `boot::ui_setup::install` and clears
  `NetodiaUpdater.updates-supported`, which gates `UpdateSection.has-matches` and so takes the card *and* its settings-search hits.
  `is_system_install()` is the inner one and softer: the update is real, only the mechanism is the
  package manager's, so the check survives and Download/Skip become a hint. Reach for the right one
  — widening `is_system_install` to cover a dev build would offer a `sudo dnf update` hint to
  someone running `cargo run`.

- **Atomic-swap retains `.old` for rollback (Linux AppImage/tarball only).** Two-step rename:
  `target → target.old`, `staged → target`, smoke-test, rollback on failure; `.old` reaped on first
  successful boot, single source `install::old_path()`. The `pkexec mv` cross-fs fallback,
  `install_via_package_manager` (Linux RPM/DEB) and `install_via_msiexec` (Windows MSI) retain
  **no** `.old` — the package format owns the replace — and skip the smoke-test via the
  `InstallMethod` match in `download_and_install`. `main.rs`'s `.old` reaper (`prepare_process`) is
  `cfg(target_os = "linux")`, and macOS isn't a CI target, so `swap_in_place` falls through to
  `std::fs::rename`.

- **Windows installs are per-machine MSIs at `C:\Program Files\Netodia\bin\`.** `crates/netodia/wix/main.wxs`
  `Scope="perMachine"` + `ProgramFiles6432Folder`; UAC at install. The Start Menu shortcut
  Component under `ProgramMenuFolder` is what lets Windows Search find the app. Console suppressed
  via `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` — release runs as GUI,
  `cargo run` keeps the console for `RUST_LOG`. The in-app updater downloads the signed `.msi` to
  `%LocalAppData%\Netodia\update-staging\` and spawns `msiexec /i <staged> /qb!` non-blocking: UAC
  re-prompts, and WiX `MajorUpgrade` + `util:RestartResource` handle replacing the running binary.
  Smoke test skipped. `system_install::probe` short-circuits `windows-*-msi` keys to `false` so the
  updater UI stays visible — symmetric with `linux_pkg::detect`.

- **Polkit helper for in-app updater RPM/DEB installs.** `/usr/libexec/netodia-update-helper`
  argv-dispatches to dnf5/dnf/apt/apt-get; policy `com.github.helpmeeadice.netodia.update`.
  `install_via_package_manager` runtime-detects and falls back to a direct `pkexec dnf install`.
  **`LinuxPackageFormat` variants double as helper argv-dispatch keys.**

- **Replacing the binary under a running process breaks its own path two different ways, and only
  one of them is recoverable after the fact.** An RPM/DEB upgrade *unlinks* `/usr/bin/Netodia` and
  the kernel marks `/proc/self/exe` `" (deleted)"`; `utils::exe::current_exe` resolves that centrally
  (root `CLAUDE.md` argues the rule) and `install_target()` is the single updater consumer. **The
  marker can only appear mid-session** — you cannot exec an unlinked path — so the callers that
  meet one are the late ones: the post-exit respawn, and `spawn_install`'s pre-swap
  `install_target()` capture. `desktop_integration`'s `Exec=` line and `linux_pkg::detect`'s
  package-DB lookup both run at boot, so today they are defended without it — **and the second of
  those defences is one edit away.** `detect` runs at boot *and* caches: its `OnceLock` is primed
  by `is_system_install()` on a root-owned install and by `desktop_integration` on a writable one,
  and every later caller reads it — `run_check` on the daily task and the user's Check
  button, the panic hook through `current_target_key()`, and `install/staging.rs`. Drop the cache
  and those three ask a fresh `rpm -qf` mid-session, squarely inside the window. Keep the routing
  regardless, because the failure **compounds**: a marked path makes `rpm -qf` miss, so `detect`
  returns `None`, so the updater offers a tarball asset to an RPM install (and would swap it into
  root-owned `/usr/bin` rather than going through `pkexec dnf install`), the crash report's
  `install=` field names the wrong format, and `desktop_integration` stops skipping and
  BLAKE3-writes `Exec=/usr/bin/Netodia (deleted)` into the user's launcher. The atomic swap
  *renames* instead, and a rename leaves `/proc/self/exe` reporting the stale path with a straight
  face — nothing can undo that after the fact, which is why
  **`ui::window_chrome::set_respawn_exe`** still has to capture the target *before* the swap and
  stays load-bearing. Don't read the central fix as retiring it.

- **`services::platform::desktop_integration` self-deploys `.desktop` + icon on boot for tarball installs.**
  Compiled-in payloads `assets/desktop/Netodia.desktop.tmpl` (`@EXEC@`) +
  `assets/icons/netodia.png`; BLAKE3-gated idempotent writes to
  `~/.local/share/applications/…desktop` + `~/.local/share/icons/…/netodia.png`. Skipped on
  AppImage (`$APPIMAGE` set) and RPM/DEB. Don't move the source paths without updating the
  `include_*!` call sites.

## Manifest

- **`latest.json` is minisign-signed and the client verifies before parse.** Same key as the
  per-artifact sigs (`assets/updater-pubkey.b64`): `fetch_latest_manifest` fetches
  `latest.json.minisig` and calls `minisign::verify_manifest_bytes`, where the `manifest=true`
  trusted comment is a **domain-separation tag**. Fail-closed — missing or invalid sig →
  `AppError::Validation`. CI: `minisign -SHm latest.json -t "version=$VERSION manifest=true"`.

- **The manifest is built once after a Release is published.** `release.yml` reads the final
  release body, builds and signs `latest.json`, then uploads it with `SHA256SUMS.txt` and the
  signed packages. The manifest therefore uses the author-edited notes on the first run. A
  `workflow_dispatch` builds packages for download as Actions artifacts and does not change a
  Release or its manifest. `.github/actions/build-manifest` receives an `--artifacts` directory
  holding installable artifacts and their `.minisig` siblings and nothing else — the script
  `SystemExit`s on anything it cannot classify.

- **Manifest schema gate + critical-release flag** (`crates/netodia-app/src/services/updater/manifest.rs`).
  `manifest_schema_version: u32` (default 1) — `check.rs` returns `Verdict::UnsupportedSchema`
  when `> SUPPORTED_MANIFEST_SCHEMA`, treated like `NoAssetForTarget`; bumping it means bumping
  `build-latest-json.py`'s `--manifest-schema-version` and the CI invocation. `critical: bool`
  (default false) hides "Skip this version" and bypasses the `skipped_release` filter, set via
  `--critical`.

## Release matrix

- **Build provenance attestation.** `release.yml` runs `actions/attest-build-provenance` (v4,
  SHA-pinned) in every slot, needing `id-token: write` + `attestations: write`. Verify with
  `gh attestation verify <file> --repo KenanSalar/Netodia`. Upstream now calls v4 a thin wrapper
  over `actions/attest` and points new work at that directly — worth folding in next time this line
  is touched.

- **aarch64 builds alongside x86_64** (Linux + Windows): the four matrix slots `release.yml`
  declares, one per OS and architecture (`ubuntu-24.04-arm`/`windows-11-arm` for ARM).
  Each architecture ships Linux tarball/AppImage/RPM/DEB and Windows MSI/portable ZIP.
  **`build-latest-json.py`'s `PLATFORM_PATTERNS` is downstream of `release.yml`'s
  packaging steps and the `scripts/build-*.sh` they call, and must move with them.**
  Every pattern pins an explicit arch token, so the two arch groups are disjoint and the
  aarch64-first ordering is readability rather than disambiguation — that ordering existed for
  cargo-deb's native `_arm64.deb`, which carries no leading-arch token, and `release.yml`
  renames the deb to the `netodia-<tag>-<arch>.deb` scheme every other slot produces so its
  `netodia-*.deb` globs match. The table was left on the old name and **v0.8.0 shipped with no deb
  entry at all** while both signed `.deb` files sat on the release, leaving deb clients on
  `NoAssetForTarget` indefinitely; it survived because `classify()` returning `None` was a bare
  `continue`, which is the guard now spelled above. Client `target::current_target_key()` is
  `cfg!`-branched per `(target_os, target_arch)` and its key strings are that same table's values,
  so a renamed key breaks both ends. All three `build-{tarball,appimage,rpm}.sh` read `ARCH`
  (default `uname -m`), and the AppImage's per-arch `linuxdeploy` SHA256s bump in lockstep.

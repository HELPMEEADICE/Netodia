---
paths:
  - .github/**
  - packaging/**
  - licenses/**
  - crates/netodia/wix/main.wxs
  - scripts/build-rpm.sh
  - scripts/build-appimage.sh
  - scripts/build-tarball.sh
  - scripts/install-linux.sh
  - scripts/build-latest-json.py
  - Cargo.toml
  - crates/netodia/Cargo.toml
  - rust-toolchain.toml
  - clippy.toml
  - .cargo/audit.toml
---

# CI and packaging

The package matrix and updater contract live in .claude/rules/updater.md; this rule covers the
separate workflows that check, build and publish those packages.

## Checks

.github/workflows/ci.yml runs quality checks on pull requests and pushes to main. Its check jobs
run independently; the quality aggregate waits for all of them and fails unless each result is
success. The release and manual build workflow is independent of CI.

- fmt runs cargo fmt --all --check with the pinned toolchain.
  It also runs the Python release-version regression tests, which need no Rust toolchain.
- audit uses .github/actions/cargo-audit; keep the advisory policy in that one composite. It scans
  the committed Cargo.lock and needs neither Rust compilation nor system dependencies.
- quality-linux installs .github/actions/linux-system-deps, explicitly runs
  cargo check --locked -p netodia-ui to compile the Slint sources, then runs workspace Clippy
  with -D warnings.
- test-linux runs cargo test --locked --workspace. AppState::init opens the default audio device,
  which hosted runners lack; .github/actions/headless-audio points ALSA at its userspace null PCM.
  Keep the system dependencies shared with release packaging.
- quality-windows runs workspace Clippy on windows-latest, which checks cfg(windows) code and test
  targets with MSVC.
- test-windows runs the workspace tests but skips only headless_scan_persists_track; WASAPI has no
  equivalent headless device here. A name filter keeps any new Windows test enabled by default.
- The Linux quality/test jobs and Windows test job cap CARGO_BUILD_JOBS at 4 and use
  line-tables-only debug info. The netodia-ui build is the memory peak; full debug info also makes
  Windows PDB linking much slower. Windows Clippy has no jobs cap because it does not reach the
  codegen peak. Keep separate ci-clippy and ci-test rust-cache keys, and pass the same key on Linux
  and Windows; the action adds OS and architecture to its cache key.

The workflow checks Rust formatting, Slint compilation, Clippy, the Linux and Windows test suites,
and RustSec advisories. Keep the quality job's needs list in sync with every check job. The
release build does not depend on the aggregate.

## Release and manual builds

A published Release event builds and uploads packages to that release. Drafts start building when
they are published. scripts/prepare-release.py validates the v-prefixed SemVer tag and sets
[workspace.package].version in the runner checkout before compilation and packaging. The runner
then uses cargo update --workspace to synchronize workspace versions in Cargo.lock while retaining
third-party dependency pins; the build still uses --locked. Publish resolves the same tag version
for signatures and latest.json. Manual runs keep the committed manifest and lockfile. The build matrix
has six formats for each supported architecture: Linux tarball, AppImage, RPM and DEB; Windows MSI and
portable ZIP; x86_64 and aarch64. Each OS/architecture combination uses one runner to build once
and package every format for that target. BINARY must point at
target/<target-triple>/release/Netodia because the Linux package scripts consume that environment
variable. Keep artifact names compatible with scripts/build-latest-json.py's PLATFORM_PATTERNS.

Release packages are minisign-signed and carry build provenance attestations. After all matrix
slots finish, publish verifies signatures, generates and signs latest.json from the final published
release notes, creates SHA256SUMS.txt, and uploads them with the packages. Release publishing
requires MINISIGN_SECRET_KEY (the entire secret key file encoded as base64) and MINISIGN_PASSWORD
repository secrets. The release-signing preflight signs a small payload and verifies it against
assets/updater-pubkey.b64 before any matrix build starts, catching missing secrets, wrong passwords
and mismatched key pairs. Manual
dispatches run the build matrix independently of CI, then expose unsigned package files as Actions
artifacts for 30 days; they do not update a Release or the updater manifest.

Every external action is SHA-pinned with a trailing version comment. Use .github/actions/setup-rust
for every job that compiles Rust; it installs the version and components from rust-toolchain.toml.
The Linux dependency and headless-audio composites explain their runner requirements at the action
source.

## File associations — five Linux spellings and one Windows one, all pinned

The `MimeType=` list makes Netodia *offerable*; the `Exec=` field code is what makes the handoff
work, and the two shipped apart for a whole release.

- **All four `.desktop` sources end `Exec=` with ` %F`** (`scripts/Netodia.desktop`, the two build
  heredocs, `assets/desktop/Netodia.desktop.tmpl`). Not `%U`, a list of *URLs* the spec lets arrive
  as `file://` and so obliges percent-decoding; not `%f`, one process per selected file.
  `all_desktop_sources_agree_on_mime_and_wmclass` matches **extracted lines**, every one of them —
  two sources are whole shell scripts, where a comment about the line reads exactly like the line,
  and a second heredoc is what checking only the first would walk past.

- **`scripts/install-linux.sh` is the fifth, and it is a rewriter.** It seds the `Exec=` of the
  same file the DEB ships verbatim; anchored at `.*` it ate the field code too, so `%F` lived in
  the DEB and died in the tarball. `s|^Exec=[^ ]*|` takes the command token only, pinned both ways
  by `the_tarball_installer_rewrite_keeps_the_field_code`. Its quoting arm answers the same
  question as `desktop_integration::render_desktop`'s — Exec is parsed with shell-like quoting, so
  a path with a space needs them, **only when it does** — but not with the same predicate: the
  shell `case` tests for a space, `quote_exec` for the spec's whole reserved set and its two
  unescaping layers. Deliberate; a `$` or a backtick in `$HOME` doesn't earn shell-quoting
  gymnastics in an installer.

- **Windows is `crates/netodia/wix/main.wxs`'s `FileAssociations` component, plain `RegistryValue` rows.** WiX's
  `ProgId`/`Extension`/`Verb` predate Vista: no `Capabilities` + `RegisteredApplications` (what
  Win10/11 reads for Default apps), and `Extension` claims `HKCR\.mp3`'s default outright.
  **`ApplicationDescription` is required** — without it the app is absent from that list and every
  key under it unreachable. `MultiSelectModel=Player` hands a whole selection to one invocation.
  `the_msi_offers_every_audio_extension` walks `utils::audio_ext::AUDIO_EXTENSIONS` against the
  comment-stripped wxs, the one format no Linux runner builds.

## Licences — every format ships `licenses/`, and the six spellings are pinned by name

Why every artifact owes the text, and why six formats exist to owe it, are both in
`docs/adr/`. What lives here is the consequence: a format that quietly stops shipping it fails nowhere until a
packager files it, so the pins are named rather than reviewed.
**`crates/netodia/tests/packaging.rs`'s `every_package_format_ships_the_licenses_dir` holds a named list**
(`build-rpm.sh`'s `%license`, the binary manifest's asset glob, `build-tarball.sh`'s `cp`,
`build-appimage.sh`'s `cp`, `main.wxs`'s `File` set, and the workflow's portable ZIP copy), each
needle the *mechanism* rather than the word. Named because the set of formats is closed; the *font*
set is open, so its sibling pin walks the directory instead.

- **Five of the six include the directory and WiX does not**, so a new licence file is free
  everywhere except `main.wxs`. `the_msi_names_every_licence_file` **walks `licenses/`** and fails
  on any file the wxs doesn't name. The two aren't redundant: the named list catches the MSI
  dropping the directory (a deliberate act), the walk catches a file going missing from it (an
  omission).

- **The RPM is in that four only because `%license` takes `licenses/*`** — spell the three out and
  Fedora acquires WiX's per-file edit without WiX's excuse. It is also the one format whose needle
  is not the statement that stages the files: `build-rpm.sh` copies them into the source tarball
  *and* names them in `%files`, and only the second ships anything, so the pin reads the `%license`
  line (delete it and the staged copy goes unread, with no warning anywhere).

- **`packaging/debian-copyright` is copied verbatim only because it opens with a DEP-5 key** —
  cargo-deb's `has_copyright_metadata` scans the first ten lines, and without one it *generates* a
  copyright from `license` + `authors`, declaring the whole package AGPL by one author, which the
  fonts, winit and the four copyleft crates falsify. **Hence `release.yml` pins cargo-deb to
  an exact version**: that check and the bare-string `license-file` spelling both live in its
  `config.rs`, not its README, so a bump can narrow either with nothing to say so, and no test can
  see it — bumping means re-reading `has_copyright_metadata`, not editing a number. Policy 12.5
  lets a package reference `/usr/share/common-licenses` only for what ships there, so the AGPL and
  OFL bodies are **quoted** while Apache-2.0, GPL-3 and MPL-2.0 are referenced, and
  `the_debian_copyright_quotes_the_licences_it_ships` re-derives both quoted ones from their
  sources rather than trusting the copy. **MPL-2.0 sits on the referenced side and is the one to
  check before moving**: Policy 12.5's own footnote names its path and base-files ships it, so a
  quoted body there is one the file was asked not to carry. Nothing catches that either way, since
  lintian's `copyright-file-contains-full-*` tags reach GPL, LGPL, GFDL and Apache-2.0 only.

- **`MPL-2.0` and `GPL-3.0` are standalone `License:` paragraphs no `Files:` stanza names, and
  lintian's `unused-license-paragraph-in-dep5-copyright` on those two is expected.** DEP-5
  describes a *source tree* and those terms reach the package through crates that have none here,
  so the only way to silence the tag is a `Files:` stanza pointing at source that does not exist,
  which is a lie in the one file whose whole job is not telling them. Nothing in this repo runs
  lintian, so the tag is a downstream packager's view rather than a gate; if it ever becomes one,
  the answer is a `usr/share/lintian/overrides/netodia` asset, never deleting the paragraphs. The
  half a deletion *would* break silently is the reference, so both are pinned.

- **`licenses/` may not fall through to `Files: *`.** Those texts are the FSF's, the ASF's,
  Mozilla's, SIL's, the Vazirmatn authors', and Adobe/Google's, reproduced because their own licences require it,
  and the catch-all stanza declares whatever it reaches AGPL by one author. It is the Ko-fi
  stanza's argument over five more files and it looks like nothing in a diff, since no byte of
  `GPL-3.0.txt` changes when the package starts claiming it. One globbed stanza covers them under
  a `verbatim-licence-text` short name, with `licenses/ATTRIBUTION.txt` carved back out *below* it,
  DEP-5 applying the last stanza that matches. Pinned three ways in
  `the_debian_copyright_quotes_the_licences_it_ships`: the glob, the carve-back, and that every
  short name a stanza uses has a paragraph defining it.

- **`LICENSE` is an input to those tests**, so keep the PR workflow unfiltered by file paths:
  compiling nothing is not the same as being unexercised. The other needles live in `scripts/`,
  `crates/netodia/Cargo.toml`, `crates/netodia/wix/` and `.github/workflows/release.yml`.

- **A face added under `crates/netodia-ui/ui/assets/fonts/` owes an entry in
  `licenses/ATTRIBUTION.txt`** — `every_bundled_font_is_named_in_the_attribution` walks the
  directory rather than listing the faces, keying on each face's repo-relative path, the family
  name not being derivable from the file.

## The workspace split, as the release workflow sees it

**Before adding a member, grep `.github/workflows/release.yml` and the packaging scripts for `netodia`.**
The split to `netodia` + `netodia-ui` broke that workflow twice, both times on something
identifying a thing by *name* that was unambiguous with one member, and both deep in a matrix slot:

- `cargo wix` picks a package for you only when the workspace has exactly *one*, so the MSI step
  names `--package netodia`. It selects the *manifest*, and cargo-wix then reads `wix/main.wxs`
  relative to that, which is why the wxs sits under `crates/netodia/` rather than at the repo root
  behind an `-I`: that flag's resolution base is undocumented and nothing short of a tagged Windows
  release would exercise it.

- Artifact upload patterns should stay extension-qualified (`.tar.gz`, `.AppImage`, `.rpm`, `.deb`,
  `.msi`, `.zip`, and `.minisig`). Release builds and manual builds share the same matrix; every
  matrix slot must produce at least one matching package, and the artifact upload fails if it does
  not. Release-only signing and provenance use the same package extensions.

The release-version preparation and three Linux packaging scripts read `[workspace.package]`
rather than asking cargo. The shell scripts anchor on the table by name, and every member carries
`version.workspace = true`.

**A toolchain bump moves four things in lockstep** — `rust-toolchain.toml`, `Cargo.toml`'s
`[workspace.package] rust-version`, `clippy.toml`'s `msrv` (an explicit clippy msrv *overrides* the
Cargo one), and the two docs (`README.md` prerequisites + the root `CLAUDE.md`) — and should expect
fresh `pedantic`/`style` lints on the first run.

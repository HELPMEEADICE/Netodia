---
paths:
  - crates/netodia-views/src/ui/window_chrome/**/*.rs
  - crates/netodia-views/src/ui/shell/tray_bridge.rs
  - crates/netodia-views/src/ui/shell/event_sink.rs
  - crates/netodia-platform/src/services/platform/tray/**/*.rs
  - crates/netodia-integrations/src/services/integrations/media_controls/**/*.rs
  - crates/netodia-platform/src/services/platform/always_on_top/**/*.rs
  - crates/netodia-platform/src/services/platform/dwm_titlebar.rs
  - crates/netodia-views/src/ui/appearance/theme_apply.rs
  - crates/netodia/src/main.rs
  - crates/netodia/src/shutdown.rs
  - crates/netodia/src/boot/**/*.rs
  - crates/netodia-ui/ui/app-window.slint
  - crates/netodia-ui/ui/components/custom-titlebar.slint
  - crates/netodia-ui/ui/components/caption-buttons.slint
  - crates/netodia-ui/ui/components/macos-titlebar-cluster.slint
  - crates/netodia-ui/ui/components/macos-traffic-light.slint
  - crates/netodia-ui/ui/layout/resize-ring.slint
  - crates/netodia-ui/ui/views/settings/window-chrome-section.slint
  - crates/netodia-ui/ui/views/settings/mini-player-section.slint
  - crates/netodia-ui/ui/views/mini-player.slint
  - crates/netodia-ui/ui/components/mini-player/*.slint
  - crates/netodia-views/src/ui/appearance/window_settings.rs
---

# The desktop shell — window chrome, tray, media keys

The app's relationship with the OS window server and the desktop's notification/media surfaces.
The theme running through it: **Slint's window API is a cache, winit is the truth**, and anything
the OS owns has to be attached late or not at all on at least one platform.

## Window chrome

- **Window-control APIs go through winit, not Slint.** Slint's `set_minimized`/`set_maximized`
  cache stalls on Wayland: `WinitWindowAccessor::with_winit_window(|w| w.set_minimized(true))`.

- **Window dragging belongs at the winit layer.** `drag_window()` from Slint `pointer-event(down)`
  leaks the input grab. Each drag region's `TouchArea` reports its hover under its own name via
  `WindowChrome.drag-region-changed(DragRegion)`; `on_winit_window_event` intercepts
  `MouseInput { Pressed, Left }` over either → `drag_window()` → `PreventDefault`.
  **So no `double-clicked` on a drag region can fire**, Slint never seeing the press: the
  titlebar's double press is read in `window_chrome::drag_region`, and the miniplayer names itself
  so its own only moves.

- **Resizing a frameless window takes the same path, and the ring's hover is the whole geometry.**
  `ResizeRing` reports a `ResizeZone` via `WindowChrome.resize-zone-changed`, kept by
  `window_chrome::resize_grab`, and the filter's resize arm (`drag_resize_window`) is matched
  **before** the drag arm, since the miniplayer's drag region runs to the edge and its atomic can
  still read true on a press that lands on a grab. The `Window`'s `resize-border-width` stays
  unbound: Slint's handler for it gives a corner only a band-wide square, which on a rounded window
  sits in the transparent cut-out, and compares the logical band with a physical cursor, so beside
  the ring the press disagreed with the cursor. macOS answers `NotSupported` and keeps AppKit's own
  edge resizing.

- **A caption style is a chip index on the Slint side, and `settings.json` never sees it.**
  `TitlebarButtonStyle` persists by name, but every Slint property carrying it holds the index
  `window_settings::{idx_for, style_for}` maps to. The `options` order of each chip group *is* that
  index, in `window-chrome-section.slint` and in `mini-player-section.slint`, and
  `custom-titlebar.slint` and `MiniCaptions` pick their mounts by comparing against it. A new style
  is a variant, both arms of the mapping, a chip in both groups, a case at both mounts and a row in
  `window_settings::tests`' `CHIPS`, which pins the chips and the mounts to the mapping; a reordered
  array labels its chips with styles they don't select. A drawn style goes through
  `CaptionButton`'s `CaptionStyle` and adds no mount, so only a cluster sharing nothing with it,
  like the traffic lights, needs a branch of its own.

- **On Win32 a resize or move drag parks winit's loop, and with it every Slint `Timer` and
  `changed` handler**, so the whole responsive layer — the miniplayer swap, grid column counts,
  each `changed width` mirror — waits for the button to come up. `window_chrome::parked_loop` is
  what keeps them running and carries the argument. The filter's `RedrawRequested` and `Moved` arms
  pump from the events the modal loop still delivers. A heartbeat thread carries the tick past the
  last of them, armed only once `LoopTicks` (installed on the backend in `main.rs`) has seen no
  `NewEvents` between two pumps, so the ordinary loop never wakes for it. A new drag-reachable winit
  arm is where a third pump site would go.

- **The Native Title Bar toggle restarts** via `DialogKind.restart-titlebar` →
  `window_chrome::request_respawn_and_quit`; hydrate `Theme.use-native-titlebar` *before*
  `app.show()` so the window maps with the right frame. Slint applies `no-frame` live, so the frame
  is not what the restart is for; `window_chrome`'s module doc says what is.

- **Under the native title bar the miniplayer drops the frame**, through `app-window.slint`'s
  `frameless`, and that couples three trees. Win32, macOS and `KWin` all keep the window's rect
  across that change, so the client area grows into the frame it lost: the winit `Resized` arm
  measures **the step the client takes when the decoration changes**
  (`window_chrome::geometry::frame_allowance`), `WindowChrome.frame-allowance-*` holds it across the
  frameless span, `MiniPlayerSwitch` widens its exit edge by it and `restore_full_player` hands it
  back. Drop any link and a window parked just inside the threshold bounces in and out.
  **The step is measured rather than asked of the window because the desktop it bites hardest on
  cannot be asked**: a Wayland compositor draws its decoration outside the surface, so winit's
  `outer_size` reads the client straight back and a frame query is zero exactly where the client
  really did grow by a titlebar. That is argued at `frame_allowance`, which also says why the
  measurement retired the platform split `with_returning_frame` used to carry.
  **The desktops it doesn't bite answer that change with no reading at all**, X11 and a compositor
  drawing its own decorations both taking a dropped frame out of the window rather than handing it
  to the client, **so a step counts only while the decoration change is recent enough to be what
  moved the client**. `changed frameless` announces it over `WindowChrome.frame-changed` and
  `geometry::wire_frame_changes` stamps the moment; that link fails silently in both directions.
  Unwired, every step goes stale and the allowance sits back at zero. Unbounded, the leave's own
  resize is read as a frame, the exit edge lands above the window the restore just asked for and
  the miniplayer never leaves again. The
  window's own minimum gives the reading up while a frame stands: Win32 fixes a resize drag's
  minimum when the drag starts, framed, and a drag from the full UI otherwise can't take the
  frameless miniplayer down to its floor until the button comes up. The
  frame changes on the swap tick in **both** directions, under the crossfade. Returned at the
  decision to leave instead, it came back over a miniplayer still on screen, and Win32 stepped the
  visible window in by its invisible resize borders before the full UI appeared. **Those borders
  are the fourth link**: the client takes them over with the frame, so the `Resized` arm also
  measures them (`geometry::frame_margins`, Win32 only), `WindowChrome.frame-margin-*` holds them,
  and the shell and its overlays inset by them under the native miniplayer, leaving them
  transparent. The root's `resize-band` widens to cover them, so they still resize as the frame's
  borders did and the visible window keeps its edges across the swap.

- **A close from the miniplayer reopens the miniplayer**, and the launch is where the frame
  argument above bites hardest. `geometry::snapshot_into` persists the window as it closed plus
  the held full player as `full_player_geometry`, and `geometry::restore` seeds the hold back so
  the restore caption still has somewhere to go; `hold_full_player` keeps a hold already there,
  since a relaunched miniplayer's settled placement is the miniplayer itself. **`restore` ends in
  `adopt-restored-size()`, and removing it builds and looks right on Linux.** The window is created
  with the `no-frame` it had at `show()`, while the seed `Timer`'s `render-active` only reaches it
  a loop pass after `resumed` has built the window: framed, then dropped, and Win32 keeps the outer
  rect when the frame goes, so a native-titlebar miniplayer grew by the frame on every launch.
  **The window floor is spelled once, `MiniPlayer.window-min-*`**, read by the root's minimum and
  by `restore`'s guard against a hand-edited size. The restore caption keeps its own higher floor,
  which has to clear the miniplayer's exit edge.

- **`DialogKind.restart-backdrop` is the third of these and the one whose deadline is earlier than
  `app.show()`** — `BackdropFlags.aurora_backdrop` decides whether the two artwork tiers hold a
  `BlurSpec` at all, so `boot::ui_setup::chrome::apply_backdrop_style` raises it ahead of
  `install_views` rather than in `hydrate_ui_from_settings`. Same three-part shape as the tray's:
  `WindowChrome.restart-backdrop()` → `controls.rs::on_restart_backdrop`
  (`library::window::set_aurora_backdrop` + `request_respawn_and_quit`). Why it can't be live is
  `.claude/rules/ui-patterns.md`'s.

- **A restart can refuse, and the check has to sit on this side of the exit.** All four restart
  paths — titlebar toggle, tray toggle, backdrop-style toggle, the updater's "Restart Now" — go
  through
  **`window_chrome::request_respawn_and_quit`**, which resolves `respawn_target()` *before*
  setting `RESPAWN_AFTER_EXIT` and quitting: past `slint::quit_event_loop()` the window is gone, so
  a failed `exec` in `shutdown::respawn_if_requested` has nothing to fall back to and the app
  simply vanishes. With no binary to come back to it stays up and raises a sticky
  `ToastKind::RestartRequired`; every caller has persisted its setting by then, so a toast asking
  the user to close and reopen is an honest answer — the change lands on the next manual launch.
  `respawn_target()` is the updater's recorded pre-swap path (`set_respawn_exe`) if there is one,
  else `utils::exe::current_exe()` — the marker-resolving form, so a package upgrade mid-session
  can't hand back a `<path> (deleted)` string. Don't inline the flag store + `quit_event_loop` pair
  at a fourth site; outside `window_chrome` you can't, both statics being private, so the rule only
  binds *inside* that module.

- **Transparent ARGB window for the rounded outline.** Slint's winit backend creates every window
  `with_transparent(true)`; `Window.background: Colors.transparent`; the shell, a rounded
  `Rectangle` (`clip: true`, `border-radius: Theme.window-radius`), holds everything the window
  paints, with only `ResizeRing` beside it, which paints nothing. Opaque + square when
  `is-maximized` or while an OS frame stands (`!frameless`). `window-radius` is the user's pick
  under the custom titlebar and the host's (`native-content-radius`) under the native one, so the
  native miniplayer keeps the frame's corners when it drops the frame.

- **The shell's clip is the only antialiased pass the corner may get.** The general rule is
  `slint-pitfalls.md`'s; beside the OS frame the harder arc is plain to see at the same radius.
  That is why the shell has no `background` of its own (its mantle is a square child), why the three
  overlays mount inside it (their scrims otherwise square off the corners over the desktop), and why
  the outline is an `EdgeOutline`. `app-window.slint` argues each at its mount; a new element
  reaching the window's edge goes inside the shell too.

- **The content panel inside the shell is rounded without a clip.** A rounded one held the whole
  view as a texture, rendered again on every scroll and visualizer frame, and under the custom
  titlebar nested inside the shell's own. The panel's clip has no radius and `RoundedFrame` masks
  its corners in `WindowChrome.mantle`, which is right only because the shell's ground is all that
  shows around the panel. Something painted there later needs the masks rethought, not recoloured.

- **A frameless window draws its own 1 px outline**, the edge every OS frame has, as an
  `EdgeOutline` mounted last in the shell under the same `frameless && !is-maximized` gate, so it
  outlines the whole window under the custom titlebar and only the miniplayer under the native one.
  The Settings rows stay live in both modes for that reason. `ui::appearance::window_border` paints
  `WindowChrome.border-color{,-unfocused}` from the palette republish; its System colour is the
  Windows accent while the user has it on window borders (`platform::window_border`, the registry),
  and Windows says nothing when that switch moves, so the winit `Focused` arm asks again on a
  focus gain.

- **Match Unfocused Window Background (KDE-only)** tints the full UI's chrome to the OS unfocused
  titlebar. `LayoutFlags.match_unfocused_to_system_bg`; hidden off-KDE, disabled in
  custom-titlebar. **A fresh KDE install opens on the native titlebar with the tint on, both set
  at first launch** (`WindowFlags::first_launch`, `LayoutFlags::first_launch`); the field's serde
  default `is_kde_desktop()` reaches only a file missing the key, never a fresh install.
  `Theme.window-focused` mirrors winit `Focused(bool)` raw.
  **The gate is one brush, `WindowChrome.mantle`, which argues its three terms and the missing
  `animate`**, and every surface the tint covers reads it: the sidebar, the now-playing bar, the
  shell's ground showing through the gutter beside the content panel, the panel's corner masks,
  which paint that ground over the panel, and the swap's crossfade, which has to paint that
  ground's brush. A surface spelling the ternary again, or `Theme.mantle` where the chrome shows,
  leaves a strip that stays dark on an unfocused window.

- **Always-on-top** is winit's window level on Windows, macOS and X11, and D-Bus to KWin or GNOME
  (`window-calls` ext.) on Wayland; bare GNOME falls back to native decorations.
  **Nothing done through `with_winit_window` before `app.show()` reaches the OS.** Slint creates
  the winit window only once the loop runs, the accessor answers `None` until then, and a
  `let _ =` swallows that. The restored pin went that way and painted an active icon over an
  unpinned window. Wait with `winit_window().await`, as `window_chrome::seed_always_on_top` does,
  or ride the attributes hook, as maximized does.
  **On KWin the pin is read back as well as written**, because the window manager's own keep-above
  titlebar button moves it without telling the client. `always_on_top::kwin::watch_keep_above`
  leaves a script loaded for the session that calls `Changed(b)` on Netodia's connection, and
  `window_chrome::follow_reported_pin` mirrors each report into `WindowChrome.always-on-top-active`
  and `library::window::record_always_on_top`. A report matching the button is Netodia's own
  toggle coming back and writes nothing. Three things hold it together, each argued at its
  definition: the receiver is sequential (`spawn = false`), the script tracks windows added later
  under both Plasma signal names, and it is named per data root so a relaunch replaces its
  predecessor. `kwin_tests` pins the script against the interface it calls. GNOME's `window-calls`
  and the winit arm report nothing, so there a pin moved from the titlebar still leaves the button
  behind.

- **OS file drag-and-drop rides the vendored winit fork** — stock 0.30.13 has no `wl_data_device`.
  `winit/` is 0.30.13 + 3 commits (PR #4009, `WindowId` fix, URI percent-decoding, cfg-gated to
  Linux), wired by `[patch.crates-io]`. Flow: `winit_filter.rs::DroppedFile` →
  `drop_coalescer.rs::schedule_drop_flush` (50 ms → `queue_import_files`);
  `HoveredFile{,Cancelled}` toggle `Queue.is-drop-hovered`. Don't bump winit without a rebase +
  re-sync — the root `CLAUDE.md`'s Known Gaps has what retires it.

## Opening a file from the file manager

The other way paths arrive from outside, and the one that can arrive before there is a window.
`main()`'s ordering constraints are the root `CLAUDE.md`'s; this is the shape.

- **Two verbs, and they are not one.** A drop appends (`queue_import_files`, routed by
  `drop_coalescer`, *discarded* when neither the queue sheet nor a playlist is open); an open
  **replaces the queue and plays** (`queue::open_files`). They share `sort_for_queue` and nothing
  else. **Neither existing fn was the whole answer**: `queue_import_files` never sets
  `current_index`, and `player_play_tracks` wants ids in order where
  `ImportFilesResult::track_ids` is partly `HashMap` order.

- **A cold start with files skips resume-on-startup** — `open_startup_files` runs synchronously
  after `restore_persisted_playback`, so resume would only be visible for the moment it takes them
  to land. It is also **too early to toast**: that bridge installs with the UI and drops rather than
  queues, so a cold-start failure can only log.

- **A warm one raises through `tray_bridge::raise_window`**, the only thing that knows whether the
  window is hidden (tray or minimize → `show_window` + `SAVED_WINDOW_GEOM`) or merely buried (→
  winit `focus_window`, a documented no-op on Wayland). Raised either way — an empty forward is
  someone launching Netodia to get at the window.

- **"The name is taken" has two spellings and `interprocess` normalises neither.** Unix `bind`
  says `EADDRINUSE`; a Windows named pipe is created under `FILE_FLAG_FIRST_PIPE_INSTANCE`, whose
  second instance fails `ERROR_ACCESS_DENIED` — `PermissionDenied`, nowhere near `AddrInUse`.
  Matching only the first left *every* Windows relaunch `Claim::Unenforced`: a second window and a
  second writer over one database, on the platform the MSI registers associations for.
  `name_is_taken` is the single place that decides, and **its pure half takes the platform as an
  argument** — `utils::redact::redact_prefix`'s shape, for the same reason. A `cfg!` inside the
  predicate is a branch the Linux gate compiles out and can never exercise, so a
  "simplification" back to one spelling merges green; `name_is_taken_on` is what
  `a_taken_name_is_recognised_in_both_spellings` can ask both ways. A genuine ACL denial takes the
  same arm and fails at the connect, landing back on `Unenforced`, which is where it belonged
  anyway.
  **Recognising the name is only half of it — the forward has to survive the transport too.** A
  named pipe has no settable I/O timeout, so `forward`'s deadline came back `Unsupported` and
  propagated, landing on the same `Unenforced` arm and the same second window; `allow_missing_timeout`
  carries that argument and is called at all three deadline sites. **What the deadline was *for*
  doesn't go away with it, and neither site may hold a blocking read on a thread something else
  needs**: `spawn_reader` takes each connection off the accept loop under a cap, `wait_for_close`
  bounds the forwarder's ack the same way. Reach for a thread there, never a timeout — one
  transport won't take one.

- **The accept loop is a detached `std::thread`, not `spawn_blocking`**, as `discord/ipc.rs` runs
  its transport: a parked blocking-pool tenant is what the 32-slot cap exists to prevent. Its
  read-failure arm still calls `on_launch` with no paths: the connection is proof of a launch, and
  a selection past `MAX_PAYLOAD_LEN` should cost the user their file list, not their window.

- **The frame declares its length, and that is not decoration.** Reading to EOF instead means the
  receiver waits on the sender's close while the sender waits to know the payload landed — **both
  block to their 2 s timeouts and the paths are dropped**, which every codec test passes straight
  through. `a_second_launch_hands_its_paths_to_the_first_and_stands_down` is why a real socket
  earns its setup. The sender *does* wait for the close, deliberately: exiting straight after the
  write leaves the payload in a pipe buffer with its own handle the last one open, on the platform
  no Linux runner covers.

## Tray and media keys

- **OS media controls** — MPRIS over `zbus` on Linux, souvlaki 0.8 for SMTC and macOS
  `MediaPlayer`. Bounded `mpsc` (cap 32) decouples the callback thread from `PlayerState`,
  `EventSink` from Slint. **Windows SMTC deferred** — souvlaki panics on a null
  `HWND` and no OS window exists at `AppState::init`, so `init_media_controls()` leaves Windows
  inert; `boot::ui_setup::install` posts a one-shot post-show `invoke_from_event_loop` grabbing
  the `HWND` and calling `MediaControlsHandle::attach_smtc`, a newly-attached `true` triggering a no-op
  `with_state_emit` to flush playback. Linux MPRIS / macOS MediaPlayer attach eagerly; `event_tx`
  retained Windows-only for the late rewire.

- **System tray** — `crates/netodia-platform/src/services/platform/tray/` cfg-split (Linux `ksni`, Win/mac `tray-icon 0.24`) behind
  a `mod.rs` façade (`TrayAction`, `TraySnapshot`, embedded `tray.png`, `init_tray`).
  `ui/shell/tray_bridge.rs` runs one task off a bounded `mpsc<TrayAction>`: playback reuses the
  media controls' `EventSink`, `ShowHideWindow`/`Quit` hop to the UI via `invoke_from_event_loop`,
  a `sinks.view_model` subscriber pushes tooltip + play/pause label. Linux eager; **Win/mac deferred,
  and dropped by `tray_bridge::shutdown()` before `process::exit` or the icon ghosts**. No SNI host
  → `init_tray` `None`/`false`, tray-less still usable. Menu labels come from the bundled
  translations on `WindowChrome`; `on_locale_changed` republishes them even while playback is idle.
  `TrayFlags.tray_enabled` (default on) gates `tray_bridge::install` from
  `boot::ui_setup::install`; flipping it is restart-gated through `DialogKind.restart-tray` → `WindowChrome.restart-tray()` →
  `controls.rs::on_restart_tray` (`library::window::set_tray_enabled` + `request_respawn_and_quit`,
  which may decline — above).

- **Every shell surface uses the supplied Netodia logo.** `assets/icons/netodia.png` is the shared
  256px artwork mounted by the titlebar and Linux launchers. `assets/icons/tray.png` is its 64px
  tray rendition, and `assets/netodia.ico` carries the same artwork at Windows shell sizes. The
  tray does not recolour its opaque background for light taskbars or panels.

- **Close-to-tray** (`TrayFlags.close_to_tray`, default off) — Slint `Window::hide/show` on
  `should_hide_to_tray()`, gated on the setting **and** a live tray (`SettingRow.disabled` when
  `Settings.tray-active` false). Hide snapshots `SAVED_WINDOW_GEOM`; re-show re-asserts it from a
  self-rescheduling 16 ms timer (`reschedule_geometry_restore`, cap `RESTORE_ATTEMPTS`).
  `WINDOW_VISIBLE` shadows visibility — `is_visible()` is `None` on Wayland.

## Shutdown

- **Force-exit.** `main()` ends in `std::process::exit(0)`; a normal return lets accesskit's a11y
  thread and any tokio worker parked on a blocking call pin the process.
  The audio output used to be a third and no longer is: `PlaybackEngine` owns it, and `main()`
  closes it explicitly after `save_state_on_exit` (`PlaybackEngine::close_output`), since an
  exclusive claim hands the device back, and the device's own volume with it, only in a destructor
  the exit skips. `main_order_tests` pins it ahead of both ways out.
  `tracker.wait()` + `db.close()` in a 3 s `timeout`, runtime dropped on a background thread;
  `save_state_on_exit` flushes synchronously *before* the timeout.

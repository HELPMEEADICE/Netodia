---
paths:
  - crates/**/*.rs
  - Cargo.toml
  - crates/*/Cargo.toml
---

# Unsafe Rust — the posture, and what to reach for instead

`unsafe_code = "deny"` sits in `[workspace.lints.rust]` and every member inherits it.
Two things follow, and they're the whole of this file: the one category that earns an
`#[allow]`, and the table of what to use instead of the ones that don't.

`rust-performance.md` shares these globs and answers a different question — how to make
safe code fast. Reach for it first. Nothing here is a performance technique.

## Why the bar is where it is

Argued in `docs/adr/`: what a soundness bug costs here, and why never for performance.
What matters at this glob is the spelling.

**`deny`, not `forbid`** — that's what makes the per-site `#[allow]` legal, and it's
deliberate. Don't "tighten" it to `forbid`; the FFI below can't be deleted, so the only
effect would be to move every allow into a `build.rs`-shaped workaround.

## The one sanctioned category: platform FFI

Every `unsafe` in production is a call into an OS the type system can't reach. There is
no other kind, and the list is short enough to keep here. **Thirty-nine calls, in thirty-eight
`unsafe` blocks, across ten files, under twenty-six `#[allow(unsafe_code)]` attributes.** Say
which of the four you mean when you quote a number, and re-derive it the same way — they differ,
and none of them is the count of rows below. (The attributes fall short of the blocks because
`dwm_titlebar.rs`'s first `#[allow]` sits on a function holding two of them,
`endpoint_volume.rs`'s `activate` on one holding three, and most of the taskbar's on several. Four
of the taskbar's blocks make no call at all, reclaiming or reading memory an OS call handed back.)

| site | what |
|---|---|
| `crates/netodia/src/main.rs` | `env::set_var` for `PIPEWIRE_ALSA` |
| `crates/netodia-platform/…/allocator.rs` | `libc::mallopt` ×3 (the glibc arena / mmap / trim knobs), `libc::malloc_trim` |
| `crates/netodia-platform/…/dwm_titlebar.rs` | `DwmSetWindowAttribute` ×3 |
| `crates/netodia-platform/…/registry.rs` | `RegGetValueW` (the DWM accent a window border takes, and the Windows app mode) |
| `crates/netodia-app/…/settings/data.rs` | `GetUserDefaultLocaleName` |
| `crates/netodia-app/…/updater/install/swap.rs` | `MoveFileExW` |
| `crates/netodia-playback/…/output/mmcss.rs` | `AvSetMmThreadCharacteristicsW` and `AvRevertMmThreadCharacteristics` (the exclusive writer's MMCSS registration, reverted on the same thread) |
| `crates/netodia-platform/…/taskbar/mod.rs` | the window subclass (`Get`/`Set`/`RemoveWindowSubclass`, `DefSubclassProc`, and reclaiming the context it hands back), `ITaskbarList3`'s thumbnail toolbar, `GetDpiForWindow` and `GetSystemMetricsForDpi` for the glyph size and the preview's, and DWM's iconic thumbnail (`DwmSetWindowAttribute`, `DwmInvalidateIconicBitmaps`, `DwmSetIconicThumbnail`) |
| `crates/netodia-platform/…/taskbar/bitmap.rs` | `CreateDIBSection` and filling the pixels it allocates, `CreateBitmap`, `CreateIconIndirect`, and the `DeleteObject`/`DestroyIcon` that release them |
| `crates/netodia-playback/…/output/endpoint_volume.rs` | `CoCreateInstance`, `IMMDeviceEnumerator::GetDevice` and `IMMDevice::Activate` (reaching the endpoint volume `wasapi` doesn't wrap), `Get`/`SetMasterVolumeLevelScalar` (the claimed device's own volume control, on the exclusive writer's thread) |

A new site outside that shape is a different *kind* of thing rather than one more of the
same, and owes a justification somewhere a reviewer will read — not only in the
attribute's `reason`.

Two non-FFI things that look like they'd need `unsafe` and don't: `Box::leak` (the
`'static` speakers in `state/mod.rs`, the interned column lists in `queries/track/columns.rs`)
is safe, and so is every `*mut c_void` that is only *stored* — `media_controls` holds an
`HWND` across time without a single `unsafe`, because souvlaki owns the deref.

## Writing one

- **`#[allow(unsafe_code, reason = "…")]` on the narrowest item that covers it** — the
  function, or the block. Not the module, and not the file unless every item in it is
  FFI. This is the documented exception to the root `CLAUDE.md`'s "`#[expect]`, never
  `allow`": `expect` is for a suppression that should stop being needed and fires
  `unfulfilled_lint_expectations` when it does. FFI unsafe is permanent, so an `expect`
  there would be correct on the day it was written and never fire again.
- **A `// SAFETY:` comment names the precondition being upheld**, never what the call
  does. `dwm_titlebar.rs`'s is the model: it says the pointer targets a stack local
  whose size matches the `cbAttribute` argument and that the API doesn't retain it —
  three claims a reviewer can check against the docs. "Calls `DwmSetWindowAttribute`"
  would be worth nothing.
- **Build raw pointers with `std::ptr::from_ref(&x).cast()`**, never
  `&x as *const _ as *const c_void`. The chained `as` silently accepts a reference that
  was never the type you meant.
- **Take the declaration from `windows-sys`** — already a dependency — never a hand-rolled
  `unsafe extern "system"` block. There are none left in the tree, so a second would be the
  first: a mistyped signature is UB the compiler will happily agree with, and it is the one
  half of an FFI site no `// SAFETY:` comment can make checkable.
  **A COM interface is the exception, and it comes from `windows`**, which is where the vtables
  are: `windows-sys` declares none. `endpoint_volume.rs` and the taskbar's `ITaskbarList3` are the two
  sites, held to the `windows` release `wasapi` and cpal already resolve, so they add no second
  copy. The taskbar takes its GDI and DWM calls from the same crate, where `windows-sys` would put
  two incompatible `HWND`s in one file.
- **`cfg`-gate at the call site *and* check the manifest.** `libc` is declared under
  `[target.'cfg(target_os = "linux")']`, so a `cfg(unix)` call site compiles on Linux
  and fails to resolve on macOS. That exact mismatch shipped once in a test.

## Test env-var mutation

`std::env::set_var` / `remove_var` are unsafe in Rust 2024 because they mutate shared
process state, and `cargo test` runs in parallel by default. There is one shape:

**lock → snapshot → clear → set → `catch_unwind(body)` → restore → `resume_unwind`**

and it is written once, in **`netodia_testkit::with_env_set`**
(`crates/netodia-testkit/src/lib.rs`).
Call it. Don't re-roll it — each of those seven steps has been missing from a hand-rolled
copy at some point, and the restore is the one that goes first.

- **The helpers are safe to call, and no test file contains `unsafe` any
  more.** That is encapsulation, not a hole: `with_env_set` is the only place in the test
  binary that mutates the environment, so "every mutation is under `ENV_LOCK`" is a
  property of that one module rather than something each caller re-argues in a `// SAFETY:`
  comment it can't actually check. The three per-variable wrappers —
  `with_env_var`, `with_appimage_env`, `settings_tests::with_locale_env` — take the
  *overrides* rather than a bare closure precisely so the mutation stays inside; a wrapper
  that hands its body a chance to `set_var` pushes the `unsafe` back out to every call site.
- **One lock per test *binary*, and the mutex behind the helpers is private.** The
  environment is process-global, so two tests holding *different* locks are still racing
  however careful each is on its own — glibc's `setenv` can realloc `environ` out from
  under another thread's `getenv`. The variables aren't independent either: the readers
  overlap through code neither caller owns, `SettingsData::default()` reaching
  `XDG_CURRENT_DESKTOP` via `host_desktop()` *and* all four locale variables via
  `default_locale()`, and `install_target()` reaching `$APPIMAGE` via
  `target::current_target_key()`. Three separate mutexes sat here (`ENV_LOCK`,
  `APPIMAGE_ENV_LOCK`, `PATH_ENV_LOCK`), each correct in isolation and collectively
  guaranteeing nothing. Consolidating assertions into one *test* doesn't help either; it
  only stops that test racing itself. Keeping the lock private is what stops the next
  copy: there is nothing to take but the helpers.
- **A reader is a different hazard from a second writer, and its opt-in is about what it
  reads.** std's contract is "no other threads concurrently writing or *reading*(!) the
  environment through functions or global variables other than the ones in this module", the
  `(!)` being theirs. `std::env::var` and `set_var` share one internal lock, so the reads
  that are undefined behaviour beside a mutator are the ones that skip it: C code, and libc
  calls such as a DNS lookup, reaching `getenv` directly. A test reading through `std::env`
  is safe beside one and can still read the value it set: four tests in `settings_tests.rs`
  built a `SettingsData::default()`, which reaches `XDG_CURRENT_DESKTOP` and all four locale
  variables through its serde defaults, beside the tests mutating both.
  **`netodia_testkit::reading_env(body)`** takes the same lock without touching a variable
  and is how such a test opts in. Nothing enforces it, so an unwrapped reader whose
  assertion turns on what it read is a flaky test, not a style nit.
- **It is not reentrant, and that is the cost of one lock.** A helper called from inside
  another helper's body would deadlock the test binary — which is why a thread-local flag
  now turns that into a named panic instead of a silent hang with no failing assertion. A
  per-variable wrapper still **delegates** rather than locking and then calling;
  `with_env_var`, `with_appimage_env`, `with_locale_env` and
  `linux_pkg_tests::with_path_env` are the four worked examples, each one line over
  `with_env_set`. A wrapper that needs its own lock is a wrapper in the wrong place.

A file-level `#![allow(unsafe_code, reason = "…")]` used to be the norm here and there are
none left anywhere, because the mutation moved behind the safe helpers.
**Delete the allow when the last `unsafe` goes**; a stale one silently pre-authorises the
next, and one sat in `library/settings/tests/folders_tests.rs` over a file with no
`unsafe` in it at all. Routing a file through the shared helper is exactly the edit that
strands one, and it has retired four (`target_tests.rs`, `system_install_tests.rs`,
`linux_pkg_tests.rs`, `settings_tests.rs`). `crates/netodia/tests/headless.rs` is the fifth and got there
the other way — `Paths::rooted_at` left it no reason to touch the environment at all. The
one that remains is on `with_env_set` itself — on the function, not the file, per the
narrowest-item rule above.

## What to reach for instead

| temptation | reach for |
|---|---|
| `get_unchecked` on a hot slice | iterator `zip` — already the idiom in `player/playback/equalizer.rs`, and the comment there says why. Or put the length in the *type* (`[T; N]` over `Box<[T]>`) so the compiler proves the mask itself |
| `transmute` between plain-data slices | `bytemuck::cast_slice`. Already in the lock file transitively, so adopting it costs a direct-dependency line rather than a build — but it still has to earn one against what it saves |
| uninit buffer + `set_len` | `Vec::with_capacity` + `extend`, or build with the fill you need. Every buffer in the DSP and visualizer paths is allocated once per source and reused, which is the win that actually mattered |
| `std::arch` SIMD | measure first, and read the shape of the work: ten cascaded biquads are *serially dependent*, so there is nothing to vectorise across bands, and two channels caps the other axis at 2× |
| `unsafe impl Send` / `Sync` | the `const _: fn() = \|\| { fn check<T: Send + Sync>() {} check::<FooUi>(); };` assertion — ten of them in the tree already |
| a raw pointer to dodge a lifetime | an owned `Arc` clone. `PlaybackContext` exists for precisely this and says so |

## Before reaching for unsafe on a hot path

The measured hot spots are not where intuition puts them, and none of them is a
bounds-check problem. Read these before proposing anything:

- **The per-sample DSP path has no index in it.** `EqSource::next_active` is four
  `zip`ped iterator loops; what survives is a handful of per-*frame* slice accesses. The
  bypass arms — a flat EQ at unity gain, which is the default — touch no buffer at all.
- **The visualizer's dominant per-frame cost was number *formatting*, not arithmetic.**
  At the column cap the trace writes two vertices per column into an SVG path string,
  and asking `core::fmt` for an exactly-rounded decimal at a fixed precision is a far
  harder question than the coordinates need — Grisu's `format_exact` with a bignum
  fallback, to print a sign, one digit and a zero-padded remainder. The fix was
  `dsp::push_fixed::<N>` — integer scale, integer print — in safe code, with the
  width a const parameter so an unrepresentable scale is a build failure rather than a
  runtime clamp. Both drawn styles write their figure through it now.
- **The backdrop solve's cost was a transcendental with a 256-value domain.** `linearized`
  takes a `u8`; three calls per pixel became three loads from a `LazyLock<[f64; 256]>`
  in `ui/backdrop.rs`.

The pattern in all three: the win came from asking a smaller question, not from removing
a check. **No `unsafe` for performance without a flamegraph or a criterion number
showing the safe version is the bottleneck** — and when you have one, the fix is usually
still safe.

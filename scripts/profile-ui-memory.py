#!/usr/bin/env python3
"""Measure the real release app in Xvfb; no Rust commands are run by this script.

Run under dbus-run-session and xvfb-run with ALSA's null device configured.
Each scenario launches a separate process against a disposable data directory.
The synthetic library exercises real migrations, SQL queries, row models and art.
It has no playable audio or cloud account, so this is an idle UI measurement.
"""

import argparse
import csv
import json
import os
from pathlib import Path
import shutil
import sqlite3
import statistics
import struct
import subprocess
import time
import zlib


TARGET_BYTES = 50_000_000
SHOW_WINDOW = '    app.show().map_err(|e| AppError::io("show window", e))?;'
# Applied only to disposable CI checkouts, equally to baseline and candidate.
# This fork resets persisted local tabs to account playlists at startup, so
# views.json alone cannot select the local Songs/Albums surfaces for measurement.
NAVIGATION_HOOK = r'''
    if let (Ok(view), Ok(ready)) = (
        std::env::var("NETODIA_MEMORY_PROFILE_VIEW"),
        std::env::var("NETODIA_MEMORY_PROFILE_READY"),
    ) {
        let library = app.global::<netodia_ui::MyLibrary>();
        let tab = if view == "songs" { library.get_tab_songs() } else { library.get_tab_albums() };
        library.set_tab_idx(tab);
        app.global::<netodia_ui::Nav>().set_selected_index(3);
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(10), move || {
            let Some(app) = weak.upgrade() else { return };
            let count = if view == "songs" {
                app.global::<netodia_ui::Tracks>().get_total_count()
            } else {
                app.global::<netodia_ui::Albums>().get_total_count()
            };
            let _ = std::fs::write(ready, count.to_string());
        });
    }
'''
SCENARIOS = (
    ("empty-search", 0, 0, False),
    ("empty-settings", 9, 0, False),
    ("library-songs", 3, 1, True),
    ("library-albums", 3, 2, True),
)


def parse_kib(text):
    """Convert named /proc kB counters to bytes without summing overlapping fields."""
    result = {}
    for line in text.splitlines():
        key, _, value = line.partition(":")
        parts = value.split()
        if len(parts) == 2 and parts[1] == "kB":
            result[key] = int(parts[0]) * 1024
    return result


def read_sample(pid):
    proc = Path(f"/proc/{pid}")
    memory = parse_kib((proc / "smaps_rollup").read_text())
    status = parse_kib((proc / "status").read_text())
    return {
        "private_footprint": memory["Anonymous"] + memory.get("Swap", 0),
        "rss": memory["Rss"],
        "pss": memory["Pss"],
        "uss": memory["Private_Clean"] + memory["Private_Dirty"],
        "peak_rss": status["VmHWM"],
    }


def check_running(process):
    if process.poll() is not None:
        raise RuntimeError(f"Netodia exited early with code {process.returncode}; see app.log")


def wait_visible(process, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        check_running(process)
        found = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--pid", str(process.pid)],
            capture_output=True, text=True, check=False,
        )
        if found.returncode == 0 and found.stdout.strip():
            return
        time.sleep(0.25)
    raise RuntimeError("Netodia did not show a window within 60 seconds")


def write_settings(root, locale, nav, tab):
    root.mkdir(parents=True, exist_ok=True)
    (root / "settings.json").write_text(json.dumps({
        "locale": locale, "window_width": 1100, "window_height": 760,
        "onboarding": {"onboarding_version": 1},
    }), encoding="utf-8")
    (root / "views.json").write_text(json.dumps({
        "last_nav_index": nav, "my_library_tab": tab, "settings_tab": 0,
    }), encoding="utf-8")


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def launch(binary, root, log):
    env = os.environ.copy()
    views = json.loads((root / "views.json").read_text())
    if views["last_nav_index"] == 3:
        env["NETODIA_MEMORY_PROFILE_VIEW"] = (
            "songs" if views["my_library_tab"] == 1 else "albums"
        )
        env["NETODIA_MEMORY_PROFILE_READY"] = str(root.parent / "mounted-count.txt")
    env.update(NETODIA_DATA_DIR=str(root), RUST_LOG="info")
    return subprocess.Popen([str(binary)], env=env, stdout=log, stderr=log)


def write_cover(path, index):
    """A deterministic RGB PNG using only the standard library."""
    def chunk(kind, data):
        crc = zlib.crc32(kind + data)
        return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", crc)

    row = b"\0" + bytes((index * 37 % 256, index * 71 % 256, index * 113 % 256)) * 256
    header = struct.pack("!2I5B", 256, 256, 8, 2, 0, 0, 0)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
                     + chunk(b"IDAT", zlib.compress(row * 256)) + chunk(b"IEND", b""))


def seed_library(binary, root, output):
    write_settings(root, "en", 0, 0)
    with (output / "bootstrap.log").open("w") as log:
        process = launch(binary, root, log)
        try:
            wait_visible(process)
        finally:
            stop(process)
    # Disabled folder avoids filesystem scans changing this reproducible fixture.
    with sqlite3.connect(root / "netodia.db") as db:
        db.execute("PRAGMA foreign_keys = ON")
        folder = db.execute(
            "INSERT INTO folders(path, is_enabled, added_at) VALUES(?, 0, ?) RETURNING id",
            (str(root / "music"), "2026-01-01T00:00:00Z"),
        ).fetchone()[0]
        artwork = root / "fixtures"
        artwork.mkdir()
        albums = []
        for index in range(100):
            cover = artwork / f"cover-{index}.png"
            write_cover(cover, index)
            name = f"Album {index:03d} 专辑"
            album = db.execute(
                "INSERT INTO albums(name, sort_name, artwork_path) VALUES(?, ?, ?) RETURNING id",
                (name, name, str(cover)),
            ).fetchone()[0]
            albums.append((album, name, str(cover)))
        db.executemany(
            """INSERT INTO tracks(file_path, file_name, title, artist, album,
               album_id, artwork_path, folder_id, duration_ms, date_added, sort_key)
               VALUES(?, ?, ?, ?, ?, ?, ?, ?, 180000, ?, ?)""",
            ((str(root / "music" / f"{i}.flac"), f"{i}.flac", f"Track {i:05d} 音乐",
              "Unknown Artist", albums[i % 100][1], albums[i % 100][0],
              albums[i % 100][2], folder, "2026-01-01T00:00:00Z", f"{i:05d}")
             for i in range(10_000)),
        )
        db.commit()
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)")


def measure(binary, root, output, settle, samples):
    with (output / "app.log").open("w") as log:
        process = launch(binary, root, log)
        try:
            wait_visible(process)
            time.sleep(settle)
            check_running(process)
            views = json.loads((root / "views.json").read_text())
            if views["last_nav_index"] == 3:
                expected = 10_000 if views["my_library_tab"] == 1 else 100
                actual = int((output / "mounted-count.txt").read_text())
                if actual != expected:
                    raise RuntimeError(f"Expected {expected} displayed items, got {actual}")
            rows = []
            for index in range(samples):
                check_running(process)
                rows.append({"sample": index, **read_sample(process.pid)})
                time.sleep(1)
            subprocess.run(
                ["import", "-window", "root", str(output / "window.png")], check=True,
            )
            with (output / "samples.csv").open("w", newline="") as file:
                writer = csv.DictWriter(file, fieldnames=rows[0].keys())
                writer.writeheader()
                writer.writerows(rows)
            return {key: statistics.median(row[key] for row in rows)
                    for key in rows[0] if key != "sample"}
        finally:
            stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare-source", type=Path,
                        help="add the identical CI-only navigation hook before building either revision")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--settle", type=int, default=20)
    parser.add_argument("--samples", type=int, default=10)
    args = parser.parse_args()
    if args.prepare_source:
        source = args.prepare_source.read_text(encoding="utf-8")
        if source.count(SHOW_WINDOW) != 1:
            parser.error("expected exactly one window-show anchor in main.rs")
        args.prepare_source.write_text(source.replace(SHOW_WINDOW, NAVIGATION_HOOK + SHOW_WINDOW),
                                       encoding="utf-8")
        return
    if args.binary is None or args.output is None:
        parser.error("--binary and --output are required for measurement")
    if args.settle < 5 or args.samples < 2:
        parser.error("allow at least 5 seconds to settle and 2 samples")
    binary = args.binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    seed = output / "library-template"
    seed_library(binary, seed, output)
    results = []
    for locale in ("en", "zh_CN"):
        for name, nav, tab, populated in SCENARIOS:
            scenario = output / f"{locale}-{name}"
            scenario.mkdir()
            root = scenario / "data"
            if populated:
                shutil.copytree(seed, root)
            write_settings(root, locale, nav, tab)
            metrics = measure(binary, root, scenario, args.settle, args.samples)
            result = {"scenario": scenario.name, **metrics}
            results.append(result)
            print(json.dumps(result), flush=True)
    (output / "results.json").write_text(json.dumps(results, indent=2))
    report = [
        "## Whole-application idle UI memory", "",
        "Real release app, visible 1100×760 window, separate process per scenario; "
        f"{args.settle}s settle and median of {args.samples} one-second samples. "
        "Library fixture: 10,000 synthetic tracks and 100 album covers; no audio playback or cloud account.", "",
        "Both revisions receive the same CI-only navigation hook to select local tabs that "
        "this fork overrides on startup. Sampling verifies 10,000 songs or 100 albums loaded. "
        "The downloadable Windows binary is built from unmodified shipping sources.", "",
        "GitHub's Xvfb uses Mesa software OpenGL. Driver/JIT allocations are included in process "
        "memory; this environment cannot certify a hardware GPU desktop's 50 MB target.", "",
        "Private footprint = anonymous + swap; RSS includes shared and executable pages. "
        "GPU device allocations are not measured. The target below is 50,000,000 bytes (47.68 MiB).", "",
        "| Scenario | Private MiB | RSS MiB | PSS MiB | USS MiB | Peak RSS MiB | Private < 50 MB |",
        "| --- | ---: | ---: | ---: | ---: | ---: | :---: |",
    ]
    for row in results:
        values = " | ".join(f"{row[key] / 1048576:.2f}" for key in
                            ("private_footprint", "rss", "pss", "uss", "peak_rss"))
        report.append(f"| {row['scenario']} | {values} | "
                      f"{'yes' if row['private_footprint'] < TARGET_BYTES else 'no'} |")
    text = "\n".join(report) + "\n"
    (output / "summary.md").write_text(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as file:
            file.write(text)


if __name__ == "__main__":
    main()

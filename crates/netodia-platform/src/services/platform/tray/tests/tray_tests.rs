//! Tests for the `TraySnapshot` rendering helpers.

use super::{TrayAction, TrayMenuLabels, TraySnapshot};

#[test]
fn tooltip_falls_back_to_brand_when_idle() {
    let snap = TraySnapshot::default();
    assert_eq!(snap.tooltip(), "Netodia");
    assert_eq!(snap.menu_track_label(), "Netodia");
}

#[test]
fn tooltip_combines_title_and_artist() {
    let snap = TraySnapshot {
        track_title: Some("Clair de Lune".to_owned()),
        track_artist: Some("Debussy".to_owned()),
        is_playing: true,
        ..Default::default()
    };
    assert_eq!(snap.tooltip(), "Clair de Lune — Debussy");
    assert_eq!(snap.menu_track_label(), "Clair de Lune");
}

#[test]
fn tooltip_uses_title_only_when_artist_missing_or_empty() {
    let no_artist = TraySnapshot {
        track_title: Some("Untitled".to_owned()),
        track_artist: None,
        is_playing: false,
        ..Default::default()
    };
    assert_eq!(no_artist.tooltip(), "Untitled");

    let empty_artist = TraySnapshot {
        track_title: Some("Untitled".to_owned()),
        track_artist: Some(String::new()),
        is_playing: false,
        ..Default::default()
    };
    assert_eq!(empty_artist.tooltip(), "Untitled");
}

#[test]
fn play_pause_label_tracks_is_playing() {
    let mut snap = TraySnapshot::default();
    assert_eq!(snap.play_pause_label(), "Play");
    snap.is_playing = true;
    assert_eq!(snap.play_pause_label(), "Pause");
}

#[test]
fn play_pause_label_uses_translated_labels() {
    let mut snap = TraySnapshot {
        labels: TrayMenuLabels {
            play: "播放".to_owned(),
            pause: "暂停".to_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(snap.play_pause_label(), "播放");
    snap.is_playing = true;
    assert_eq!(snap.play_pause_label(), "暂停");
}

#[test]
fn tray_action_is_copy() {
    // `TrayAction` is moved into menu callbacks by value; keep it `Copy`.
    let a = TrayAction::PlayPause;
    let b = a;
    assert_eq!(a, b);
}

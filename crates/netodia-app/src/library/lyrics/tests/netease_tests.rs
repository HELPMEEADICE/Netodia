use super::{attach_parallel, parse_response};
use crate::library::lyrics::lrc;
use netodia_core::entities::cloud::Lyrics as CloudLyrics;
use netodia_core::entities::lyrics::LyricsOutcome;
use netodia_core::entities::lyrics::LyricsSource;

#[test]
fn parallel_lyrics_match_by_timestamp_when_rows_are_missing() {
    let parsed =
        lrc::parse("[00:01.00]First\n[00:02.00]Second\n[00:03.00]Third", LyricsSource::Online);
    assert!(parsed.is_some(), "original lyrics should parse");
    let Some(mut original) = parsed else { return };

    attach_parallel(
        &mut original.lines,
        "[00:01.00]第一句\n[00:03.00]第三句",
        |line, value| line.translation = Some(value),
    );

    assert_eq!(original.lines[0].translation.as_deref(), Some("第一句"));
    assert_eq!(original.lines[1].translation, None);
    assert_eq!(original.lines[2].translation.as_deref(), Some("第三句"));
}

#[test]
fn untimed_parallel_lyrics_fall_back_to_row_order() {
    let parsed = lrc::parse("[00:01.00]First\n[00:02.00]Second", LyricsSource::Online);
    assert!(parsed.is_some(), "original lyrics should parse");
    let Some(mut original) = parsed else { return };

    attach_parallel(&mut original.lines, "第一句\n第二句", |line, value| {
        line.translation = Some(value);
    });

    assert_eq!(original.lines[0].translation.as_deref(), Some("第一句"));
    assert_eq!(original.lines[1].translation.as_deref(), Some("第二句"));
}

#[test]
fn netease_lyrics_keep_original_translation_and_romanization_together() {
    let response = CloudLyrics {
        original: "[00:01.00]こんにちは".to_owned(),
        translation: "[00:01.00]你好".to_owned(),
        romanization: "[00:01.00]Konnichiwa".to_owned(),
    };
    let outcome = parse_response(&response);
    assert!(matches!(&outcome, LyricsOutcome::Sheet(_)), "expected lyrics");
    let LyricsOutcome::Sheet(mut sheet) = outcome else { return };

    crate::library::lyrics::romanize::apply(&mut sheet.lines);

    assert!(sheet.is_synced());
    assert_eq!(sheet.lines[0].text, "こんにちは");
    assert_eq!(sheet.lines[0].translation.as_deref(), Some("你好"));
    assert_eq!(sheet.lines[0].romanization.as_deref(), Some("Konnichiwa"));
}

#[test]
fn translation_is_still_shown_when_original_lyrics_are_missing() {
    let response =
        CloudLyrics { translation: "[00:01.00]译文".to_owned(), ..CloudLyrics::default() };
    let outcome = parse_response(&response);
    assert!(matches!(&outcome, LyricsOutcome::Sheet(_)), "expected translated lyrics");
    let LyricsOutcome::Sheet(sheet) = outcome else { return };
    assert_eq!(sheet.lines[0].text, "译文");
}

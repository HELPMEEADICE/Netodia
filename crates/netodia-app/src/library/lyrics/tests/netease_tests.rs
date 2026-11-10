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

#[test]
fn parallel_lyrics_align_when_separators_differ_between_original_translation_and_romanization() {
    // Certain tracks mix dot tags for credits with colon tags for body lines,
    // while translation uses colons and romanization uses dots.
    let response = CloudLyrics {
        original: "[00:00.00]作词 : 作词人\n[00:00.03]作曲 : 作曲人\n[00:00:06]君のいない部屋で\n[00:04:22]思い出してしまうよ".to_owned(),
        translation: "[00:00:06]在没有你的房间里\n[00:04:22]还是会不由自主地想起".to_owned(),
        romanization: "[00:00.060]kimi no inai heya de\n[00:04.220]omoidashite shimau yo".to_owned(),
    };
    let outcome = parse_response(&response);
    assert!(matches!(&outcome, LyricsOutcome::Sheet(_)), "expected sheet");
    let LyricsOutcome::Sheet(sheet) = outcome else { return };

    assert!(sheet.is_synced());
    assert_eq!(sheet.lines.len(), 4);

    assert_eq!(sheet.lines[0].at_ms, Some(0));
    assert_eq!(sheet.lines[0].text, "作词 : 作词人");
    assert_eq!(sheet.lines[0].translation, None);
    assert_eq!(sheet.lines[0].romanization, None);

    assert_eq!(sheet.lines[1].at_ms, Some(30));
    assert_eq!(sheet.lines[1].text, "作曲 : 作曲人");
    assert_eq!(sheet.lines[1].translation, None);
    assert_eq!(sheet.lines[1].romanization, None);

    assert_eq!(sheet.lines[2].at_ms, Some(60));
    assert_eq!(sheet.lines[2].text, "君のいない部屋で");
    assert_eq!(sheet.lines[2].translation.as_deref(), Some("在没有你的房间里"));
    assert_eq!(sheet.lines[2].romanization.as_deref(), Some("kimi no inai heya de"));

    assert_eq!(sheet.lines[3].at_ms, Some(4_220));
    assert_eq!(sheet.lines[3].text, "思い出してしまうよ");
    assert_eq!(sheet.lines[3].translation.as_deref(), Some("还是会不由自主地想起"));
    assert_eq!(sheet.lines[3].romanization.as_deref(), Some("omoidashite shimau yo"));
}

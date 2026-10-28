use super::{decode_created_playlists, decode_intelligence_songs, decode_saved_playlists, ep};
use crate::netease::Result;
use crate::netease::endpoints::Protocol;
use netodia_core::entities::cloud::Quality;
use serde_json::json;

#[test]
fn all_time_ranking_retains_every_song_count_and_large_id() -> Result<()> {
    let songs = super::decode_most_played_songs(json!({
        "allData": [
            {"playCount": 100, "score": 100, "song": {"id": 4_000_000_001_i64, "name": "First", "artists": [{"id": 7, "name": "Artist"}], "album": {"id": 1, "name": "Album", "picUrl": "https://p1.music.126.net/a.jpg"}, "duration": 210000}},
            {"playCount": 42, "score": 42, "song": {"id": 4_000_000_002_i64, "name": "Second"}},
            {"playCount": 1, "score": 1, "song": {"id": 7, "name": "Third"}}
        ],
        "weekData": [{"playCount": 5, "song": {"id": 99, "name": "Weekly"}}]
    }))?;

    assert_eq!(
        songs.iter().map(|entry| entry.song.id).collect::<Vec<_>>(),
        [4_000_000_001, 4_000_000_002, 7]
    );
    assert_eq!(songs.iter().map(|entry| entry.play_count).collect::<Vec<_>>(), [100, 42, 1]);
    assert_eq!(songs[0].song.artist_names(), "Artist");
    assert_eq!(songs[0].song.duration_ms, 210_000);
    assert_eq!(songs[0].song.album.cover_url.as_deref(), Some("https://p1.music.126.net/a.jpg"));
    Ok(())
}

#[test]
fn empty_ranking_and_invalid_responses_are_distinct() -> Result<()> {
    assert!(super::decode_most_played_songs(json!({"allData": []}))?.is_empty());
    assert!(super::decode_most_played_songs(json!({"weekData": []})).is_err());
    assert!(super::decode_most_played_songs(json!({"allData": null})).is_err());
    assert!(
        super::decode_most_played_songs(
            json!({"allData": [{"song": {"id": 42, "name": "Missing count"}}]})
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn listening_ranking_uses_the_native_weapi_route() -> Result<()> {
    let client = super::Client::new(reqwest::Client::new());
    let request = client
        .prepare_request(
            ep::USER_RECORD,
            json!({"uid":42,"type":0}),
            &std::collections::BTreeMap::new(),
        )
        .await?
        .build()
        .map_err(|_| crate::netease::Error::InvalidResponse)?;
    assert_eq!(request.url().as_str(), "https://music.163.com/weapi/v1/play/record");
    assert_eq!(ep::USER_RECORD.protocol, Protocol::Weapi);
    const { assert!(ep::USER_RECORD.read_only) };
    Ok(())
}

#[tokio::test]
async fn listening_uses_clientlog_eapi_with_the_desktop_reporting_profile() -> Result<()> {
    let client = super::Client::new(reqwest::Client::new());
    let cookies =
        std::collections::BTreeMap::from([("MUSIC_U".to_owned(), "test-token".to_owned())]);
    let request = client
        .prepare_request(ep::WEBLOG, super::listen_log("play", 42, 123)?, &cookies)
        .await?
        .build()
        .map_err(|_| crate::netease::Error::InvalidResponse)?;
    assert_eq!(request.url().as_str(), "https://clientlog.music.163.com/eapi/feedback/weblog");
    assert_eq!(ep::WEBLOG.protocol, Protocol::Eapi);
    const { assert!(!ep::WEBLOG.read_only) };
    let (_, header, _) = client.request_context(ep::WEBLOG, &cookies, "csrf", 1000);
    assert_eq!(header["os"], "osx");
    assert_eq!(header["MUSIC_U"], "test-token");
    assert_eq!(header["__csrf"], "csrf");
    assert_eq!(header["deviceId"], client.device_id());
    Ok(())
}

#[test]
fn listening_logs_pair_song_identity_with_actual_seconds() -> Result<()> {
    for action in ["startplay", "play"] {
        let body = super::listen_log(action, 42, 123)?;
        let logs: serde_json::Value = serde_json::from_str(
            body["logs"].as_str().ok_or(crate::netease::Error::InvalidResponse)?,
        )
        .map_err(|_| crate::netease::Error::InvalidResponse)?;
        assert_eq!(logs[0]["action"], action);
        assert_eq!(logs[0]["json"]["id"], "42");
        assert_eq!(logs[0]["json"]["type"], "song");
        assert_eq!(logs[0]["json"]["content"], "id=0");
        if action == "play" {
            assert_eq!(logs[0]["json"]["time"], 123);
            assert_eq!(logs[0]["json"]["end"], "playend");
        } else {
            assert!(logs[0]["json"]["time"].is_null());
        }
    }
    Ok(())
}

#[tokio::test]
async fn listening_never_sends_guest_or_stale_session_logs() {
    let client = super::Client::new(reqwest::Client::new());
    assert!(matches!(
        client.report_listen(42, 60).await,
        Err(crate::netease::Error::Authentication)
    ));
    let generation = client.generation();
    client.clear_session().await;
    assert!(matches!(
        client.request_in_generation(ep::WEBLOG, json!({}), generation).await,
        Err(crate::netease::Error::Cancelled)
    ));
}

#[test]
fn stream_quality_uses_yesplaymusic_bitrates() {
    assert_eq!(ep::STREAM.path, "/api/song/enhance/player/url");
    assert_eq!(ep::STREAM.protocol, Protocol::Weapi);
    for (quality, bitrate) in [
        (Quality::Standard, 128_000),
        (Quality::Higher, 192_000),
        (Quality::Exhigh, 320_000),
        (Quality::Lossless, 350_000),
        (Quality::Hires, 999_000),
    ] {
        assert_eq!(quality.bitrate(), bitrate);
    }
}

#[test]
fn created_and_collected_pages_partition_account_playlists() -> Result<()> {
    let body = json!({"more": true, "playlist": [
        {"id": 3_136_952_023_i64, "name": "Created", "creator": {"userId": 55}},
        {"id": 4_000_000_000_i64, "name": "Collected", "creator": {"userId": 77}},
        {"id": 12, "name": "Unknown owner"}
    ]});
    let (created, created_more) = decode_created_playlists(&body, 55)?;
    let (collected, collected_more) = decode_saved_playlists(&body, 55)?;

    assert_eq!(created.iter().map(|playlist| playlist.id).collect::<Vec<_>>(), [3_136_952_023]);
    assert_eq!(
        collected.iter().map(|playlist| playlist.id).collect::<Vec<_>>(),
        [4_000_000_000, 12]
    );
    assert!(created_more);
    assert!(collected_more);
    Ok(())
}

#[test]
fn an_empty_filtered_page_does_not_hide_later_created_playlists() -> Result<()> {
    let body = json!({"more": true, "playlist": [{"id": 11, "name": "Collected", "creator": {"userId": 77}}]});
    let (created, more) = decode_created_playlists(&body, 55)?;
    assert!(created.is_empty());
    assert!(more);
    Ok(())
}

#[test]
fn intelligence_decodes_nested_songs_in_server_order() -> Result<()> {
    let songs = decode_intelligence_songs(&json!({"data": [
        {"id": 3_000_000_000_i64, "songInfo": {"id": 3_000_000_000_i64, "name": "First", "artists": [{"id": 8, "name": "Artist"}]}},
        {"id": 7, "songInfo": {"id": 7, "name": "Second"}}
    ]}))?;
    assert_eq!(songs.iter().map(|song| song.id).collect::<Vec<_>>(), [3_000_000_000, 7]);
    assert_eq!(songs[0].artist_names(), "Artist");
    Ok(())
}

#[test]
fn intelligence_rejects_an_entry_without_song_info() {
    assert!(decode_intelligence_songs(&json!({"data": [{"id": 7}]})).is_err());
}

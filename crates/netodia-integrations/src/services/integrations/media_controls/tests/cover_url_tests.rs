use std::path::{MAIN_SEPARATOR_STR, Path};

use super::cover_url;

#[test]
fn remote_covers_wait_for_the_apps_capped_download() {
    assert_eq!(cover_url("https://example.test/cover.jpg"), None);
    assert_eq!(cover_url("http://example.test/cover.jpg"), None);
    assert_eq!(cover_url("HTTPS://example.test/cover.jpg"), None);
}

#[test]
fn uris_are_not_published_as_local_files() {
    assert_eq!(cover_url("http://"), None);
    assert_eq!(cover_url("https://"), None);
    assert_eq!(cover_url("file:///cover.jpg"), None);
    assert_eq!(cover_url("data:image/png;base64,AAAA"), None);
    assert_eq!(cover_url("ftp://example.test/cover.jpg"), None);
}

#[test]
fn a_local_windows_cover_uses_the_separator_winrt_accepts() {
    let root = format!("C:{MAIN_SEPARATOR_STR}");
    let path = Path::new(&root).join("artwork").join("cover.jpg");
    let path = path.to_string_lossy();
    let forward_slashes = path.replace(MAIN_SEPARATOR_STR, "/");
    let expected = Some(format!("file://{path}"));

    assert_eq!(cover_url(&path), expected);
    assert_eq!(cover_url(&forward_slashes), expected);
}

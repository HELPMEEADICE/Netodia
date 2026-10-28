//! The wrapper routes in `YesPlayMusic` are not upstream URLs. This table records the wire paths.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Weapi,
    Eapi,
    Xeapi,
}

#[derive(Clone, Copy, Debug)]
pub struct Endpoint<'a> {
    pub path: &'a str,
    pub protocol: Protocol,
    pub read_only: bool,
}

impl<'a> Endpoint<'a> {
    pub const fn read(path: &'a str, protocol: Protocol) -> Self {
        Self { path, protocol, read_only: true }
    }

    pub const fn write(path: &'a str, protocol: Protocol) -> Self {
        Self { path, protocol, read_only: false }
    }
}

use Protocol::{Eapi, Weapi, Xeapi};

pub const QR_KEY: Endpoint<'static> = Endpoint::read("/api/login/qrcode/unikey", Eapi);
pub const QR_CHECK: Endpoint<'static> = Endpoint::read("/api/login/qrcode/client/login", Eapi);
pub const REGISTER_ANONYMOUS: Endpoint<'static> = Endpoint::write("/api/register/anonimous", Xeapi);
pub const PHONE_LOGIN: Endpoint<'static> = Endpoint::write("/api/w/login/cellphone", Weapi);
pub const EMAIL_LOGIN: Endpoint<'static> = Endpoint::write("/api/w/login", Eapi);
pub const ACCOUNT: Endpoint<'static> = Endpoint::read("/api/nuser/account/get", Weapi);
pub const REFRESH: Endpoint<'static> = Endpoint::write("/api/login/token/refresh", Weapi);
pub const LOGOUT: Endpoint<'static> = Endpoint::write("/api/logout", Weapi);
pub const DISCOVER: Endpoint<'static> = Endpoint::read("/api/personalized/playlist", Weapi);
pub const TOPLIST: Endpoint<'static> = Endpoint::read("/api/toplist", Eapi);
pub const SEARCH: Endpoint<'static> = Endpoint::read("/api/cloudsearch/pc", Eapi);
pub const PLAYLIST: Endpoint<'static> = Endpoint::read("/api/v6/playlist/detail", Eapi);
pub const SONGS: Endpoint<'static> = Endpoint::read("/api/v3/song/detail", Weapi);
pub const STREAM: Endpoint<'static> = Endpoint::read("/api/song/enhance/player/url", Weapi);
pub const LYRIC: Endpoint<'static> = Endpoint::read("/api/song/lyric", Eapi);
pub const DAILY: Endpoint<'static> = Endpoint::read("/api/v3/discovery/recommend/songs", Weapi);
pub const RECOMMENDATIONS: Endpoint<'static> =
    Endpoint::read("/api/v1/discovery/recommend/resource", Weapi);
pub const INTELLIGENCE: Endpoint<'static> =
    Endpoint::read("/api/playmode/intelligence/list", Weapi);
pub const SIMILAR_SONGS: Endpoint<'static> = Endpoint::read("/api/v1/discovery/simiSong", Weapi);
pub const FM: Endpoint<'static> = Endpoint::read("/api/v1/radio/get", Weapi);
pub const FM_TRASH: Endpoint<'static> = Endpoint::write("/api/radio/trash/add", Weapi);
pub const USER_PLAYLISTS: Endpoint<'static> = Endpoint::read("/api/user/playlist", Weapi);
pub const LIKED: Endpoint<'static> = Endpoint::read("/api/song/like/get", Weapi);
pub const RECENT_SONGS: Endpoint<'static> = Endpoint::read("/api/play-record/song/list", Weapi);
pub const USER_RECORD: Endpoint<'static> = Endpoint::read("/api/v1/play/record", Weapi);
pub const WEBLOG: Endpoint<'static> = Endpoint::write("/api/feedback/weblog", Eapi);
pub const LIKE: Endpoint<'static> = Endpoint::write("/api/radio/like", Weapi);
pub const PLAYLIST_SUBSCRIBE: Endpoint<'static> = Endpoint::write("/api/playlist/subscribe", Weapi);
pub const ALBUMS: Endpoint<'static> = Endpoint::read("/api/album/sublist", Weapi);
pub const ARTISTS: Endpoint<'static> = Endpoint::read("/api/artist/sublist", Weapi);
pub const ALBUM_UNSUBSCRIBE: Endpoint<'static> = Endpoint::write("/api/album/unsub", Weapi);
pub const ALBUM_SUBSCRIBE: Endpoint<'static> = Endpoint::write("/api/album/sub", Weapi);
pub const ARTIST_SUBSCRIBE: Endpoint<'static> = Endpoint::write("/api/artist/sub", Weapi);

/// Collection writes use separate upstream routes, rather than a `t` flag on the subscribe route.
pub const fn album_subscription(saved: bool) -> Endpoint<'static> {
    if saved { ALBUM_SUBSCRIBE } else { ALBUM_UNSUBSCRIBE }
}

//! Keep the Windows thumbnail toolbar and cover preview attached to the current Slint window.
//!
//! **The cover is decoded here rather than borrowed from Now Playing**, whose high-res tier exists
//! only while a surface draws it. This one is small, one per song, and kept for the song's length.
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use image::{DynamicImage, RgbImage};
use netodia_app::state::AppState;
use netodia_artwork::media::image::image_decode::{
    FilterType, MAX_SOURCE_DIM, decode_capped_to, decode_memory_capped_to, fit_within,
    large_decode_guard, memory_dimensions, resize_rgb8, resize_rgb8_image,
};
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{EventSink, PlayerEvent};
use netodia_engine::player::engine::now_playing::SourceId;
use netodia_engine::player::engine::state::PlayerViewModelLight;
use netodia_platform::services::platform::taskbar::{
    self, Action, Cover, CoverPixels, Snapshot, Thumbnail,
};
use netodia_ui::AppWindow;
use slint::{ComponentHandle, Weak};

use super::event_sink::SlintEventSink;
use crate::ui::remote_covers;

/// The long edge the cover is kept at. Above what DWM asks for at 150 % scaling, where the
/// thumbnail's height tops out; a larger request gets this, slightly soft.
const COVER_EDGE: u32 = 256;

type Callback = Arc<dyn Fn(Action) + Send + Sync>;

/// The song a cover belongs to, by the two things that change what it shows.
#[derive(Clone, PartialEq, Eq)]
struct CoverSource {
    track_id: i64,
    artwork_path: Option<String>,
}

struct Bridge {
    weak: Weak<AppWindow>,
    state: AppState,
    callback: Callback,
    cover: RefCell<Option<Cover>>,
    /// The song the cover above was decoded for, or is being decoded for.
    cover_source: RefCell<Option<CoverSource>>,
    /// Bumped per song, so a decode that a later song overtook drops its result.
    cover_generation: Cell<u64>,
}

pub fn install(ui: &AppWindow, state: &AppState) {
    let sink = Arc::new(SlintEventSink { state: state.clone() });
    let callback: Callback = Arc::new(move |action| {
        sink.handle(match action {
            Action::Previous => PlayerEvent::Previous,
            Action::PlayPause => PlayerEvent::PlayPause,
            Action::Next => PlayerEvent::Next,
        });
    });
    let bridge = Rc::new(Bridge {
        weak: ui.as_weak(),
        state: state.clone(),
        callback,
        cover: RefCell::new(None),
        cover_source: RefCell::new(None),
        cover_generation: Cell::new(0),
    });
    let subscriber = bridge.clone();
    let mut rx = state.sinks.view_model.subscribe();
    if let Err(e) = slint::spawn_local(async_compat::Compat::new(async move {
        loop {
            // Prime from the current value too: subscribing marks it as already seen.
            let (snapshot, source, track) = {
                let vm = rx.borrow_and_update();
                let vm = vm.as_ref();
                (snapshot(vm), cover_source(vm), vm.and_then(|vm| vm.current_track.clone()))
            };
            subscriber.follow_song(source, track);
            if !subscriber.update(snapshot) || rx.changed().await.is_err() {
                break;
            }
        }
    })) {
        log::warn!("taskbar: state subscriber failed to spawn: {}", describe(&e));
    }
    // A window can be recreated without a playback change, and Explorer can reject an update.
    tick(bridge);
}

fn snapshot(vm: Option<&PlayerViewModelLight>) -> Snapshot {
    vm.map_or_else(Snapshot::default, |vm| Snapshot {
        playing: vm.status == "playing",
        has_previous: vm.has_previous,
        has_next: vm.has_next,
    })
}

fn cover_source(vm: Option<&PlayerViewModelLight>) -> Option<CoverSource> {
    let source = vm?.source()?;
    let SourceId::Track(track_id) = source.id;
    Some(CoverSource { track_id, artwork_path: source.artwork_path.map(str::to_owned) })
}

impl Bridge {
    /// Pushes the toolbar and the current cover to the window. `false` once the window is gone.
    fn update(&self, transport: Snapshot) -> bool {
        let Some(ui) = self.weak.upgrade() else { return false };
        if let Some(hwnd) = crate::ui::window_chrome::win32_hwnd(&ui) {
            let thumbnail = Thumbnail { transport, cover: self.cover.borrow().clone() };
            taskbar::update(hwnd, &thumbnail, self.callback.clone());
        }
        true
    }

    /// Starts decoding the cover for a new song.
    ///
    /// **The old cover stays up until the load settles**, and is replaced then even by nothing.
    /// Dropped at the change instead, DWM shows its placeholder for as long as the decode or the
    /// download takes, which on a cloud song is long enough to see on every skip.
    fn follow_song(self: &Rc<Self>, source: Option<CoverSource>, track: Option<Arc<TrackSummary>>) {
        if *self.cover_source.borrow() == source {
            return;
        }
        self.cover_source.replace(source.clone());
        let generation = self.cover_generation.get().wrapping_add(1);
        self.cover_generation.set(generation);
        let Some(source) = source else {
            self.cover.replace(None);
            return;
        };

        let bridge = self.clone();
        let load = async move {
            let image = load_cover(&bridge.state, source.artwork_path, track).await;
            if bridge.cover_generation.get() != generation {
                return;
            }
            bridge.cover.replace(image.map(cover_from));
            let transport = snapshot(bridge.state.sinks.view_model.borrow().as_ref());
            bridge.update(transport);
        };
        if let Err(e) = slint::spawn_local(async_compat::Compat::new(load)) {
            log::warn!("taskbar: cover load failed to spawn: {}", describe(&e));
        }
    }
}

fn tick(bridge: Rc<Bridge>) {
    slint::Timer::single_shot(Duration::from_millis(500), move || {
        let transport = snapshot(bridge.state.sinks.view_model.borrow().as_ref());
        if bridge.update(transport) {
            tick(bridge);
        }
    });
}

/// The song's own artwork first, then the album looked up online, the order Now Playing takes.
async fn load_cover(
    state: &AppState,
    artwork_path: Option<String>,
    track: Option<Arc<TrackSummary>>,
) -> Option<RgbImage> {
    if let Some(path) = artwork_path
        && let Some(image) = decode(state, path).await
    {
        return Some(image);
    }
    let track = track?;
    let (artist, album) = (track.artist.as_deref()?, track.album.as_deref()?);
    if artist.trim().is_empty() || album.trim().is_empty() {
        return None;
    }
    let url = state.track_artwork.resolve(state.http_client(), artist, album).await?;
    decode(state, url).await
}

async fn decode(state: &AppState, source: String) -> Option<RgbImage> {
    let decoded = if remote_covers::is_remote_artwork_url(&source) {
        let bytes = remote_covers::fetch_image_bytes(state.http_client(), &source).await?;
        state.runtime.spawn_blocking(move || decode_memory(&bytes)).await
    } else {
        state.runtime.spawn_blocking(move || decode_file(Path::new(&source))).await
    };
    decoded.unwrap_or_else(|e| {
        log::warn!("taskbar: cover decode task join: {}", describe(&e));
        None
    })
}

fn decode_file(path: &Path) -> Option<RgbImage> {
    shrink(&decode_capped_to(path, MAX_SOURCE_DIM, COVER_EDGE).ok()?)
}

fn decode_memory(bytes: &[u8]) -> Option<RgbImage> {
    let _oversized = memory_dimensions(bytes, MAX_SOURCE_DIM)
        .map(|(width, height)| u64::from(width) * u64::from(height))
        .and_then(large_decode_guard);
    shrink(&decode_memory_capped_to(bytes, MAX_SOURCE_DIM, COVER_EDGE)?)
}

fn shrink(decoded: &DynamicImage) -> Option<RgbImage> {
    let (width, height) = fit_within(decoded.width(), decoded.height(), COVER_EDGE, COVER_EDGE);
    resize_rgb8(decoded, width, height, FilterType::Lanczos3)
}

fn cover_from(image: RgbImage) -> Cover {
    Rc::new(move |max_width, max_height| fit_cover(&image, max_width, max_height))
}

fn fit_cover(image: &RgbImage, max_width: u32, max_height: u32) -> Option<CoverPixels> {
    let (width, height) = fit_within(image.width(), image.height(), max_width, max_height);
    let fitted = resize_rgb8_image(Cow::Borrowed(image), width, height, FilterType::Lanczos3)?;
    let bgra = fitted.pixels().flat_map(|pixel| {
        let [red, green, blue] = pixel.0;
        [blue, green, red, u8::MAX]
    });
    Some(CoverPixels { width, height, bgra: bgra.collect() })
}

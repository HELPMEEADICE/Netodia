//! Windows taskbar thumbnail: the transport buttons under it and the cover it previews.
//!
//! **The preview is the current song's cover or the shell's own, never a capture of ours.** With a
//! cover the window asks DWM for an iconic representation and supplies the bitmap; without one it
//! hands the thumbnail back to DWM's live view of the window.

mod bitmap;
mod glyphs;

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;

use netodia_core::error::describe;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_FORCE_ICONIC_REPRESENTATION, DWMWA_HAS_ICONIC_BITMAP, DWMWINDOWATTRIBUTE,
    DwmInvalidateIconicBitmaps, DwmSetIconicThumbnail, DwmSetWindowAttribute,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass,
    THB_FLAGS, THB_ICON, THB_TOOLTIP, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THUMBBUTTON,
    TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HICON, RegisterWindowMessageW, SM_CXSMICON, WM_COMMAND, WM_DPICHANGED,
    WM_DWMSENDICONICTHUMBNAIL, WM_NCDESTROY, WM_SETTINGCHANGE,
};
use windows::core::{BOOL, w};

use self::bitmap::Icon;
use self::glyphs::Glyph;
use super::color_mode;

const SUBCLASS_ID: usize = 0x4e54;
const PREVIOUS: u32 = 1;
const PLAY_PAUSE: u32 = 2;
const NEXT: u32 = 3;

/// The small-icon size at 96 DPI, for a window whose DPI won't read.
const FALLBACK_GLYPH_SIZE: u32 = 16;
/// The preview's square edge at 96 DPI, scaled by the window's. Measured off DWM's own request,
/// 250×135 at 125 %: a square cover fills that height, and knowing it up front lets the first
/// hover lay the flyout out around the cover rather than around the request's full width.
const PREVIEW_EDGE_AT_96_DPI: u32 = 108;
const BASE_DPI: u32 = 96;
/// Glyph inks, matching the shell's own text on each taskbar mode.
const INK_ON_DARK: [u8; 3] = [0xff, 0xff, 0xff];
const INK_ON_LIGHT: [u8; 3] = [0x1a, 0x1a, 0x1a];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub playing: bool,
    pub has_previous: bool,
    pub has_next: bool,
}

#[derive(Clone, Copy)]
pub enum Action {
    Previous,
    PlayPause,
    Next,
}

/// Opaque top-down BGRA rows for the preview.
pub struct CoverPixels {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Draws the current song's cover to fit inside the maximum width and height DWM asks for.
///
/// A closure rather than pixels because the size is DWM's to pick, per request, and resampling
/// is a question for the crate that owns the resampler.
pub type Cover = Rc<dyn Fn(u32, u32) -> Option<CoverPixels>>;

/// What the taskbar should show for the window.
pub struct Thumbnail {
    pub transport: Snapshot,
    pub cover: Option<Cover>,
}

type Callback = Arc<dyn Fn(Action) + Send + Sync>;
type Shared = Rc<RefCell<State>>;

/// What the glyphs were drawn for: the size the shell draws them at and the taskbar's mode.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GlyphStyle {
    size: u32,
    light: bool,
}

struct GlyphIcons {
    style: GlyphStyle,
    previous: Icon,
    play: Icon,
    pause: Icon,
    next: Icon,
}

struct State {
    taskbar: ITaskbarList3,
    glyphs: Option<GlyphIcons>,
    callback: Callback,
    created_message: u32,
    snapshot: Snapshot,
    applied_snapshot: Option<Snapshot>,
    added: bool,
    cover: Option<Cover>,
    /// The largest thumbnail DWM last asked for, so a new cover can go straight in at it.
    requested_size: Option<(u32, u32)>,
}

/// Install or update the toolbar and the preview. Must be called on the window's UI thread with a
/// live HWND. Repeated calls also retry if Explorer was not ready when the window was first shown.
#[allow(unsafe_code, reason = "install the Win32 window subclass")]
pub fn update(hwnd: *mut c_void, thumbnail: &Thumbnail, callback: Callback) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = HWND(hwnd);
    let mut data = 0usize;
    // SAFETY: the caller supplies a live window on its owning thread. A successful lookup
    // returns the Box<Rc<...>> registered below, kept alive until WM_NCDESTROY.
    let existing =
        unsafe { GetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, Some(&raw mut data)) };
    let state = if existing.as_bool() {
        // SAFETY: the successful lookup returned the live context installed on this UI thread.
        unsafe { (*(data as *const Shared)).clone() }
    } else {
        let Some(state) = create_state(callback) else { return };
        let data = Box::into_raw(Box::new(state.clone()));
        // SAFETY: data is owned by the subclass until WM_NCDESTROY, or reclaimed on failure.
        if !unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, data as usize) }
            .as_bool()
        {
            // SAFETY: registration failed, so the callback did not take ownership of the Box.
            unsafe {
                drop(Box::from_raw(data));
            }
            log::warn!("taskbar: could not subclass the window");
            return;
        }
        state
    };
    let (buttons_changed, had_cover) = {
        let mut s = state.borrow_mut();
        s.snapshot = thumbnail.transport;
        let had_cover = (!same_cover(s.cover.as_ref(), thumbnail.cover.as_ref())).then(|| {
            let had_cover = s.cover.is_some();
            s.cover.clone_from(&thumbnail.cover);
            had_cover
        });
        (s.applied_snapshot != Some(thumbnail.transport) || !s.added, had_cover)
    };
    if buttons_changed {
        refresh(hwnd, &state);
    }
    if let Some(had_cover) = had_cover {
        show_new_cover(hwnd, &state, had_cover);
    }
}

/// Puts a changed cover in front of DWM.
fn show_new_cover(hwnd: HWND, state: &Shared, had_cover: bool) {
    if state.borrow().cover.is_none() {
        set_iconic(hwnd, false);
        return;
    }
    if !had_cover {
        set_iconic(hwnd, true);
    }
    push_cover(hwnd, state);
}

/// Hands DWM the current cover without waiting to be asked, which it does once per session.
///
/// **Pushed rather than invalidated.** An invalidation drops the bitmap DWM holds and paints its
/// placeholder until the request it triggers comes back, long enough to see on a song change;
/// and before the first request DWM knows no size, so it lays the flyout out at the request's full
/// width and leaves the cover beside a gap. A push DWM refuses, a guess over its limit, falls back
/// to the request.
fn push_cover(hwnd: HWND, state: &Shared) {
    let (cover, requested_size) = {
        let s = state.borrow();
        (s.cover.clone(), s.requested_size)
    };
    let Some(cover) = cover else { return };
    let (max_width, max_height) = requested_size.unwrap_or_else(|| preview_size(hwnd));
    if !send_cover(hwnd, &cover, max_width, max_height) {
        invalidate_cover(hwnd);
    }
}

#[allow(unsafe_code, reason = "read the window's DPI")]
fn preview_size(hwnd: HWND) -> (u32, u32) {
    // SAFETY: a read on the live window the preview belongs to; 0 means it won't answer.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => BASE_DPI,
        dpi => dpi,
    };
    let edge = (PREVIEW_EDGE_AT_96_DPI * dpi).div_ceil(BASE_DPI);
    (edge, edge)
}

fn same_cover(a: Option<&Cover>, b: Option<&Cover>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (a, b) => a.is_none() && b.is_none(),
    }
}

#[allow(unsafe_code, reason = "initialize the Windows taskbar COM interface")]
fn create_state(callback: Callback) -> Option<Shared> {
    // SAFETY: the UI thread's window backend initializes COM; the CLSID and interface are paired.
    let taskbar: ITaskbarList3 =
        match unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) } {
            Ok(value) => value,
            Err(e) => {
                log::warn!("taskbar: COM initialization failed: {}", describe(&e));
                return None;
            }
        };
    // SAFETY: the interface was created on this COM-initialized thread.
    if let Err(e) = unsafe { taskbar.HrInit() } {
        log::warn!("taskbar: initialization failed: {}", describe(&e));
        return None;
    }
    // SAFETY: the constant is a terminated UTF-16 string.
    let created_message = unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
    Some(Rc::new(RefCell::new(State {
        taskbar,
        glyphs: None,
        callback,
        created_message,
        snapshot: Snapshot::default(),
        applied_snapshot: None,
        added: false,
        cover: None,
        requested_size: None,
    })))
}

#[allow(unsafe_code, reason = "read the window's DPI and the small-icon size at it")]
fn glyph_style(hwnd: HWND) -> GlyphStyle {
    // SAFETY: both calls only read; `hwnd` is the live window the toolbar belongs to.
    let size = unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 { 0 } else { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }
    };
    GlyphStyle {
        size: u32::try_from(size).ok().filter(|&size| size > 0).unwrap_or(FALLBACK_GLYPH_SIZE),
        light: color_mode::system_theme() == "light",
    }
}

fn draw_glyphs(style: GlyphStyle) -> Option<GlyphIcons> {
    let ink = if style.light { INK_ON_LIGHT } else { INK_ON_DARK };
    let draw = |glyph| bitmap::icon(style.size, &glyphs::render(glyph, style.size, ink));
    Some(GlyphIcons {
        style,
        previous: draw(Glyph::Previous)?,
        play: draw(Glyph::Play)?,
        pause: draw(Glyph::Pause)?,
        next: draw(Glyph::Next)?,
    })
}

fn button(id: u32, icon: HICON, label: &str, enabled: bool) -> THUMBBUTTON {
    let mut button = THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        hIcon: icon,
        dwFlags: if enabled { THBF_ENABLED } else { THBF_DISABLED },
        ..Default::default()
    };
    for (slot, ch) in button.szTip.iter_mut().zip(label.encode_utf16()) {
        *slot = ch;
    }
    button
}

#[allow(unsafe_code, reason = "update the Windows taskbar COM interface")]
fn refresh(hwnd: HWND, state: &Shared) {
    let style = glyph_style(hwnd);
    // Release the borrow before calling the shell, which may dispatch window messages.
    let (taskbar, buttons, snapshot, added) = {
        let mut s = state.borrow_mut();
        if s.glyphs.as_ref().is_none_or(|glyphs| glyphs.style != style) {
            // The icons a button holds are copied by the shell, so the old set may go now.
            s.glyphs = draw_glyphs(style);
        }
        let Some(glyphs) = s.glyphs.as_ref() else { return };
        let p = s.snapshot;
        (
            s.taskbar.clone(),
            [
                button(PREVIOUS, glyphs.previous.0, "Previous", p.has_previous),
                button(
                    PLAY_PAUSE,
                    if p.playing { glyphs.pause.0 } else { glyphs.play.0 },
                    if p.playing { "Pause" } else { "Play" },
                    true,
                ),
                button(NEXT, glyphs.next.0, "Next", p.has_next),
            ],
            p,
            s.added,
        )
    };
    // SAFETY: all calls run on the UI thread, with a live HWND and icons held by state.
    let result = unsafe {
        if added {
            taskbar.ThumbBarUpdateButtons(hwnd, &buttons)
        } else {
            taskbar
                .ThumbBarAddButtons(hwnd, &buttons)
                .or_else(|_| taskbar.ThumbBarUpdateButtons(hwnd, &buttons))
        }
    };
    let mut s = state.borrow_mut();
    if result.is_ok() {
        s.added = true;
        // The shell may dispatch a newer update while this call is in flight.
        s.applied_snapshot = Some(snapshot);
    } else {
        // An update failure doesn't remove the toolbar; retry updating the existing buttons.
        s.applied_snapshot = None;
    }
}

/// Redraws the glyphs if the taskbar's mode or the window's DPI moved under them.
fn restyle(hwnd: HWND, state: &Shared) {
    let stale =
        state.borrow().glyphs.as_ref().is_some_and(|glyphs| glyphs.style != glyph_style(hwnd));
    if stale {
        state.borrow_mut().applied_snapshot = None;
        refresh(hwnd, state);
    }
}

/// Switches the thumbnail between the cover we supply and DWM's live view.
#[allow(unsafe_code, reason = "set the window's DWM thumbnail attributes")]
fn set_iconic(hwnd: HWND, iconic: bool) {
    let set = |attribute: DWMWINDOWATTRIBUTE| {
        let value = BOOL::from(iconic);
        // SAFETY: `value` is a stack-local BOOL and the size passed is its own; DWM retains no
        // pointer past the call.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                std::ptr::from_ref(&value).cast(),
                u32::try_from(size_of::<BOOL>()).unwrap_or(4),
            )
        }
    };
    let result = set(DWMWA_FORCE_ICONIC_REPRESENTATION).and_then(|()| set(DWMWA_HAS_ICONIC_BITMAP));
    if let Err(e) = result {
        log::warn!("taskbar: could not switch the thumbnail preview: {}", describe(&e));
    }
}

/// Drops whatever bitmap DWM holds, so its next request fetches the cover now current.
#[allow(unsafe_code, reason = "invalidate the window's DWM iconic bitmaps")]
fn invalidate_cover(hwnd: HWND) {
    // SAFETY: `hwnd` is the live window the thumbnail belongs to.
    if let Err(e) = unsafe { DwmInvalidateIconicBitmaps(hwnd) } {
        log::warn!("taskbar: could not refresh the thumbnail: {}", describe(&e));
    }
}

/// Hands DWM the cover drawn to fit `max_width` × `max_height`. `false` if it never got there.
#[allow(unsafe_code, reason = "hand DWM the cover bitmap for the thumbnail")]
fn send_cover(hwnd: HWND, cover: &Cover, max_width: u32, max_height: u32) -> bool {
    let Some(pixels) = cover(max_width, max_height) else { return false };
    if pixels.width > max_width || pixels.height > max_height {
        log::warn!("taskbar: a cover larger than {max_width}x{max_height} was refused");
        return false;
    }
    let Some(bitmap) = bitmap::dib_section(pixels.width, pixels.height, &pixels.bgra) else {
        return false;
    };
    // SAFETY: the bitmap is a live 32-bit DIB section within the size DWM asked for, and DWM
    // copies it, so it may be deleted as soon as the call returns.
    match unsafe { DwmSetIconicThumbnail(hwnd, bitmap.0, 0) } {
        Ok(()) => true,
        Err(e) => {
            log::warn!("taskbar: could not set the thumbnail: {}", describe(&e));
            false
        }
    }
}

#[allow(unsafe_code, reason = "handle the Win32 subclass callback and its owned context")]
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: SetWindowSubclass owns this Box until WM_NCDESTROY; clone before calling out.
    let state = unsafe { (*(data as *const Shared)).clone() };
    let created_message = state.borrow().created_message;
    if msg == WM_NCDESTROY {
        // SAFETY: unregister and reclaim exactly the Box installed in update.
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID);
            drop(Box::from_raw(data as *mut Shared));
        }
    } else if msg != 0 && msg == created_message {
        {
            let mut s = state.borrow_mut();
            s.added = false;
            s.applied_snapshot = None;
        }
        refresh(hwnd, &state);
        // A cover pushed before the taskbar button existed may have reached nothing.
        push_cover(hwnd, &state);
    } else if msg == WM_DWMSENDICONICTHUMBNAIL {
        // The maximum size arrives packed, width in the high word.
        let max_width = u32::try_from((lparam.0 >> 16) & 0xffff).unwrap_or(0);
        let max_height = u32::try_from(lparam.0 & 0xffff).unwrap_or(0);
        let cover = {
            let mut s = state.borrow_mut();
            s.requested_size = Some((max_width, max_height));
            s.cover.clone()
        };
        if let Some(cover) = cover {
            send_cover(hwnd, &cover, max_width, max_height);
            return LRESULT(0);
        }
    } else if msg == WM_DPICHANGED {
        // The size DWM asked for belonged to the old scale.
        state.borrow_mut().requested_size = None;
        restyle(hwnd, &state);
        push_cover(hwnd, &state);
    } else if msg == WM_SETTINGCHANGE {
        restyle(hwnd, &state);
    } else if msg == WM_COMMAND && ((wparam.0 >> 16) & 0xffff) == THBN_CLICKED as usize {
        let action = match wparam.0 & 0xffff {
            id if id == PREVIOUS as usize => Some(Action::Previous),
            id if id == PLAY_PAUSE as usize => Some(Action::PlayPause),
            id if id == NEXT as usize => Some(Action::Next),
            _ => None,
        };
        if let Some(action) = action {
            let callback = state.borrow().callback.clone();
            callback(action);
            return LRESULT(0);
        }
    }
    // SAFETY: forward all unhandled messages to the rest of the subclass chain.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

//! Windows taskbar thumbnail transport buttons, attached to the live UI window.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;

use netodia_core::error::describe;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass,
    THB_FLAGS, THB_ICON, THB_TOOLTIP, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THUMBBUTTON,
    TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIcon, DestroyIcon, HICON, RegisterWindowMessageW, WM_COMMAND, WM_NCDESTROY,
};
use windows::core::w;

const SUBCLASS_ID: usize = 0x4e54;
const PREVIOUS: u32 = 1;
const PLAY_PAUSE: u32 = 2;
const NEXT: u32 = 3;

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

type Callback = Arc<dyn Fn(Action) + Send + Sync>;
type Shared = Rc<RefCell<State>>;

struct Icon(HICON);
#[allow(unsafe_code, reason = "destroy the Win32 icon owned by this wrapper")]
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: each icon is created by CreateIcon and owned by this wrapper.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

struct State {
    taskbar: ITaskbarList3,
    icons: [Icon; 4],
    callback: Callback,
    created_message: u32,
    snapshot: Snapshot,
    applied_snapshot: Option<Snapshot>,
    added: bool,
}

/// Install or update the toolbar. Must be called on the window's UI thread with a live HWND.
/// Repeated calls also retry if Explorer was not ready when the window was first shown.
#[allow(unsafe_code, reason = "install the Win32 window subclass")]
pub fn update(hwnd: *mut c_void, snapshot: Snapshot, callback: Callback) {
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
    let changed = {
        let mut s = state.borrow_mut();
        s.snapshot = snapshot;
        s.applied_snapshot != Some(snapshot) || !s.added
    };
    if changed {
        refresh(hwnd, &state);
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
    let icons = [icon(PREVIOUS)?, icon(PLAY_PAUSE)?, icon(NEXT)?, icon(4)?];
    // SAFETY: the constant is a terminated UTF-16 string.
    let created_message = unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
    Some(Rc::new(RefCell::new(State {
        taskbar,
        icons,
        callback,
        created_message,
        snapshot: Snapshot::default(),
        applied_snapshot: None,
        added: false,
    })))
}

/// Draw a small monochrome icon: an AND mask for transparency and XOR mask for white pixels.
#[allow(unsafe_code, reason = "create a Win32 icon from its pixel masks")]
fn icon(kind: u32) -> Option<Icon> {
    let mut and_mask = [0xffu8; 32];
    let mut xor_mask = [0u8; 32];
    for y in 0..16usize {
        for x in 0..16usize {
            let on = match kind {
                PREVIOUS => {
                    (x == 3 && (3..13).contains(&y))
                        || ((5..12).contains(&x) && y.abs_diff(8) <= x - 5 && y.abs_diff(8) <= 5)
                }
                NEXT => {
                    (x == 12 && (3..13).contains(&y))
                        || ((4..11).contains(&x) && y.abs_diff(8) <= 10 - x && y.abs_diff(8) <= 5)
                }
                PLAY_PAUSE => (4..12).contains(&x) && y.abs_diff(8) <= (11 - x) * 3 / 4,
                _ => {
                    (4..7).contains(&x) && (3..13).contains(&y)
                        || (9..12).contains(&x) && (3..13).contains(&y)
                }
            };
            if on {
                let offset = y * 2 + x / 8;
                let bit = 0x80 >> (x % 8);
                and_mask[offset] &= !bit;
                xor_mask[offset] |= bit;
            }
        }
    }
    // SAFETY: both masks have 16 word-aligned rows, as required for a 16x16 1bpp icon.
    match unsafe { CreateIcon(None, 16, 16, 1, 1, and_mask.as_ptr(), xor_mask.as_ptr()) } {
        Ok(icon) => Some(Icon(icon)),
        Err(e) => {
            log::warn!("taskbar: icon creation failed: {}", describe(&e));
            None
        }
    }
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
    // Release the borrow before calling the shell, which may dispatch window messages.
    let (taskbar, buttons, snapshot, added) = {
        let s = state.borrow();
        let p = s.snapshot;
        (
            s.taskbar.clone(),
            [
                button(PREVIOUS, s.icons[0].0, "Previous", p.has_previous),
                button(
                    PLAY_PAUSE,
                    s.icons[if p.playing { 3 } else { 1 }].0,
                    if p.playing { "Pause" } else { "Play" },
                    true,
                ),
                button(NEXT, s.icons[2].0, "Next", p.has_next),
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

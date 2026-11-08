//! The GDI objects handed to the shell: a glyph's alpha icon and the bitmap the preview shows.

use std::ffi::c_void;

use netodia_core::error::describe;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};

pub(super) struct Bitmap(pub(super) HBITMAP);

#[allow(unsafe_code, reason = "delete the GDI bitmap owned by this wrapper")]
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateDIBSection or CreateBitmap and is owned here alone.
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

pub(super) struct Icon(pub(super) HICON);

#[allow(unsafe_code, reason = "destroy the Win32 icon owned by this wrapper")]
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: the icon came from CreateIconIndirect and is owned here alone.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

/// A 32-bit top-down DIB section holding `bgra`, which must be exactly `width` × `height` pixels.
#[allow(unsafe_code, reason = "create a DIB section and fill the pixels GDI allocated for it")]
pub(super) fn dib_section(width: u32, height: u32, bgra: &[u8]) -> Option<Bitmap> {
    if bgra.len() != 4 * width as usize * height as usize {
        log::warn!("taskbar: a {width}x{height} bitmap was handed {} bytes", bgra.len());
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
            biWidth: i32::try_from(width).ok()?,
            // Negative for top-down rows, the order every buffer here is written in.
            biHeight: -i32::try_from(height).ok()?,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` is a complete header for a 32-bit DIB and outlives the call, and `bits` is a
    // local the call writes the section's pixel pointer into.
    let section = match unsafe {
        CreateDIBSection(None, &raw const info, DIB_RGB_COLORS, &raw mut bits, None, 0)
    } {
        Ok(section) => Bitmap(section),
        Err(e) => {
            log::warn!("taskbar: bitmap creation failed: {}", describe(&e));
            return None;
        }
    };
    if bits.is_null() {
        return None;
    }
    // SAFETY: a 32-bit section of these dimensions owns exactly `bgra.len()` bytes at `bits`, and
    // nothing else can reach them before `section` is returned.
    unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), bgra.len()) }.copy_from_slice(bgra);
    Some(section)
}

/// An icon carrying its own alpha, from `size` × `size` straight-alpha BGRA.
#[allow(unsafe_code, reason = "create a Win32 icon from a colour bitmap and its mask")]
pub(super) fn icon(size: u32, bgra: &[u8]) -> Option<Icon> {
    let colour = dib_section(size, size, bgra)?;
    // Ignored where the colour bitmap has alpha, but an icon is refused without one. Monochrome
    // rows are padded to a 16-bit boundary.
    let mask_bytes = vec![0u8; size.div_ceil(16) as usize * 2 * size as usize];
    let size = i32::try_from(size).ok()?;
    // SAFETY: `mask_bytes` holds a full 1-bpp bitmap of these dimensions and outlives the call.
    let mask = Bitmap(unsafe { CreateBitmap(size, size, 1, 1, Some(mask_bytes.as_ptr().cast())) });
    if mask.0.is_invalid() {
        log::warn!("taskbar: icon mask creation failed");
        return None;
    }
    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask.0,
        hbmColor: colour.0,
    };
    // SAFETY: both bitmaps are live for the call, and the icon takes copies of them, so dropping
    // the wrappers afterwards is the documented cleanup.
    match unsafe { CreateIconIndirect(&raw const info) } {
        Ok(icon) => Some(Icon(icon)),
        Err(e) => {
            log::warn!("taskbar: icon creation failed: {}", describe(&e));
            None
        }
    }
}

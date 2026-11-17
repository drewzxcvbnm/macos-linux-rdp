//! macOS workarounds.

use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::runtime::{AnyClass, Sel};
use objc2::{rc::Retained, sel, AnyThread};
use objc2_app_kit::{NSCursor, NSImage};
use objc2_foundation::{NSPoint, NSSize};

/// Pointer to a cursor retained for the life of the process.
struct CursorPtr(NonNull<NSCursor>);
// SAFETY: NSCursor is immutable once created and only handed out as a +0 pointer.
unsafe impl Send for CursorPtr {}
unsafe impl Sync for CursorPtr {}

static INVISIBLE: OnceLock<CursorPtr> = OnceLock::new();

/// Replaces SDL's `+[NSCursor invisibleCursor]`, which decodes an embedded GIF through ImageIO and crashes
/// (EXC_ARM_DA_ALIGN) on recent macOS versions. SDL uses it whenever the cursor is hidden or relative mouse
/// mode is on. A cursor made from an empty 1x1 image is invisible without touching ImageIO.
pub fn patch_sdl_invisible_cursor() {
    let Some(class) = AnyClass::get(CStr::from_bytes_with_nul(b"NSCursor\0").unwrap()) else { return };
    let Some(method) = class.class_method(sel!(invisibleCursor)) else {
        log::debug!("SDL's NSCursor(InvisibleCursor) category not present; nothing to patch");
        return;
    };
    unsafe extern "C-unwind" fn invisible(_cls: *const AnyClass, _sel: Sel) -> *mut NSCursor {
        INVISIBLE
            .get_or_init(|| {
                let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(1.0, 1.0));
                let cursor = NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, NSPoint::new(0.0, 0.0));
                CursorPtr(NonNull::from(&*Retained::into_raw(cursor)))
            })
            .0
            .as_ptr()
    }
    // SAFETY: the replacement has the `+ (NSCursor *)invisibleCursor` signature SDL's own method has, and
    // returns a valid cursor that outlives every caller (it is never released).
    unsafe {
        let imp: unsafe extern "C-unwind" fn(*const AnyClass, Sel) -> *mut NSCursor = invisible;
        method.set_implementation(std::mem::transmute::<_, objc2::runtime::Imp>(imp));
    }
    log::debug!("patched SDL's invisible cursor to avoid the ImageIO GIF crash");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unpatched, this call crashes in ImageIO on affected macOS versions.
    #[test]
    #[ignore = "needs a window server"]
    fn invisible_cursor_is_patched() {
        let _sdl = sdl2::init().unwrap();
        patch_sdl_invisible_cursor();
        let class = AnyClass::get(CStr::from_bytes_with_nul(b"NSCursor\0").unwrap()).unwrap();
        let cursor: *mut NSCursor = unsafe { objc2::msg_send![class, invisibleCursor] };
        assert!(!cursor.is_null());
        assert_eq!(cursor, INVISIBLE.get().unwrap().0.as_ptr());
    }
}

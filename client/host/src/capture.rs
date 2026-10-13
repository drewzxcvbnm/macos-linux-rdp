//! X11 screen capture: MIT-SHM `GetImage` of one monitor, XDamage change tracking and the cursor
//! (from XFixes) blended into the image, since X never includes it in captures.

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::display::Rect;

/// A captured frame: BGRX, 4 bytes per pixel.
pub struct Image<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}

pub struct Capture {
    conn: RustConnection,
    root: xproto::Window,
    rect: Rect,
    shm: Option<ShmSegment>,
    /// Pixels from a non-SHM `GetImage` (e.g. X over the network).
    fallback: Vec<u8>,
    damage: Option<damage::Damage>,
    dirty: bool,
    show_cursor: bool,
    cursor: Option<xfixes::GetCursorImageReply>,
}

struct ShmSegment {
    seg: shm::Seg,
    addr: *mut u8,
    size: usize,
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.addr as *const libc::c_void) };
    }
}

impl Capture {
    pub fn new(rect: Rect, show_cursor: bool, track_damage: bool) -> Result<Capture> {
        let (conn, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let bpp = conn.setup().pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel);
        if !matches!(depth, 24 | 32) || bpp != Some(32) {
            bail!("unsupported X visual: depth {depth}, {bpp:?} bits per pixel (need 24/32-bit TrueColor)");
        }

        let shm = match ShmSegment::new(&conn, rect.w as usize * rect.h as usize * 4) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("capture: MIT-SHM unavailable ({e:#}); using slower GetImage");
                None
            }
        };

        let damage = if track_damage { Self::create_damage(&conn, root) } else { None };
        if show_cursor {
            conn.xfixes_query_version(4, 0)?.reply().context("XFixes is required for the cursor")?;
        }
        Ok(Capture { conn, root, rect, shm, fallback: vec![], damage, dirty: true, show_cursor, cursor: None })
    }

    fn create_damage(conn: &RustConnection, root: xproto::Window) -> Option<damage::Damage> {
        let r = (|| -> Result<damage::Damage> {
            conn.damage_query_version(1, 1)?.reply()?;
            let id = conn.generate_id()?;
            conn.damage_create(id, root, damage::ReportLevel::NON_EMPTY)?.check()?;
            Ok(id)
        })();
        match r {
            Ok(id) => Some(id),
            Err(e) => {
                log::warn!("capture: XDamage unavailable ({e:#}); capturing every frame");
                None
            }
        }
    }

    /// Whether anything changed since the last `grab` (screen contents or the cursor).
    pub fn changed(&mut self) -> Result<bool> {
        let Some(damage) = self.damage else { return Ok(true) };
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::DamageNotify(_) = event {
                self.dirty = true;
            }
        }
        if self.dirty {
            // NON_EMPTY reports once until the damage is subtracted.
            self.conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE)?;
        }
        if self.show_cursor {
            let c = self.conn.xfixes_get_cursor_image()?.reply()?;
            let moved = self.cursor.as_ref().is_none_or(|o| (o.x, o.y, o.cursor_serial) != (c.x, c.y, c.cursor_serial));
            // Redraw when it moves on, within or off this monitor.
            let was_visible = self.cursor.as_ref().is_some_and(|o| self.cursor_visible(o));
            self.dirty |= moved && (was_visible || self.cursor_visible(&c));
            self.cursor = Some(c);
        }
        Ok(self.dirty)
    }

    /// Captures the monitor (with the cursor drawn in).
    pub fn grab(&mut self) -> Result<Image<'_>> {
        self.dirty = false;
        let Rect { x, y, w, h } = self.rect;
        let (x, y, w16, h16) = (x as i16, y as i16, w as u16, h as u16);
        let data: &mut [u8] = if let Some(shm) = &self.shm {
            self.conn
                .shm_get_image(self.root, x, y, w16, h16, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)?
                .reply()
                .context("XShmGetImage failed (monitor layout changed?)")?;
            unsafe { std::slice::from_raw_parts_mut(shm.addr, shm.size) }
        } else {
            let reply = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, x, y, w16, h16, !0)?.reply()?;
            self.fallback = reply.data;
            &mut self.fallback
        };
        if self.show_cursor {
            if self.damage.is_none() || self.cursor.is_none() {
                self.cursor = Some(self.conn.xfixes_get_cursor_image()?.reply()?);
            }
            if let Some(c) = &self.cursor {
                blend_cursor(data, self.rect, c);
            }
        }
        Ok(Image { data, stride: w as usize * 4, width: w, height: h })
    }

    fn cursor_visible(&self, c: &xfixes::GetCursorImageReply) -> bool {
        let (cx, cy) = (c.x as i32 - c.xhot as i32, c.y as i32 - c.yhot as i32);
        let r = self.rect;
        cx < r.x + r.w as i32 + 1 && cy < r.y + r.h as i32 + 1 && cx + c.width as i32 >= r.x - 1 && cy + c.height as i32 >= r.y - 1
    }
}

impl ShmSegment {
    fn new(conn: &RustConnection, size: usize) -> Result<ShmSegment> {
        conn.shm_query_version()?.reply()?;
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            bail!("shmget: {}", std::io::Error::last_os_error());
        }
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if addr as isize == -1 {
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            bail!("shmat: {}", std::io::Error::last_os_error());
        }
        let seg = conn.generate_id()?;
        let attached = conn.shm_attach(seg, id as u32, false).map_err(anyhow::Error::from).and_then(|c| Ok(c.check()?));
        // The segment is freed once both sides detach.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        let segment = ShmSegment { seg, addr: addr as *mut u8, size };
        attached.context("XShmAttach")?;
        Ok(segment)
    }
}

/// Alpha-blends the (premultiplied ARGB) cursor onto a BGRX image of `rect`.
fn blend_cursor(data: &mut [u8], rect: Rect, c: &xfixes::GetCursorImageReply) {
    let ox = c.x as i32 - c.xhot as i32 - rect.x;
    let oy = c.y as i32 - c.yhot as i32 - rect.y;
    let (cw, ch) = (c.width as i32, c.height as i32);
    let stride = rect.w as usize * 4;
    for cy in 0..ch {
        let y = oy + cy;
        if y < 0 || y >= rect.h as i32 {
            continue;
        }
        for cx in 0..cw {
            let x = ox + cx;
            if x < 0 || x >= rect.w as i32 {
                continue;
            }
            let Some(&p) = c.cursor_image.get((cy * cw + cx) as usize) else { return };
            let a = p >> 24;
            if a == 0 {
                continue;
            }
            let i = y as usize * stride + x as usize * 4;
            let Some(px) = data.get_mut(i..i + 3) else { continue };
            let inv = 255 - a;
            for (k, shift) in [(0, 0), (1, 8), (2, 16)] {
                let src = (p >> shift) & 0xFF;
                px[k] = (src + (px[k] as u32 * inv + 127) / 255).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(x: i16, y: i16, pixels: Vec<u32>, w: u16) -> xfixes::GetCursorImageReply {
        xfixes::GetCursorImageReply {
            sequence: 0,
            length: 0,
            x,
            y,
            width: w,
            height: (pixels.len() / w as usize) as u16,
            xhot: 0,
            yhot: 0,
            cursor_serial: 0,
            cursor_image: pixels,
        }
    }

    #[test]
    fn blends_opaque_and_transparent_pixels() {
        let rect = Rect { x: 100, y: 0, w: 2, h: 1 };
        let mut img = vec![10u8; 8];
        // Opaque red at x=100, fully transparent at x=101.
        blend_cursor(&mut img, rect, &cursor(100, 0, vec![0xFFFF0000, 0x00000000], 2));
        assert_eq!(&img[..3], &[0, 0, 255]);
        assert_eq!(&img[4..7], &[10, 10, 10]);
    }

    #[test]
    fn cursor_outside_monitor_is_clipped() {
        let rect = Rect { x: 0, y: 0, w: 2, h: 2 };
        let mut img = vec![0u8; 16];
        blend_cursor(&mut img, rect, &cursor(-1, -1, vec![0xFFFFFFFF; 4], 2));
        assert_eq!(&img[..3], &[255, 255, 255]);
        assert_eq!(&img[4..7], &[0, 0, 0]);
    }
}

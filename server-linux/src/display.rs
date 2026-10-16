//! X11 monitors (RandR 1.5) as protocol displays.

use anyhow::{Context, Result};
use macrd_proto::DisplayInfo;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::ConnectionExt as _;

/// A rectangle on the X root window, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

pub struct Display {
    pub info: DisplayInfo,
    pub x: i32,
    pub y: i32,
}

impl Display {
    pub fn rect(&self) -> Rect {
        Rect { x: self.x, y: self.y, w: self.info.width_px, h: self.info.height_px }
    }
}

/// Display ids are RandR output ids (stable while the output exists); 0 is never used, it means "main".
pub fn list() -> Result<Vec<Display>> {
    let (conn, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
    let screen = &conn.setup().roots[screen_num];
    let mut out = vec![];
    let monitors = conn.randr_get_monitors(screen.root, true).ok().and_then(|c| c.reply().ok());
    if let Some(reply) = monitors {
        for (i, m) in reply.monitors.iter().enumerate() {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| format!("Monitor {}", i + 1));
            out.push(Display {
                info: DisplayInfo {
                    id: m.outputs.first().copied().filter(|&o| o != 0).unwrap_or(i as u32 + 1),
                    width_px: m.width as u32,
                    height_px: m.height as u32,
                    scale: 1.0,
                    is_main: m.primary,
                    name,
                },
                x: m.x as i32,
                y: m.y as i32,
            });
        }
    }
    if out.is_empty() {
        // No RandR: the whole screen is one display.
        out.push(Display {
            info: DisplayInfo {
                id: 1,
                width_px: screen.width_in_pixels as u32,
                height_px: screen.height_in_pixels as u32,
                scale: 1.0,
                is_main: true,
                name: "Screen".into(),
            },
            x: 0,
            y: 0,
        });
    }
    if !out.iter().any(|d| d.info.is_main) {
        out[0].info.is_main = true;
    }
    Ok(out)
}

/// The display a `StartStream.display_id` refers to: exact id, else the main display.
pub fn find(displays: Vec<Display>, id: u32) -> Option<Display> {
    let mut main = None;
    for d in displays {
        if d.info.id == id && id != 0 {
            return Some(d);
        }
        if d.info.is_main && main.is_none() {
            main = Some(d);
        }
    }
    main
}

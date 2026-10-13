//! Keeps the screensaver, screen locker and DPMS blanking away while a client is connected, by
//! periodically resetting the X screensaver timer (the same thing real input does).

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};

const INTERVAL: Duration = Duration::from_secs(30);

/// Active until the returned guard is dropped.
pub struct Guard(#[allow(dead_code)] mpsc::Sender<()>);

pub fn spawn() -> Guard {
    let (tx, rx) = mpsc::channel::<()>();
    thread::Builder::new()
        .name("awake".into())
        .spawn(move || {
            let Ok((conn, _)) = x11rb::connect(None) else { return };
            loop {
                let ok = conn.force_screen_saver(ScreenSaver::RESET).is_ok() && conn.flush().is_ok();
                if !ok {
                    return;
                }
                match rx.recv_timeout(INTERVAL) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        })
        .expect("spawn awake thread");
    Guard(tx)
}

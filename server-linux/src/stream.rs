//! The stream thread: captures the selected monitor at the requested rate, encodes and sends frames,
//! with the frame-ack flow control from PROTOCOL.md.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use macrd_proto::{Message, CODEC_H264, START_FLAG_VIRTUAL_DISPLAY};
use tokio::sync::mpsc::UnboundedSender;

use crate::capture::Capture;
use crate::display::{self, Rect};
use crate::encode::Encoder;
use crate::Config;

#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub bitrate_kbps: u32,
    pub codec: u8,
    pub flags: u8,
}

pub enum Cmd {
    Start(Params),
    Resize { width: u32, height: u32 },
    Keyframe,
    Ack(u64),
}

/// Unacknowledged frames older than this no longer count, so a lost ack cannot stall the stream.
const ACK_TIMEOUT: Duration = Duration::from_secs(1);
/// A static screen still gets a frame this often.
const REFRESH: Duration = Duration::from_secs(1);

/// `target` receives the streamed monitor's rectangle, for mapping mouse coordinates.
pub fn spawn(
    config: Arc<Config>,
    out: UnboundedSender<Message>,
    target: Arc<Mutex<Option<Rect>>>,
    peer: SocketAddr,
) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("stream".into())
        .spawn(move || {
            let mut s = Stream { config, out, target, peer, active: None, params: None, epoch: Instant::now(), stats: Stats::new() };
            s.run(rx);
        })
        .expect("spawn stream thread");
    tx
}

struct Active {
    capture: Capture,
    encoder: Encoder,
    interval: Duration,
    next_tick: Instant,
    last_sent: Instant,
    force_keyframe: bool,
    in_flight: HashMap<u64, Instant>,
}

struct Stream {
    config: Arc<Config>,
    out: UnboundedSender<Message>,
    target: Arc<Mutex<Option<Rect>>>,
    peer: SocketAddr,
    active: Option<Active>,
    params: Option<Params>,
    epoch: Instant,
    stats: Stats,
}

impl Stream {
    fn run(&mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            let wait = match &self.active {
                Some(a) => a.next_tick.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            match rx.recv_timeout(wait) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = self.tick() {
                        log::error!("[{}] stream: {e:#}", self.peer);
                        let _ = self.out.send(Message::Bye { reason: format!("stream error: {e}") });
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(p) => self.start(p),
            Cmd::Resize { width, height } => {
                let Some(mut p) = self.params else { return };
                if width == 0 || height == 0 || (p.width, p.height) == (width, height) {
                    return;
                }
                (p.width, p.height) = (width, height);
                self.start(p);
            }
            Cmd::Keyframe => {
                if let Some(a) = &mut self.active {
                    a.force_keyframe = true;
                    a.next_tick = Instant::now();
                }
            }
            Cmd::Ack(pts) => {
                if let Some(a) = &mut self.active {
                    a.in_flight.remove(&pts);
                }
            }
        }
    }

    fn start(&mut self, p: Params) {
        self.active = None;
        self.params = Some(p);
        match self.set_up(p) {
            Ok(a) => self.active = Some(a),
            Err(e) => {
                log::error!("[{}] stream start failed: {e:#}", self.peer);
                let _ = self.out.send(Message::Bye { reason: format!("stream start failed: {e}") });
            }
        }
    }

    fn set_up(&mut self, p: Params) -> Result<Active> {
        if p.flags & START_FLAG_VIRTUAL_DISPLAY != 0 {
            log::info!("[{}] virtual displays are not supported on X11; streaming a physical monitor", self.peer);
        }
        let d = display::find(display::list()?, p.display_id).ok_or_else(|| anyhow::anyhow!("no displays"))?;
        let fps = if p.fps == 0 { 60 } else { p.fps.min(240) } as u32;
        let bitrate = if p.bitrate_kbps == 0 { 40_000 } else { p.bitrate_kbps };
        let max_dim = if p.codec == CODEC_H264 { 4096 } else { 8192 };
        let rect = d.rect();
        let (w, h) = fit((rect.w, rect.h), (p.width, p.height), max_dim);

        let capture = Capture::new(rect, self.config.show_cursor, !self.config.always_capture)?;
        let encoder = Encoder::new((rect.w, rect.h), w, h, p.codec, fps, bitrate, self.config.encoder)?;
        *self.target.lock().unwrap() = Some(rect);
        log::info!(
            "[{}] streaming display {} ({}) at {w}x{h}@{fps} via {}, {bitrate} kbps",
            self.peer,
            d.info.id,
            d.info.name,
            encoder.name
        );
        let _ = self.out.send(Message::StreamStarted { display_id: d.info.id, width: w, height: h, codec: encoder.codec });
        let now = Instant::now();
        Ok(Active {
            capture,
            encoder,
            interval: Duration::from_secs_f64(1.0 / fps as f64),
            next_tick: now,
            last_sent: now,
            force_keyframe: true,
            in_flight: HashMap::new(),
        })
    }

    fn tick(&mut self) -> Result<()> {
        let Some(a) = &mut self.active else { return Ok(()) };
        let now = Instant::now();
        a.next_tick += a.interval;
        if a.next_tick < now {
            // Fell behind (slow capture/encode): do not try to catch up.
            a.next_tick = now + a.interval;
        }

        let changed = a.capture.changed()?;
        if !(changed || a.force_keyframe || a.last_sent.elapsed() >= REFRESH) {
            return Ok(());
        }
        a.in_flight.retain(|_, sent| now.duration_since(*sent) < ACK_TIMEOUT);
        if a.in_flight.len() >= self.config.max_in_flight {
            self.stats.skipped += 1;
            return Ok(());
        }

        let pts = self.epoch.elapsed().as_micros() as u64;
        let keyframe = std::mem::take(&mut a.force_keyframe);
        let img = a.capture.grab()?;
        for f in a.encoder.encode(&img, pts, keyframe)? {
            a.in_flight.insert(f.pts_us, Instant::now());
            self.stats.frames += 1;
            self.stats.bytes += f.data.len();
            let _ = self.out.send(Message::VideoFrame { pts_us: f.pts_us, keyframe: f.keyframe, data: f.data });
        }
        a.last_sent = Instant::now();
        self.stats.report(self.peer);
        Ok(())
    }
}

/// Scales `source` to fit inside `box` (0 = native) preserving aspect ratio; even dimensions, at most `max_dim`.
/// Same as `Session.fit` in the Swift server.
pub fn fit(source: (u32, u32), bx: (u32, u32), max_dim: u32) -> (u32, u32) {
    let (mut w, mut h) = (source.0 as f64, source.1 as f64);
    if bx.0 > 0 && bx.1 > 0 {
        let s = (bx.0 as f64 / w).min(bx.1 as f64 / h).min(1.0);
        w *= s;
        h *= s;
    }
    let s = (max_dim as f64 / w).min(max_dim as f64 / h).min(1.0);
    w *= s;
    h *= s;
    ((w as u32 & !1).max(16), (h as u32 & !1).max(16))
}

struct Stats {
    frames: u32,
    bytes: usize,
    skipped: u32,
    since: Instant,
}

impl Stats {
    fn new() -> Stats {
        Stats { frames: 0, bytes: 0, skipped: 0, since: Instant::now() }
    }

    fn report(&mut self, peer: SocketAddr) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        log::info!(
            "[{peer}] {:.1} fps, {:.1} Mbps, {} skipped",
            self.frames as f64 / secs,
            self.bytes as f64 * 8.0 / secs / 1e6,
            self.skipped
        );
        *self = Stats::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_matches_swift_server() {
        assert_eq!(fit((2560, 1440), (0, 0), 4096), (2560, 1440));
        assert_eq!(fit((2560, 1440), (1280, 1000), 4096), (1280, 720));
        assert_eq!(fit((3841, 2161), (5000, 5000), 8192), (3840, 2160));
        assert_eq!(fit((7680, 4320), (0, 0), 4096), (4096, 2304));
    }
}

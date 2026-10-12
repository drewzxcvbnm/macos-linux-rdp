//! macrd client: low-latency remote desktop into a macOS machine running macrd-server.

mod clipboard;
mod decode;
mod input;
mod net;
mod render;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use macrd_proto::{DisplayInfo, Message, CODEC_H264, CODEC_HEVC, DEFAULT_PORT, START_FLAG_VIRTUAL_DISPLAY};
use sdl2::event::{Event, WindowEvent};
use sdl2::video::FullscreenType;

use crate::decode::{FrameSlot, HwAccel};
use crate::input::{Hotkey, InputState};
use crate::net::{ConnectOptions, Inbound};

#[derive(Parser, Debug)]
#[command(version, about, after_help = HOTKEYS)]
struct Args {
    /// Server address: host or host:port
    host: String,
    /// Password (default: $MACRD_PASSWORD, else prompt)
    #[arg(long)]
    password: Option<String>,
    /// Disable TLS (the server must also run with --insecure)
    #[arg(long)]
    insecure: bool,
    /// Expected server TLS fingerprint (hex); otherwise known_hosts / trust-on-first-use
    #[arg(long)]
    fingerprint: Option<String>,
    /// Trust an unknown server fingerprint without asking
    #[arg(long)]
    accept_new: bool,
    /// Display to show: index (0, 1, ...) or CoreGraphics display ID; default main display
    #[arg(long)]
    display: Option<u32>,
    #[arg(long, value_enum, default_value_t = CodecArg::H264)]
    codec: CodecArg,
    /// Video bitrate in Mbps
    #[arg(long, default_value_t = 40)]
    bitrate: u32,
    #[arg(long, default_value_t = 60)]
    fps: u8,
    /// Start in fullscreen
    #[arg(long)]
    fullscreen: bool,
    /// Stream at the display's native resolution instead of the window size
    #[arg(long)]
    native: bool,
    /// Ask the server for a virtual display matching the window (server needs --virtual-display)
    #[arg(long)]
    virtual_display: bool,
    /// Swap Ctrl and Super so PC-style Ctrl+C/V maps to Cmd+C/V
    #[arg(long)]
    swap_cmd_ctrl: bool,
    #[arg(long, value_enum, default_value_t = HwAccel::Auto)]
    hwaccel: HwAccel,
    /// Disable clipboard sync
    #[arg(long)]
    no_clipboard: bool,
    /// Show the local cursor on top of the video
    #[arg(long)]
    local_cursor: bool,
    /// Pixels per touchpad scroll unit
    #[arg(long, default_value_t = 30.0)]
    scroll_speed: f32,
}

const HOTKEYS: &str = "Hotkeys:\n  Ctrl+Alt+G      toggle keyboard/mouse grab\n  Ctrl+Alt+F      toggle fullscreen\n  \
Ctrl+Alt+Right  next display\n  Ctrl+Alt+Left   previous display\n  Ctrl+Alt+Q      quit";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
}

/// Wakes the SDL event loop from other threads; coalesced so the event queue never floods.
struct Wake;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run(Args::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    ffmpeg_next::init().context("initializing FFmpeg")?;
    let (host, port) = split_host_port(&args.host)?;
    let password = match args.password.clone().or_else(|| std::env::var("MACRD_PASSWORD").ok()) {
        Some(p) => p,
        None => rpassword::prompt_password(format!("Password for {host}: "))?,
    };
    let fingerprint = match &args.fingerprint {
        Some(s) => Some(net::parse_fingerprint(s).ok_or_else(|| anyhow!("invalid --fingerprint"))?),
        None => None,
    };

    // SDL setup first so the network thread can wake the event loop.
    sdl2::hint::set("SDL_GRAB_KEYBOARD", "1");
    sdl2::hint::set("SDL_MOUSE_FOCUS_CLICKTHROUGH", "1");
    sdl2::hint::set("SDL_VIDEO_X11_NET_WM_BYPASS_COMPOSITOR", "0");
    let sdl = sdl2::init().map_err(|e| anyhow!(e))?;
    let video = sdl.video().map_err(|e| anyhow!(e))?;
    let events = sdl.event().map_err(|e| anyhow!(e))?;
    events.register_custom_event::<Wake>().map_err(|e| anyhow!(e))?;
    let wake_pending = Arc::new(AtomicBool::new(false));
    let make_waker = || {
        let sender = events.event_sender();
        let pending = wake_pending.clone();
        move || {
            if !pending.swap(true, Ordering::AcqRel) {
                let _ = sender.push_custom_event(Wake);
            }
        }
    };

    // Network → (video to decoder, control to main loop).
    let (video_tx, video_rx) = mpsc::channel::<Message>();
    let control: Arc<Mutex<Vec<Inbound>>> = Arc::default();
    let conn = {
        let control = control.clone();
        let waker = make_waker();
        let video_tx = Mutex::new(video_tx);
        net::connect(
            ConnectOptions { host: host.clone(), port, password, insecure: args.insecure, fingerprint, accept_new: args.accept_new },
            move |inbound| match inbound {
                Inbound::Video(m) => {
                    let _ = video_tx.lock().unwrap().send(m);
                }
                other => {
                    control.lock().unwrap().push(other);
                    waker();
                }
            },
        )?
    };
    let out = conn.out.clone();
    let displays = conn.displays.clone();
    if displays.is_empty() {
        bail!("server reported no displays");
    }
    for (i, d) in displays.iter().enumerate() {
        log::info!(
            "display {i}: id {} {}x{} scale {}{} — {}",
            d.id,
            d.width_px,
            d.height_px,
            d.scale,
            if d.is_main { " (main)" } else { "" },
            d.name
        );
    }
    let mut display_index = pick_display(&displays, args.display);

    let slot = Arc::new(FrameSlot::default());
    decode::spawn(video_rx, slot.clone(), out.clone(), args.hwaccel, make_waker());

    let (clip_tx, clip_rx) = mpsc::channel::<Message>();
    if !args.no_clipboard {
        clipboard::spawn(clip_rx, out.clone());
    }

    // Window sized to fit the remote display on the local screen.
    let d = &displays[display_index];
    let bounds = video.display_usable_bounds(0).map_err(|e| anyhow!(e))?;
    let logical = (d.width_px as f32 / d.scale.max(1.0), d.height_px as f32 / d.scale.max(1.0));
    let fit = (bounds.width() as f32 * 0.9 / logical.0).min(bounds.height() as f32 * 0.9 / logical.1).min(1.0);
    let (win_w, win_h) = ((logical.0 * fit) as u32, (logical.1 * fit) as u32);
    let mut window = video
        .window(&format!("macrd — {host}"), win_w.max(320), win_h.max(200))
        .position_centered()
        .resizable()
        .allow_highdpi()
        .build()?;
    if args.fullscreen {
        window.set_fullscreen(FullscreenType::Desktop).map_err(|e| anyhow!(e))?;
    }
    let canvas = window.into_canvas().accelerated().build()?;
    let mut renderer = render::Renderer::new(canvas);
    sdl.mouse().show_cursor(args.local_cursor);

    let codec = if args.codec == CodecArg::Hevc { CODEC_HEVC } else { CODEC_H264 };
    let start_stream = |index: usize, size: (u32, u32)| {
        let (w, h) = if args.native && !args.virtual_display { (0, 0) } else { size };
        let _ = out.send(Message::StartStream {
            display_id: displays[index].id,
            width: w,
            height: h,
            fps: args.fps,
            bitrate_kbps: args.bitrate.saturating_mul(1000),
            codec,
            flags: if args.virtual_display { START_FLAG_VIRTUAL_DISPLAY } else { 0 },
        });
    };
    start_stream(display_index, renderer.canvas.output_size().map_err(|e| anyhow!(e))?);

    let mut input = InputState::new(args.swap_cmd_ctrl);
    let mut grabbed = true;
    set_grab(&mut renderer, grabbed);
    let mut pending_resize: Option<Instant> = None;
    let mut last_ping = Instant::now() - Duration::from_secs(10);
    let mut rtt_ms: Option<f64> = None;
    let mut stats_since = Instant::now();
    let mut stats_frames = 0u64;
    let mut stats_decode_us = 0u64;
    let mut stats_bytes = conn.bytes_in.load(Ordering::Relaxed);
    let mut stream_label = String::new();
    let mut pump = sdl.event_pump().map_err(|e| anyhow!(e))?;

    'main: loop {
        let mut redraw = false;
        let first = pump.wait_event_timeout(50);
        for event in first.into_iter().chain(pump.poll_iter().collect::<Vec<_>>()) {
            if event.is_user_event() {
                wake_pending.store(false, Ordering::Release);
                continue;
            }
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::SizeChanged(..) | WindowEvent::Resized(..) => {
                        pending_resize = Some(Instant::now());
                        redraw = true;
                    }
                    WindowEvent::Exposed => redraw = true,
                    WindowEvent::FocusLost => {
                        for m in input.release_all() {
                            let _ = out.send(m);
                        }
                    }
                    _ => {}
                },
                Event::KeyDown { scancode: Some(sc), keymod, .. } => {
                    let (hotkey, msg) = input.key_down(sc, keymod);
                    if let Some(m) = msg {
                        let _ = out.send(m);
                    }
                    match hotkey {
                        Some(Hotkey::Quit) => break 'main,
                        Some(Hotkey::ToggleGrab) => {
                            grabbed = !grabbed;
                            set_grab(&mut renderer, grabbed);
                        }
                        Some(Hotkey::ToggleFullscreen) => {
                            let w = renderer.canvas.window_mut();
                            let next = if w.fullscreen_state() == FullscreenType::Off {
                                FullscreenType::Desktop
                            } else {
                                FullscreenType::Off
                            };
                            let _ = w.set_fullscreen(next);
                        }
                        Some(h @ (Hotkey::NextDisplay | Hotkey::PrevDisplay)) if displays.len() > 1 => {
                            let n = displays.len();
                            display_index = if h == Hotkey::NextDisplay { (display_index + 1) % n } else { (display_index + n - 1) % n };
                            log::info!("switching to display {display_index} ({})", displays[display_index].name);
                            start_stream(display_index, renderer.canvas.output_size().unwrap_or((0, 0)));
                        }
                        _ => {}
                    }
                }
                Event::KeyUp { scancode: Some(sc), .. } => {
                    if let Some(m) = input.key_up(sc) {
                        let _ = out.send(m);
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    if let Some(r) = video_rect_in_window(&renderer) {
                        let (nx, ny) = input::normalize(x, y, r);
                        let _ = out.send(Message::MouseMove { x: nx, y: ny });
                    }
                }
                Event::MouseButtonDown { mouse_btn, x, y, .. } | Event::MouseButtonUp { mouse_btn, x, y, .. } => {
                    let down = matches!(event, Event::MouseButtonDown { .. });
                    if let (Some(b), Some(r)) = (InputState::button(mouse_btn), video_rect_in_window(&renderer)) {
                        let (nx, ny) = input::normalize(x, y, r);
                        let _ = out.send(Message::MouseMove { x: nx, y: ny });
                        let _ = out.send(Message::MouseButton { button: b, down });
                    }
                }
                Event::MouseWheel { precise_x, precise_y, direction, .. } => {
                    if let Some(m) = input.wheel(precise_x, precise_y, direction, args.scroll_speed) {
                        let _ = out.send(m);
                    }
                }
                _ => {}
            }
        }

        // Control messages from the network thread.
        let inbound: Vec<Inbound> = std::mem::take(&mut *control.lock().unwrap());
        for m in inbound {
            match m {
                Inbound::Closed(reason) => {
                    eprintln!("{reason}");
                    return Ok(());
                }
                Inbound::Control(Message::StreamStarted { display_id, width, height, codec }) => {
                    stream_label = format!(
                        "{} {width}x{height} {}",
                        displays.iter().find(|d| d.id == display_id).map(|d| d.name.as_str()).unwrap_or("virtual display"),
                        if codec == CODEC_HEVC { "HEVC" } else { "H.264" }
                    );
                    log::info!("stream started: {stream_label}");
                }
                Inbound::Control(Message::Pong { t }) => {
                    rtt_ms = Some(now_us().saturating_sub(t) as f64 / 1000.0);
                }
                Inbound::Control(Message::Ping { t }) => {
                    let _ = out.send(Message::Pong { t });
                }
                Inbound::Control(m @ Message::Clipboard { .. }) => {
                    let _ = clip_tx.send(m);
                }
                Inbound::Control(_) | Inbound::Video(_) => {}
            }
        }

        // Newest decoded frame.
        if let Some(f) = slot.frame.lock().unwrap().take() {
            if let Err(e) = renderer.upload(&f) {
                log::warn!("render: {e:#}");
            }
            redraw = true;
        }
        if redraw {
            renderer.present();
        }

        if let Some(t) = pending_resize {
            if t.elapsed() > Duration::from_millis(300) {
                pending_resize = None;
                if !args.native || args.virtual_display {
                    let (w, h) = renderer.canvas.output_size().unwrap_or((0, 0));
                    if w > 0 && h > 0 {
                        let _ = out.send(Message::Resize { width: w, height: h });
                    }
                }
            }
        }

        if last_ping.elapsed() >= Duration::from_secs(1) {
            last_ping = Instant::now();
            let _ = out.send(Message::Ping { t: now_us() });
        }
        if stats_since.elapsed() >= Duration::from_secs(2) {
            let secs = stats_since.elapsed().as_secs_f64();
            let frames = slot.decoded.load(Ordering::Relaxed);
            let decode_us = slot.decode_us_total.load(Ordering::Relaxed);
            let bytes = conn.bytes_in.load(Ordering::Relaxed);
            let df = frames - stats_frames;
            let fps = df as f64 / secs;
            let mbps = (bytes - stats_bytes) as f64 * 8.0 / secs / 1e6;
            let dec_ms = if df > 0 { (decode_us - stats_decode_us) as f64 / df as f64 / 1000.0 } else { 0.0 };
            let rtt = rtt_ms.map(|r| format!(" · RTT {r:.1} ms")).unwrap_or_default();
            let grab = if grabbed { "" } else { " · ungrabbed (Ctrl+Alt+G)" };
            let _ = renderer.canvas.window_mut().set_title(&format!(
                "macrd — {host} · {stream_label} · {fps:.0} fps · {mbps:.1} Mbps · decode {dec_ms:.1} ms{rtt}{grab}"
            ));
            stats_frames = frames;
            stats_decode_us = decode_us;
            stats_bytes = bytes;
            stats_since = Instant::now();
        }
    }

    for m in input.release_all() {
        let _ = out.send(m);
    }
    let _ = out.send(Message::Bye { reason: "client quit".into() });
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

/// The video rectangle in window coordinates (mouse events use window points, not drawable pixels).
fn video_rect_in_window(renderer: &render::Renderer) -> Option<sdl2::rect::Rect> {
    let (ww, wh) = renderer.canvas.window().size();
    renderer.video_rect(ww, wh)
}

fn set_grab(renderer: &mut render::Renderer, grabbed: bool) {
    let w = renderer.canvas.window_mut();
    w.set_keyboard_grab(grabbed);
}

fn pick_display(displays: &[DisplayInfo], wanted: Option<u32>) -> usize {
    if let Some(w) = wanted {
        if let Some(i) = displays.iter().position(|d| d.id == w) {
            return i;
        }
        if (w as usize) < displays.len() {
            return w as usize;
        }
        log::warn!("display {w} not found, using the main display");
    }
    displays.iter().position(|d| d.is_main).unwrap_or(0)
}

fn split_host_port(s: &str) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // [ipv6]:port or [ipv6]
        let (h, tail) = rest.split_once(']').ok_or_else(|| anyhow!("bad address {s}"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse()?,
            None => DEFAULT_PORT,
        };
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Ok((h.to_string(), p.parse().context("bad port")?)),
        _ => Ok((s.to_string(), DEFAULT_PORT)),
    }
}

fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port() {
        assert_eq!(split_host_port("mac.local").unwrap(), ("mac.local".into(), DEFAULT_PORT));
        assert_eq!(split_host_port("10.0.0.2:9000").unwrap(), ("10.0.0.2".into(), 9000));
        assert_eq!(split_host_port("[fe80::1]:9000").unwrap(), ("fe80::1".into(), 9000));
        assert_eq!(split_host_port("fe80::1").unwrap(), ("fe80::1".into(), DEFAULT_PORT));
    }
}

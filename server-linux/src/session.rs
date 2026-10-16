//! One connected client: handshake, then routing messages to the stream, input and clipboard threads.
//! Mirrors `server-macos/Sources/MacRDCore/Net/Session.swift`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use macrd_proto::{auth_mac, frame_len, Message, PROTOCOL_VERSION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::net::Active;
use crate::stream::Cmd;
use crate::{awake, clipboard, display, input, stream, Config};

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const REPLACED: &str = "replaced by a new connection";

/// Runs the session to completion; returns why it ended.
pub async fn run<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    peer: SocketAddr,
    config: Arc<Config>,
    active: Arc<Active>,
    clipboard: std::sync::mpsc::Sender<clipboard::Event>,
) -> Result<String> {
    let (mut rd, mut wr) = tokio::io::split(io);
    match tokio::time::timeout(AUTH_TIMEOUT, authenticate(&mut rd, &mut wr, &config)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            send(&mut wr, &Message::Bye { reason: format!("{e}") }).await.ok();
            return Err(e);
        }
        Err(_) => {
            send(&mut wr, &Message::Bye { reason: "authentication timeout".into() }).await.ok();
            bail!("authentication timeout");
        }
    }
    log::info!("[{peer}] authenticated");
    let mut killed = active.take_over();

    let displays = display::list()?;
    // Input before the first StreamStarted goes to the main display.
    let main = displays.iter().find(|d| d.info.is_main).map(|d| d.rect());
    send(&mut wr, &Message::ServerInfo { displays: displays.into_iter().map(|d| d.info).collect() }).await?;

    let (out_tx, out_rx) = unbounded_channel();
    let target = Arc::new(Mutex::new(main));
    let stream_tx = stream::spawn(config.clone(), out_tx.clone(), target.clone(), peer);
    let input_tx = input::spawn(target);
    let _ = clipboard.send(clipboard::Event::Attach(out_tx.clone()));
    let _awake = awake::spawn();

    let reason = tokio::select! {
        r = read_loop(&mut rd, &out_tx, &stream_tx, &input_tx, &clipboard) => r.unwrap_or_else(|e| format!("connection error: {e:#}")),
        r = write_loop(&mut wr, out_rx) => match r {
            Ok(reason) => reason,
            Err(e) => format!("connection error: {e:#}"),
        },
        _ = &mut killed => REPLACED.to_string(),
    };
    if reason == REPLACED {
        send(&mut wr, &Message::Bye { reason: reason.clone() }).await.ok();
    }
    wr.shutdown().await.ok();
    Ok(reason)
}

async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(rd: &mut R, wr: &mut W, config: &Config) -> Result<()> {
    let mut nonce = None;
    loop {
        match read_message(rd).await? {
            Message::Ping { t } => send(wr, &Message::Pong { t }).await?,
            Message::Hello { version, .. } if nonce.is_none() => {
                if version != PROTOCOL_VERSION {
                    bail!("unsupported protocol version {version}");
                }
                let mut n = [0u8; 32];
                getrandom::getrandom(&mut n)?;
                nonce = Some(n);
                send(wr, &Message::AuthChallenge { nonce: n }).await?;
            }
            Message::AuthResponse { mac } => {
                let Some(n) = nonce else { continue };
                let expected = auth_mac(&config.password, &n, &config.fingerprint);
                // Constant-time comparison.
                let ok = expected.iter().zip(mac.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
                send(wr, &Message::AuthResult { ok }).await?;
                if !ok {
                    bail!("authentication failed");
                }
                return Ok(());
            }
            _ => {} // Everything else is ignored before authentication.
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    rd: &mut R,
    out: &UnboundedSender<Message>,
    stream: &std::sync::mpsc::Sender<Cmd>,
    input: &std::sync::mpsc::Sender<Message>,
    clipboard: &std::sync::mpsc::Sender<clipboard::Event>,
) -> Result<String> {
    loop {
        let m = read_message(rd).await?;
        match m {
            Message::StartStream { display_id, width, height, fps, bitrate_kbps, codec, flags } => {
                let _ = stream.send(Cmd::Start(stream::Params { display_id, width, height, fps, bitrate_kbps, codec, flags }));
            }
            Message::Resize { width, height } => {
                let _ = stream.send(Cmd::Resize { width, height });
            }
            Message::RequestKeyframe => {
                let _ = stream.send(Cmd::Keyframe);
            }
            Message::FrameAck { pts_us } => {
                let _ = stream.send(Cmd::Ack(pts_us));
            }
            m @ (Message::Key { .. }
            | Message::MouseMove { .. }
            | Message::MouseButton { .. }
            | Message::Scroll { .. }
            | Message::MediaKey { .. }) => {
                let _ = input.send(m);
            }
            m @ Message::Clipboard { .. } => {
                let _ = clipboard.send(clipboard::Event::Remote(m));
            }
            Message::Ping { t } => {
                let _ = out.send(Message::Pong { t });
            }
            Message::Bye { reason } => return Ok(format!("client said bye: {reason}")),
            _ => {}
        }
    }
}

/// Returns when the outgoing queue closes or a `Bye` was sent.
async fn write_loop<W: AsyncWrite + Unpin>(wr: &mut W, mut rx: UnboundedReceiver<Message>) -> Result<String> {
    while let Some(m) = rx.recv().await {
        let mut bye = match &m {
            Message::Bye { reason } => Some(reason.clone()),
            _ => None,
        };
        wr.write_all(&m.encode()).await?;
        // Coalesce whatever else is queued into one flush.
        while bye.is_none() {
            let Ok(m) = rx.try_recv() else { break };
            if let Message::Bye { reason } = &m {
                bye = Some(reason.clone());
            }
            wr.write_all(&m.encode()).await?;
        }
        wr.flush().await?;
        if let Some(reason) = bye {
            return Ok(reason);
        }
    }
    Ok("session ended".into())
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, m: &Message) -> Result<()> {
    wr.write_all(&m.encode()).await?;
    wr.flush().await?;
    Ok(())
}

async fn read_message<R: AsyncRead + Unpin>(rd: &mut R) -> Result<Message> {
    loop {
        let mut header = [0u8; 4];
        rd.read_exact(&mut header).await?;
        let len = frame_len(header)?;
        let mut buf = vec![0u8; len];
        rd.read_exact(&mut buf).await?;
        if let Some(m) = Message::decode(buf[0], &buf[1..])? {
            return Ok(m);
        }
    }
}

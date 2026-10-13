//! Connection to the server: TLS with fingerprint pinning, handshake, reader/writer tasks.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use anyhow::{anyhow, bail, Context, Result};
use macrd_proto::{auth_mac, frame_len, DisplayInfo, Message, PROTOCOL_VERSION};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub password: String,
    pub insecure: bool,
    /// Fingerprint given on the command line (hex, colons optional); overrides known_hosts.
    pub fingerprint: Option<[u8; 32]>,
    /// Trust an unknown server without asking.
    pub accept_new: bool,
}

/// What the network thread hands to the rest of the client.
pub enum Inbound {
    /// A video frame or stream reconfiguration, in stream order (goes to the decoder).
    Video(Message),
    /// Any other message from the server.
    Control(Message),
    /// The connection ended.
    Closed(String),
}

pub struct Connection {
    pub displays: Vec<DisplayInfo>,
    pub out: UnboundedSender<Message>,
    /// Total bytes received, for stats.
    pub bytes_in: Arc<AtomicU64>,
}

/// Connects and authenticates on a dedicated network thread. `on_inbound` is called from that thread.
pub fn connect(opts: ConnectOptions, on_inbound: impl Fn(Inbound) + Send + 'static) -> Result<Connection> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Connection>>();
    thread::Builder::new().name("net".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let bytes_in = Arc::new(AtomicU64::new(0));
            let (out_tx, out_rx) = unbounded_channel();
            let stream = match establish(&opts).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let (mut rd, mut wr) = tokio::io::split(stream.io);
            let displays = match handshake(&mut rd, &mut wr, &opts.password, &stream.fingerprint).await {
                Ok(d) => d,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(Connection { displays, out: out_tx, bytes_in: bytes_in.clone() }));

            let reason = tokio::select! {
                r = read_loop(&mut rd, &on_inbound, &bytes_in) => match r {
                    Ok(reason) => reason,
                    Err(e) => format!("connection error: {e:#}"),
                },
                r = write_loop(&mut wr, out_rx) => match r {
                    Ok(()) => "client closed".to_string(),
                    Err(e) => format!("connection error: {e:#}"),
                },
            };
            on_inbound(Inbound::Closed(reason));
        });
    })?;
    ready_rx.recv().map_err(|_| anyhow!("network thread exited"))?
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Established {
    io: Box<dyn Io>,
    fingerprint: [u8; 32],
}

async fn establish(opts: &ConnectOptions) -> Result<Established> {
    let tcp = TcpStream::connect((opts.host.as_str(), opts.port))
        .await
        .with_context(|| format!("connecting to {}:{}", opts.host, opts.port))?;
    tcp.set_nodelay(true)?;
    if opts.insecure {
        log::warn!("TLS disabled (--insecure); traffic is unencrypted");
        return Ok(Established { io: Box::new(tcp), fingerprint: [0; 32] });
    }

    let host_key = format!("{}:{}", opts.host, opts.port);
    let pinned = opts.fingerprint.or_else(|| known_hosts::lookup(&host_key));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinVerifier { expected: pinned, observed: Mutex::new(None), provider: provider.clone() });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let server_name = ServerName::try_from(opts.host.clone()).unwrap_or_else(|_| ServerName::try_from("macrd").unwrap());
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .map_err(|e| {
            if let Some(expected) = pinned {
                if let Some(seen) = *verifier.observed.lock().unwrap() {
                    if seen != expected {
                        return anyhow!(
                            "SERVER FINGERPRINT MISMATCH for {host_key}!\n  expected {}\n  got      {}\n\
                             Someone may be intercepting the connection. If the server was reinstalled, \
                             remove its line from {}",
                            hex(&expected),
                            hex(&seen),
                            known_hosts::path().display()
                        );
                    }
                }
            }
            anyhow!("TLS handshake failed: {e}")
        })?;
    let fingerprint = verifier.observed.lock().unwrap().ok_or_else(|| anyhow!("no server certificate"))?;

    if pinned.is_none() {
        eprintln!("The server at {host_key} is not known.\nTLS fingerprint (SHA-256): {}", hex(&fingerprint));
        eprintln!("Compare it with the fingerprint printed by macrd-server.");
        if !opts.accept_new {
            eprint!("Trust this server? [y/N] ");
            std::io::stderr().flush().ok();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                bail!("server not trusted");
            }
        }
        known_hosts::add(&host_key, &fingerprint)?;
        eprintln!("Saved to {}", known_hosts::path().display());
    }
    Ok(Established { io: Box::new(tls), fingerprint })
}

async fn handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    rd: &mut R,
    wr: &mut W,
    password: &str,
    fingerprint: &[u8; 32],
) -> Result<Vec<DisplayInfo>> {
    send(wr, &Message::Hello { version: PROTOCOL_VERSION, caps: 0 }).await?;
    let nonce = match read_message(rd).await? {
        Message::AuthChallenge { nonce } => nonce,
        Message::Bye { reason } => bail!("server closed the connection: {reason}"),
        other => bail!("unexpected message during handshake: {other:?}"),
    };
    send(wr, &Message::AuthResponse { mac: auth_mac(password, &nonce, fingerprint) }).await?;
    loop {
        match read_message(rd).await? {
            Message::AuthResult { ok: true } => {}
            Message::AuthResult { ok: false } => bail!("authentication failed (wrong password?)"),
            Message::ServerInfo { displays } => return Ok(displays),
            Message::Bye { reason } => bail!("server closed the connection: {reason}"),
            Message::Clipboard { .. } | Message::Pong { .. } => {}
            other => bail!("unexpected message during handshake: {other:?}"),
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, m: &Message) -> Result<()> {
    wr.write_all(&m.encode()).await?;
    wr.flush().await?;
    Ok(())
}

async fn read_message<R: AsyncRead + Unpin>(rd: &mut R) -> Result<Message> {
    loop {
        let (ty, payload) = read_frame(rd).await?;
        if let Some(m) = Message::decode(ty, &payload)? {
            return Ok(m);
        }
    }
}

async fn read_frame<R: AsyncRead + Unpin>(rd: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    rd.read_exact(&mut header).await?;
    let len = frame_len(header)?;
    let mut buf = vec![0u8; len];
    rd.read_exact(&mut buf).await?;
    let ty = buf[0];
    buf.remove(0);
    Ok((ty, buf))
}

async fn read_loop<R: AsyncRead + Unpin>(
    rd: &mut R,
    on_inbound: &impl Fn(Inbound),
    bytes_in: &AtomicU64,
) -> Result<String> {
    loop {
        let (ty, payload) = read_frame(rd).await?;
        bytes_in.fetch_add(payload.len() as u64 + 5, Ordering::Relaxed);
        let Some(m) = Message::decode(ty, &payload)? else { continue };
        match m {
            Message::Bye { reason } => return Ok(format!("server closed the connection: {reason}")),
            m @ (Message::VideoFrame { .. } | Message::StreamStarted { .. }) => {
                if let Message::StreamStarted { .. } = &m {
                    on_inbound(Inbound::Control(m.clone()));
                }
                on_inbound(Inbound::Video(m));
            }
            m => on_inbound(Inbound::Control(m)),
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(wr: &mut W, mut rx: UnboundedReceiver<Message>) -> Result<()> {
    while let Some(m) = rx.recv().await {
        let bye = matches!(m, Message::Bye { .. });
        wr.write_all(&m.encode()).await?;
        // Coalesce whatever else is queued into one flush (one TLS record / TCP segment burst).
        while let Ok(m) = rx.try_recv() {
            wr.write_all(&m.encode()).await?;
        }
        wr.flush().await?;
        if bye {
            wr.shutdown().await.ok();
            break;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PinVerifier {
    expected: Option<[u8; 32]>,
    observed: Mutex<Option<[u8; 32]>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        *self.observed.lock().unwrap() = Some(fp);
        match self.expected {
            Some(expected) if expected != fp => Err(rustls::Error::General("server fingerprint mismatch".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() != 64 || s.chars().any(|c| !c.is_ascii_hexdigit() && c != ':' && c != ' ') {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in clean.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

/// `~/.config/macrd/known_hosts`: one `host:port FINGERPRINT` per line.
mod known_hosts {
    use super::*;

    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("macrd").join("known_hosts")
    }

    pub fn lookup(host: &str) -> Option<[u8; 32]> {
        let text = std::fs::read_to_string(path()).ok()?;
        text.lines().find_map(|line| {
            let (h, fp) = line.trim().split_once(char::is_whitespace)?;
            (h == host).then(|| parse_fingerprint(fp.trim())).flatten()
        })
    }

    pub fn add(host: &str, fp: &[u8; 32]) -> Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        writeln!(f, "{host} {}", hex(fp))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trip() {
        let fp: [u8; 32] = core::array::from_fn(|i| i as u8 * 7);
        assert_eq!(parse_fingerprint(&hex(&fp)), Some(fp));
        assert_eq!(parse_fingerprint(&hex(&fp).replace(':', "")), Some(fp));
        assert_eq!(parse_fingerprint("zz"), None);
    }
}

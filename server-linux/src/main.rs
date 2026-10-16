//! macrd-server-linux: shares a Linux X11 desktop with macrd clients, speaking the same protocol as the macOS
//! `macrd-server` (see PROTOCOL.md).

mod awake;
mod capture;
mod clipboard;
mod display;
mod encode;
mod identity;
mod input;
mod keymap;
mod net;
mod session;
mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use ffmpeg_next as ffmpeg;
use macrd_proto::DEFAULT_PORT;

use crate::encode::EncoderKind;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// Disable TLS (development only; the client must also use --insecure)
    #[arg(long)]
    insecure: bool,
    /// Connection password (default: $MACRD_PASSWORD, else generated and stored in ~/.config/macrd-server-linux/password)
    #[arg(long)]
    password: Option<String>,
    /// Do not draw the cursor into the video
    #[arg(long)]
    hide_cursor: bool,
    /// Frames sent but not yet acknowledged
    #[arg(long, default_value_t = 3)]
    max_in_flight: usize,
    /// Video encoder
    #[arg(long, value_enum, default_value_t = EncoderKind::Auto)]
    encoder: EncoderKind,
    /// Capture every frame instead of only when XDamage reports changes
    #[arg(long)]
    always_capture: bool,
    /// Print displays and exit
    #[arg(long)]
    list_displays: bool,
}

pub struct Config {
    pub password: String,
    /// SHA-256 of the TLS certificate DER, or zeros with --insecure.
    pub fingerprint: [u8; 32],
    pub max_in_flight: usize,
    pub show_cursor: bool,
    pub encoder: EncoderKind,
    pub always_capture: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    if args.list_displays {
        for d in display::list()? {
            println!(
                "{}\t{}x{}+{}+{}\t{}{}",
                d.info.id,
                d.info.width_px,
                d.info.height_px,
                d.x,
                d.y,
                if d.info.is_main { "main\t" } else { "\t" },
                d.info.name
            );
        }
        return Ok(());
    }

    ffmpeg::init().context("initializing FFmpeg")?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    display::list().context("cannot talk to the X server (is DISPLAY set?)")?;

    let password = match args.password.or_else(|| std::env::var("MACRD_PASSWORD").ok()) {
        Some(p) => p,
        None => stored_password()?,
    };
    let tls = if args.insecure {
        log::warn!("TLS disabled (--insecure); traffic is unencrypted");
        None
    } else {
        let id = identity::load_or_create(&config_dir())?;
        println!("TLS fingerprint (SHA-256): {}", identity::hex(&id.fingerprint));
        Some(id)
    };
    let config = Arc::new(Config {
        password,
        fingerprint: tls.as_ref().map_or([0; 32], |t| t.fingerprint),
        max_in_flight: args.max_in_flight.max(1),
        show_cursor: !args.hide_cursor,
        encoder: args.encoder,
        always_capture: args.always_capture,
    });

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(net::serve(&args.bind, args.port, config, tls))
}

pub fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("macrd-server-linux");
    // Keep the identity and password from before the rename (`macrd-host`), so clients stay paired.
    let old = base.join("macrd-host");
    if !dir.exists() && old.is_dir() && std::fs::rename(&old, &dir).is_ok() {
        log::info!("moved {} to {}", old.display(), dir.display());
    }
    dir
}

/// Generated on first run, like the macOS server does.
fn stored_password() -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = config_dir().join("password");
    if let Ok(p) = std::fs::read_to_string(&path) {
        let p = p.trim().to_string();
        if !p.is_empty() {
            println!("Password: stored in {}", path.display());
            return Ok(p);
        }
    }
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw)?;
    let p: String = raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    std::fs::create_dir_all(config_dir())?;
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
    std::io::Write::write_all(&mut f, p.as_bytes())?;
    println!("Generated password: {p}\n(stored in {})", path.display());
    Ok(p)
}

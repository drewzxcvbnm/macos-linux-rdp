//! Finds the user's running X session when started outside of it (SSH, a TTY, a systemd unit), by
//! borrowing `DISPLAY`/`XAUTHORITY` from the environment of one of the user's own X clients.

use std::os::unix::fs::MetadataExt;

/// Window managers / session processes preferred as the source, in order.
const PREFERRED: &[&str] = &["i3", "sway", "xfwm4", "openbox", "bspwm", "awesome", "kwin_x11", "gnome-shell", "xfce4-session"];

/// Sets `DISPLAY` (and `XAUTHORITY` when the session has one) unless `DISPLAY` is already set.
/// Must run before any other thread starts.
pub fn detect() {
    if std::env::var_os("DISPLAY").is_some() {
        return;
    }
    let Some(found) = find() else {
        log::warn!("DISPLAY is not set and no running X session of this user was found");
        return;
    };
    log::info!(
        "DISPLAY not set; using {} (XAUTHORITY {}) from the running session ({})",
        found.display,
        found.xauthority.as_deref().unwrap_or("default"),
        found.source
    );
    // Single-threaded at this point.
    std::env::set_var("DISPLAY", &found.display);
    if let Some(x) = &found.xauthority {
        if std::env::var_os("XAUTHORITY").is_none() {
            std::env::set_var("XAUTHORITY", x);
        }
    }
}

struct Found {
    display: String,
    xauthority: Option<String>,
    source: String,
}

fn find() -> Option<Found> {
    let uid = unsafe { libc::getuid() };
    let me = std::process::id();
    let mut best: Option<(usize, Found)> = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == me || entry.metadata().ok().map(|m| m.uid()) != Some(uid) {
            continue;
        }
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else { continue };
        let (display, xauthority) = parse_environ(&environ);
        let Some(display) = display else { continue };
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default().trim().to_string();
        let rank = PREFERRED.iter().position(|p| *p == comm).unwrap_or(PREFERRED.len());
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, Found { display, xauthority, source: format!("{comm}, pid {pid}") }));
        }
    }
    best.map(|(_, f)| f)
}

fn parse_environ(environ: &[u8]) -> (Option<String>, Option<String>) {
    let (mut display, mut xauthority) = (None, None);
    for var in environ.split(|&b| b == 0) {
        let var = String::from_utf8_lossy(var);
        if let Some(v) = var.strip_prefix("DISPLAY=") {
            display = Some(v.to_string()).filter(|v| !v.is_empty());
        } else if let Some(v) = var.strip_prefix("XAUTHORITY=") {
            xauthority = Some(v.to_string()).filter(|v| !v.is_empty());
        }
    }
    (display, xauthority)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_environ() {
        let env = b"HOME=/home/u\0DISPLAY=:0\0XAUTHORITY=/run/user/1000/lyxauth\0";
        assert_eq!(parse_environ(env), (Some(":0".into()), Some("/run/user/1000/lyxauth".into())));
        assert_eq!(parse_environ(b"DISPLAY=\0"), (None, None));
    }
}

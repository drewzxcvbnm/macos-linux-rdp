//! Self-signed TLS identity, created on first run and kept in the config directory.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub struct Identity {
    pub acceptor: tokio_rustls::TlsAcceptor,
    pub fingerprint: [u8; 32],
}

pub fn load_or_create(dir: &Path) -> Result<Identity> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    let (cert, key) = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        (Ok(c), Ok(k)) => (c, k),
        _ => {
            let ck = rcgen::generate_simple_self_signed(vec!["macrd".to_string()]).context("generating certificate")?;
            let (cert, key) = (ck.cert.der().to_vec(), ck.key_pair.serialize_der());
            std::fs::create_dir_all(dir)?;
            write_private(&key_path, &key)?;
            std::fs::write(&cert_path, &cert)?;
            log::info!("created TLS identity in {}", dir.display());
            (cert, key)
        }
    };
    let fingerprint: [u8; 32] = Sha256::digest(&cert).into();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
        .with_context(|| format!("loading TLS identity from {}", dir.display()))?;
    Ok(Identity { acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)), fingerprint })
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut f, data)?;
    Ok(())
}

/// Same format the client prints and stores in known_hosts.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

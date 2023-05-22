use std::{fs, path::PathBuf, sync::Arc};

use anyhow::Context;
use quinn::{ClientConfig, TransportConfig};
use rustls::RootCertStore;
use tracing::info;
use webpki_roots::TLS_SERVER_ROOTS;

use crate::util::waht_data_root;

use super::WAHT_ALPN;

pub fn make_selfsigned_server_config(
    keylog: bool,
    server_name: String,
) -> Result<rustls::ServerConfig, anyhow::Error> {
    let (certs, key) = load_or_generate_selfsigned_server_cert(server_name)?;
    let mut crypto = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    crypto.alpn_protocols = vec![WAHT_ALPN.to_vec()];
    if keylog {
        crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    }
    Ok(crypto)
}

pub fn load_or_generate_selfsigned_server_cert(
    server_name: String,
) -> anyhow::Result<(Vec<rustls::Certificate>, rustls::PrivateKey)> {
    let path = waht_data_root()?
        .join("tls")
        .join("selfsigned")
        .join(&server_name);
    let key_path = path.join("key.der");
    let cert_path = path.join("cert.der");
    match load_selfsigned_server_cert(&key_path, &cert_path) {
        Ok(res) => Ok(res),
        Err(_err) => generate_selfigned_server_certs(vec![server_name], &key_path, &cert_path),
    }
}

pub fn load_selfsigned_server_cert(
    key_path: &PathBuf,
    cert_path: &PathBuf,
) -> anyhow::Result<(Vec<rustls::Certificate>, rustls::PrivateKey)> {
    let key = std::fs::read(key_path).context("failed to read private key")?;
    let key = if key_path.extension().map_or(false, |x| x == "der") {
        rustls::PrivateKey(key)
    } else {
        let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
            .context("malformed PKCS #8 private key")?;
        match pkcs8.into_iter().next() {
            Some(x) => rustls::PrivateKey(x),
            None => {
                let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                    .context("malformed PKCS #1 private key")?;
                match rsa.into_iter().next() {
                    Some(x) => rustls::PrivateKey(x),
                    None => {
                        anyhow::bail!("no private keys found");
                    }
                }
            }
        }
    };
    let cert_chain = std::fs::read(cert_path).context("failed to read certificate chain")?;
    let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
        vec![rustls::Certificate(cert_chain)]
    } else {
        rustls_pemfile::certs(&mut &*cert_chain)
            .context("invalid PEM-encoded certificate")?
            .into_iter()
            .map(rustls::Certificate)
            .collect()
    };

    Ok((cert_chain, key))
}

pub fn generate_selfigned_server_certs(
    server_names: Vec<String>,
    key_path: &PathBuf,
    cert_path: &PathBuf,
) -> anyhow::Result<(Vec<rustls::Certificate>, rustls::PrivateKey)> {
    info!("generating self-signed certificate");
    let cert = rcgen::generate_simple_self_signed(server_names).unwrap();
    let key = cert.serialize_private_key_der();
    let cert = cert.serialize_der().unwrap();
    fs::create_dir_all(key_path.parent().context("invalid key path")?)
        .context("failed to create certificate directory")?;
    fs::create_dir_all(cert_path.parent().context("invalid cert path")?)
        .context("failed to create certificate directory")?;
    fs::write(&cert_path, &cert).context("failed to write certificate")?;
    fs::write(&key_path, &key).context("failed to write private key")?;
    let key = rustls::PrivateKey(key);
    let cert = rustls::Certificate(cert);
    Ok((vec![cert], key))
}

// Implementation of `ServerCertVerifier` that verifies everything as trustworthy.
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

pub fn configure_client(accept_insecure: bool) -> ClientConfig {
    let crypto = rustls::ClientConfig::builder().with_safe_defaults();
    let mut crypto = if accept_insecure {
        crypto
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth()
    } else {
        let mut root_store = RootCertStore::empty();
        root_store.add_server_trust_anchors(TLS_SERVER_ROOTS.0.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        crypto
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    crypto.alpn_protocols = vec![WAHT_ALPN.to_vec()];

    let transport = TransportConfig::default();

    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport));
    config
}

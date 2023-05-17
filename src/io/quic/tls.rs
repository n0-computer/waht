use std::sync::Arc;

use quinn::ClientConfig;

use super::WAHT_ALPN;

pub fn make_server_config(keylog: bool) -> Result<rustls::ServerConfig, anyhow::Error> {
    let (certs, key) = generate_server_cert();
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

pub fn generate_server_cert() -> (Vec<rustls::Certificate>, rustls::PrivateKey) {
    tracing::info!("generating self-signed certificate");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = cert.serialize_private_key_der();
    let cert = cert.serialize_der().unwrap();
    // fs::create_dir_all(path).context("failed to create certificate directory")?;
    // fs::write(&cert_path, &cert).context("failed to write certificate")?;
    // fs::write(&key_path, &key).context("failed to write private key")?;

    let key = rustls::PrivateKey(key);
    let cert = rustls::Certificate(cert);
    (vec![cert], key)
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

pub fn configure_client() -> ClientConfig {
    let mut crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();

    crypto.alpn_protocols = vec![WAHT_ALPN.to_vec()];

    ClientConfig::new(Arc::new(crypto))
}


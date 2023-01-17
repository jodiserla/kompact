use std::{sync::Arc};
use quinn_proto::{
    ServerConfig,
    ClientConfig,
};
use rustls::{Certificate, KeyLogFile, PrivateKey};
use lazy_static::lazy_static;

pub fn server_config() -> ServerConfig {
    ServerConfig::with_crypto(Arc::new(server_crypto()))
}

pub fn server_crypto() -> rustls::ServerConfig {
    let cert = Certificate(CERTIFICATE.serialize_der().unwrap());
    let key = PrivateKey(CERTIFICATE.serialize_private_key_der());
    server_crypto_with_cert(cert, key)
}

pub fn server_crypto_with_cert(cert: Certificate, key: PrivateKey) -> rustls::ServerConfig {
    server_conf(vec![cert], key).unwrap()
}

pub fn server_conf(
    cert_chain: Vec<rustls::Certificate>,
    key: rustls::PrivateKey,
) -> Result<rustls::ServerConfig, rustls::Error> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    cfg.max_early_data_size = u32::MAX;
    Ok(cfg)
}

pub fn client_config() -> ClientConfig {
    ClientConfig::new(Arc::new(client_crypto()))
}

pub fn client_crypto() -> rustls::ClientConfig {
    let cert = rustls::Certificate(CERTIFICATE.serialize_der().unwrap());
    client_crypto_with_certs(vec![cert])
}

pub fn client_crypto_with_certs(certs: Vec<rustls::Certificate>) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(&cert).unwrap();
    }
    let mut config = rustls::ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.enable_early_data = true;
    config.key_log = Arc::new(KeyLogFile::new());
    config
}

lazy_static! {
    static ref CERTIFICATE: rcgen::Certificate =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
}
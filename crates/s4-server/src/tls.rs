//! TLS termination helpers.
//!
//! Used by the binary's listener wiring. Kept as a separate library module so
//! parsing logic (`load_tls_config`) is unit-testable and the `tokio-rustls`
//! dependency is centralised here.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Loads PEM cert + key files into a rustls `ServerConfig` ready for
/// `TlsAcceptor::from`. Supports PKCS#8 and RSA private keys via
/// `rustls_pemfile::private_key`.
///
/// ALPN protocols default to `h2` then `http/1.1` — matching the
/// `hyper_util::server::conn::auto::Builder` upstream so HTTP/2 is negotiated
/// when the client offers it.
pub fn load_tls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ServerConfig>, Box<dyn Error + Send + Sync + 'static>> {
    use std::fs::File;
    use std::io::BufReader;

    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path.display()).into());
    }

    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| format!("no private key found in {}", key_path.display()))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Installs the `ring` crypto provider as the process-wide default. rustls
/// 0.23+ requires this before any `ServerConfig::builder()` call. Idempotent.
pub fn install_default_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Helper: write a self-signed cert+key pair to two NamedTempFiles using
    /// rcgen and return them so the test can pass paths to load_tls_config.
    fn write_self_signed_pair() -> (NamedTempFile, NamedTempFile) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut cert_file = NamedTempFile::new().unwrap();
        cert_file.write_all(cert.cert.pem().as_bytes()).unwrap();
        cert_file.flush().unwrap();
        let mut key_file = NamedTempFile::new().unwrap();
        key_file
            .write_all(cert.key_pair.serialize_pem().as_bytes())
            .unwrap();
        key_file.flush().unwrap();
        (cert_file, key_file)
    }

    #[test]
    fn loads_pkcs8_cert_and_key() {
        install_default_crypto_provider();
        let (cert, key) = write_self_signed_pair();
        let cfg = load_tls_config(cert.path(), key.path()).expect("config should load");
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn rejects_missing_cert_file() {
        let (_cert, key) = write_self_signed_pair();
        let err = load_tls_config(std::path::Path::new("/nonexistent/cert.pem"), key.path())
            .expect_err("should fail on missing cert");
        assert!(
            err.to_string().contains("No such file") || err.to_string().contains("cannot find")
        );
    }

    #[test]
    fn rejects_empty_cert_file() {
        let cert = NamedTempFile::new().unwrap();
        let (_, key) = write_self_signed_pair();
        let err =
            load_tls_config(cert.path(), key.path()).expect_err("should fail on empty cert PEM");
        assert!(err.to_string().contains("no certificates found"));
    }
}

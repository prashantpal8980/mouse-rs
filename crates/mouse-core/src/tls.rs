//! TLS for the link: a self-signed cert generated fresh on the server each run,
//! and a client that encrypts but does not verify the cert's identity.
//!
//! Identity/authentication is established by the HMAC shared-secret handshake on
//! top of TLS (see `auth`). TLS here provides confidentiality + integrity for a
//! single-LAN, single-user tool — it is deliberately not a CA-backed PKI. The
//! client still verifies the certificate's *signature*, just not *who* it
//! belongs to, exactly matching the Python app's security model.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Install the process-wide ring crypto provider exactly once. Safe to call
/// repeatedly; only the first call wins.
pub fn ensure_crypto_provider() {
    // `install_default` errors if one is already installed — that's fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Build a `TlsAcceptor` (server side) backed by a freshly generated self-signed
/// certificate. A new cert per run is fine — the client doesn't pin identity.
pub fn server_acceptor() -> Result<TlsAcceptor, TlsError> {
    ensure_crypto_provider();

    let cert = rcgen::generate_simple_self_signed(vec!["mouse-rs".to_string()])
        .map_err(|e| TlsError::Cert(e.to_string()))?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| TlsError::Config(e.to_string()))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a `TlsConnector` (client side) that encrypts but accepts any server
/// certificate identity. Signature verification still runs.
pub fn client_connector() -> Result<TlsConnector, TlsError> {
    ensure_crypto_provider();

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerName(provider())))
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

/// A `ServerName` placeholder for the client handshake (identity isn't checked).
pub fn dummy_server_name() -> ServerName<'static> {
    ServerName::try_from("mouse-rs").expect("static name is valid")
}

/// Verifier that accepts any certificate identity but still validates that the
/// handshake signatures are well-formed using the active crypto provider.
#[derive(Debug)]
struct AcceptAnyServerName(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerName {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate generation failed: {0}")]
    Cert(String),
    #[error("tls config error: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_acceptor_and_connector() {
        // Exercises cert generation, provider install, and config building.
        assert!(server_acceptor().is_ok());
        assert!(client_connector().is_ok());
    }
}

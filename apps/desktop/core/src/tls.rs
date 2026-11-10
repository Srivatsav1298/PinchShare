//! TLS helpers: certificate pinning by SHA-256 fingerprint (no CA, no hostname checks).
use std::sync::{Arc, Mutex, Once};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::crypto::sha256_hex;

static INIT: Once = Once::new();

/// Install the ring crypto provider once per process.
pub fn init_crypto() {
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Verifies the server certificate by fingerprint.
/// `expected = Some(fp)`: only that certificate is accepted (normal operation).
/// `expected = None`: any certificate is accepted but its fingerprint is recorded (pairing only;
/// the SPAKE2 confirmation MAC later authenticates the recorded fingerprint).
#[derive(Debug)]
pub struct PinVerifier {
    expected: Option<String>,
    seen: Mutex<Vec<String>>,
    provider: Arc<CryptoProvider>,
}

impl PinVerifier {
    pub fn new(expected: Option<String>) -> Arc<PinVerifier> {
        init_crypto();
        Arc::new(PinVerifier {
            expected,
            seen: Mutex::new(Vec::new()),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        })
    }

    /// The single fingerprint seen across all connections, or None if none/inconsistent.
    pub fn seen_single(&self) -> Option<String> {
        let s = self.seen.lock().unwrap();
        let first = s.first()?;
        s.iter().all(|x| x == first).then(|| first.clone())
    }

    pub fn client_config(self: &Arc<Self>) -> ClientConfig {
        ClientConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS1.3 supported")
            .dangerous()
            .with_custom_certificate_verifier(self.clone())
            .with_no_client_auth()
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = sha256_hex(end_entity.as_ref());
        self.seen.lock().unwrap().push(fp.clone());
        match &self.expected {
            Some(e) if *e != fp => Err(rustls::Error::General("certificate fingerprint mismatch".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

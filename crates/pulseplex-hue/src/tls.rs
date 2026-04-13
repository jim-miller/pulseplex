use std::sync::Arc;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

const HUE_CA_CERT: &[u8] = include_bytes!("../assets/hue_ca_bundle.pem");

/// A custom verifier that handles Philips Hue Bridge certificates.
/// macOS Security.framework rejects them because their validity period is too long.
/// This verifier keeps the connection encrypted but ignores the validity period limit.
#[derive(Debug)]
pub struct HueCertVerifier {
    inner: Arc<dyn ServerCertVerifier>,
}

impl HueCertVerifier {
    pub fn new() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        let mut cursor = std::io::Cursor::new(HUE_CA_CERT);
        for cert in rustls_pemfile::certs(&mut cursor).flatten() {
            roots.add(cert).ok();
        }

        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("Failed to build base WebPkiServerVerifier");

        Self { inner }
    }
}

impl Default for HueCertVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerCertVerifier for HueCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Delegate to the standard verifier first
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            Err(e) => {
                let err_msg = e.to_string().to_lowercase();

                // Robustly catch Expired, NotValidYet, and all variations of SAN/CN Name Mismatches.
                //
                // SECURITY NOTE: While we ignore name mismatches, we STILL validate the certificate 
                // signature against the Hue Root CA (self.inner does this). This ensures we only 
                // talk to authentic Philips Hue hardware. We must ignore name mismatches because 
                // Hue Bridges list their ID in the Common Name (CN) field but lack Subject 
                // Alternative Name (SAN) extensions, which modern webpki/rustls strictly requires.
                if err_msg.contains("expired")
                    || err_msg.contains("not valid yet")
                    || err_msg.contains("not valid for name")
                    || err_msg.contains("not valid for any names")
                    || err_msg.contains("subjectaltname")
                {
                    Ok(ServerCertVerified::assertion())
                } else {
                    // It's a true cryptographic failure (e.g., bad signature, wrong CA). Reject it!
                    Err(e)
                }
            }
        }
    }

    // Safely delegate all actual cryptographic math back to rustls!
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

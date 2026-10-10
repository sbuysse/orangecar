//! TLS client session for Android Auto protocol.
//!
//! Bridge is TLS SERVER (ServerConnection + self-signed cert).
//! We are TLS CLIENT (ClientConnection). We accept any server cert
//! because the bridge uses a self-signed rcgen cert.
//!
//! TLS bytes are exchanged inside SslHandshake messages (type 3) on ch0,
//! not over a real TCP socket — we drive the state machine manually.

use anyhow::Result;
use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use std::io::{Read, Write};
use std::sync::Arc;

/// TLS client session driven manually (no TCP socket).
pub struct AaTlsSession {
    conn: ClientConnection,
    handshake_complete: bool,
}

impl AaTlsSession {
    pub fn new() -> Result<Self> {
        // The ring provider is passed explicitly to builder_with_provider
        // below, so there is no need to install a process-wide default here.
        let config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(|e| anyhow::anyhow!("setting TLS 1.2: {e}"))?
        .dangerous()
        // NoCertVerifier (certificate verification disabled) is INTENTIONAL:
        // this is a protocol-conformance prober talking to the bridge's
        // self-signed cert over a non-TCP transport. Do NOT enable verification
        // — it would break the handshake against the bridge.
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();

        // "rascar" is the SNI — doesn't need to match anything since we skip verification
        let server_name = ServerName::try_from("rascar")
            .map_err(|e| anyhow::anyhow!("invalid server name: {e}"))?
            .to_owned();

        let conn = ClientConnection::new(Arc::new(config), server_name)?;
        Ok(Self { conn, handshake_complete: false })
    }

    /// Feed incoming bytes from bridge; returns outgoing TLS bytes to send.
    pub fn feed(&mut self, incoming: &[u8]) -> Result<Vec<u8>> {
        if !incoming.is_empty() {
            let mut rd = incoming;
            while !rd.is_empty() {
                match self.conn.read_tls(&mut rd) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            self.conn.process_new_packets()?;
        }
        if !self.handshake_complete && !self.conn.is_handshaking() {
            self.handshake_complete = true;
        }
        let mut outgoing = Vec::new();
        self.conn.write_tls(&mut outgoing)?;
        Ok(outgoing)
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.conn.writer().write_all(plaintext)?;
        let mut out = Vec::new();
        self.conn.write_tls(&mut out)?;
        Ok(out)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut rd = ciphertext;
        while !rd.is_empty() {
            match self.conn.read_tls(&mut rd) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.conn.process_new_packets()?;
        let mut plaintext = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            match self.conn.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => plaintext.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(plaintext)
    }

    pub fn is_handshake_complete(&self) -> bool {
        self.handshake_complete
    }
}

/// Accepts any server certificate. Safe here because:
/// - Bridge generates a fresh self-signed cert on first run
/// - We control both endpoints; this is for protocol testing only
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_session_creates_without_error() {
        AaTlsSession::new().unwrap();
    }

    #[test]
    fn tls_session_starts_handshaking() {
        let session = AaTlsSession::new().unwrap();
        assert!(!session.is_handshake_complete());
    }

    #[test]
    fn tls_session_produces_client_hello_on_first_feed() {
        let mut session = AaTlsSession::new().unwrap();
        // Feed empty bytes — rustls should produce a ClientHello
        let output = session.feed(&[]).unwrap();
        assert!(!output.is_empty(), "expected ClientHello bytes");
    }
}

//! TLS client of the head unit, with client authentication.
//!
//! The phone is the TLS SERVER; the head unit is the CLIENT and must present
//! a certificate signed by Google's "Google Automotive Link" CA. Every open
//! source head unit uses the JVC Kenwood certificate + RSA key shipped by
//! opencardev/aasdk (`cert/headunit.crt` / `cert/headunit.key`, valid
//! 2014-07-04 → 2045-04-29). We do NOT embed or commit them: they are read at
//! runtime from disk (see `DEFAULT_CERT_DIRS`). Personal use only.
//!
//! aasdk uses `TLSv1_2_client_method()` (src/Transport/SSLWrapper.cpp), so we
//! pin TLS 1.2. The phone's own certificate is not verified (aasdk does not
//! verify it either); TLS here protects the link, it does not authenticate
//! the phone.
//!
//! The TLS bytes travel inside ENCAPSULATED_SSL messages during the handshake
//! and as per-frame records afterwards, so the session is driven by hand.

use crate::aa::framing::Cipher;
use anyhow::{anyhow, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::ResolvesClientCert;
use rustls::pki_types::pem::PemObject;
use rustls::sign::CertifiedKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Searched in order for `headunit.crt` + `headunit.key`.
pub const DEFAULT_CERT_DIRS: &[&str] = &["/etc/rascar/aa", "/etc/openauto", "/usr/share/aasdk/cert"];

/// Head-unit identity: certificate chain + private key.
pub struct HuIdentity {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub source: PathBuf,
}

impl HuIdentity {
    /// Load `headunit.crt` / `headunit.key` from `dir`.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let crt = dir.join("headunit.crt");
        let key = dir.join("headunit.key");
        let certs = CertificateDer::pem_file_iter(&crt)
            .with_context(|| format!("reading {}", crt.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("parsing {}", crt.display()))?;
        if certs.is_empty() {
            return Err(anyhow!("{} holds no certificate", crt.display()));
        }
        // Accepts PKCS#1 ("BEGIN RSA PRIVATE KEY", what aasdk ships), PKCS#8 and SEC1.
        let key = PrivateKeyDer::from_pem_file(&key).with_context(|| format!("reading {}", key.display()))?;
        Ok(Self { certs, key, source: dir.to_path_buf() })
    }

    /// First directory of `dirs` that holds a loadable identity.
    pub fn discover(dirs: &[PathBuf]) -> Result<Self> {
        let mut errors = Vec::new();
        for d in dirs {
            match Self::from_dir(d) {
                Ok(id) => return Ok(id),
                Err(e) => errors.push(format!("{}: {e:#}", d.display())),
            }
        }
        Err(anyhow!(
            "no head-unit certificate found. Install headunit.crt + headunit.key (setup/aa-headunit-cert.sh). Tried:\n  {}",
            errors.join("\n  ")
        ))
    }
}

/// What the session needs from a TLS client (lets tests swap in a fake).
pub trait TlsClient: Cipher + Send {
    /// Bytes to send first (the ClientHello).
    fn start(&mut self) -> Result<Vec<u8>>;
    /// Feed handshake bytes from the phone; returns bytes to send back.
    fn feed(&mut self, incoming: &[u8]) -> Result<Vec<u8>>;
    fn is_handshaking(&self) -> bool;
}

pub struct HuTls {
    conn: ClientConnection,
}

impl HuTls {
    pub fn new(id: &HuIdentity) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Not `with_client_auth_cert`: it cross-checks key and certificate by
        // parsing the certificate with webpki, which refuses X.509 v1 — and
        // the JVC head-unit certificate IS v1 ("UnsupportedCertVersion",
        // caught by `handshake_with_real_headunit_identity_if_present`). The
        // certificate is only ever sent as opaque bytes, so load the key and
        // pair them without that check.
        let signing_key = provider
            .key_provider
            .load_private_key(id.key.clone_key())
            .map_err(|e| anyhow!("head-unit private key rejected by rustls: {e}"))?;
        let certified = Arc::new(CertifiedKey::new(id.certs.clone(), signing_key));
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(|e| anyhow!("TLS 1.2 config: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyPhoneCert))
            .with_client_cert_resolver(Arc::new(AlwaysPresent(certified)));
        // SNI is irrelevant (no verification); rustls needs a syntactically valid name.
        let name = ServerName::try_from("android.auto").expect("static name").to_owned();
        Ok(Self { conn: ClientConnection::new(Arc::new(config), name)? })
    }

    fn drain_tls(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while self.conn.wants_write() {
            self.conn.write_tls(&mut out)?;
        }
        Ok(out)
    }

    fn absorb(&mut self, mut incoming: &[u8]) -> Result<()> {
        while !incoming.is_empty() {
            if self.conn.read_tls(&mut incoming)? == 0 {
                break;
            }
            self.conn.process_new_packets().map_err(|e| anyhow!("TLS: {e}"))?;
        }
        Ok(())
    }
}

impl Cipher for HuTls {
    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.conn.writer().write_all(plaintext)?;
        self.drain_tls()
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        self.absorb(ciphertext)?;
        let mut plain = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.conn.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => plain.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(plain)
    }
}

impl TlsClient for HuTls {
    fn start(&mut self) -> Result<Vec<u8>> {
        self.drain_tls()
    }

    fn feed(&mut self, incoming: &[u8]) -> Result<Vec<u8>> {
        self.absorb(incoming)?;
        self.drain_tls()
    }

    fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }
}

/// Presents the head-unit certificate whatever CA hints the phone sends.
#[derive(Debug)]
struct AlwaysPresent(Arc<CertifiedKey>);

impl ResolvesClientCert for AlwaysPresent {
    fn resolve(&self, _root_hint_subjects: &[&[u8]], _sigschemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// The phone's certificate is accepted as-is (aasdk does the same).
#[derive(Debug)]
struct AcceptAnyPhoneCert;

impl ServerCertVerifier for AcceptAnyPhoneCert {
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
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    //! The handshake is checked against an in-process rustls SERVER that
    //! demands a client certificate, so the client-auth path (sign the
    //! CertificateVerify with the head-unit key) really runs.
    use super::*;
    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::{DistinguishedName, ServerConfig, ServerConnection};

    /// `rsa_pub`: PKCS#1 public key to check the CertificateVerify with
    /// directly (webpki cannot parse the X.509 v1 JVC certificate, while the
    /// phone's BoringSSL can). `None` = verify through the certificate.
    #[derive(Debug)]
    struct RequireAnyClientCert {
        rsa_pub: Option<Vec<u8>>,
    }
    impl ClientCertVerifier for RequireAnyClientCert {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }
        fn verify_client_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _now: UnixTime,
        ) -> Result<ClientCertVerified, rustls::Error> {
            Ok(ClientCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            // Really verify: proves the client signed with the key matching its cert.
            if let Some(pk) = &self.rsa_pub {
                use ring::signature as sig;
                let alg: &dyn sig::VerificationAlgorithm = match dss.scheme {
                    SignatureScheme::RSA_PKCS1_SHA256 => &sig::RSA_PKCS1_2048_8192_SHA256,
                    SignatureScheme::RSA_PKCS1_SHA384 => &sig::RSA_PKCS1_2048_8192_SHA384,
                    SignatureScheme::RSA_PKCS1_SHA512 => &sig::RSA_PKCS1_2048_8192_SHA512,
                    SignatureScheme::RSA_PSS_SHA256 => &sig::RSA_PSS_2048_8192_SHA256,
                    SignatureScheme::RSA_PSS_SHA384 => &sig::RSA_PSS_2048_8192_SHA384,
                    SignatureScheme::RSA_PSS_SHA512 => &sig::RSA_PSS_2048_8192_SHA512,
                    other => return Err(rustls::Error::General(format!("unexpected scheme {other:?}"))),
                };
                return sig::UnparsedPublicKey::new(alg, pk)
                    .verify(message, dss.signature())
                    .map(|_| HandshakeSignatureValid::assertion())
                    .map_err(|_| rustls::Error::General("bad CertificateVerify signature".into()));
            }
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            _m: &[u8],
            _c: &CertificateDer<'_>,
            _d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("TLS 1.3 not expected".into()))
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }

    fn server(rsa_pub: Option<Vec<u8>>) -> ServerConnection {
        let ck = rcgen::generate_simple_self_signed(vec!["phone".into()]).unwrap();
        let cfg = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .with_client_cert_verifier(Arc::new(RequireAnyClientCert { rsa_pub }))
            .with_single_cert(
                vec![ck.cert.der().clone()],
                PrivateKeyDer::try_from(ck.key_pair.serialize_der()).unwrap(),
            )
            .unwrap();
        ServerConnection::new(Arc::new(cfg)).unwrap()
    }

    fn pump(server: &mut ServerConnection, incoming: &[u8]) -> Vec<u8> {
        let mut rd = incoming;
        while !rd.is_empty() {
            server.read_tls(&mut rd).unwrap();
            server.process_new_packets().unwrap();
        }
        let mut out = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut out).unwrap();
        }
        out
    }

    /// Full handshake + one record each way. Returns the client's cert as seen by the server.
    fn handshake_with(id: &HuIdentity, rsa_pub: Option<Vec<u8>>) -> Vec<u8> {
        let mut hu = HuTls::new(id).unwrap();
        let mut srv = server(rsa_pub);
        let mut to_srv = hu.start().unwrap();
        assert!(!to_srv.is_empty(), "ClientHello expected");
        for _ in 0..10 {
            let to_hu = pump(&mut srv, &to_srv);
            to_srv = hu.feed(&to_hu).unwrap();
            if !hu.is_handshaking() && !srv.is_handshaking() && to_srv.is_empty() {
                break;
            }
        }
        assert!(!hu.is_handshaking() && !srv.is_handshaking(), "handshake did not complete");

        // HU → phone record
        let rec = hu.encrypt(b"\x00\x06hello").unwrap();
        pump(&mut srv, &rec);
        let mut got = vec![0u8; 64];
        let n = srv.reader().read(&mut got).unwrap();
        assert_eq!(&got[..n], b"\x00\x06hello");
        // phone → HU record
        srv.writer().write_all(b"\x00\x05sdr").unwrap();
        let rec = pump(&mut srv, &[]);
        assert_eq!(hu.decrypt(&rec).unwrap(), b"\x00\x05sdr");

        srv.peer_certificates().expect("server saw a client certificate")[0].to_vec()
    }

    #[test]
    fn handshake_presents_client_certificate() {
        let ck = rcgen::generate_simple_self_signed(vec!["hu".into()]).unwrap();
        let id = HuIdentity {
            certs: vec![ck.cert.der().clone()],
            key: PrivateKeyDer::try_from(ck.key_pair.serialize_der()).unwrap(),
            source: PathBuf::from("<rcgen>"),
        };
        let seen = handshake_with(&id, None);
        assert_eq!(seen, ck.cert.der().to_vec());
    }

    /// Same handshake with the real JVC/aasdk RSA identity, when available:
    /// `AA_HU_CERT_DIR=<dir with headunit.crt/.key> cargo test -p rascar-headunit`.
    /// Proves rustls accepts the PKCS#1 key and signs with it.
    #[test]
    fn handshake_with_real_headunit_identity_if_present() {
        let Some(dir) = std::env::var_os("AA_HU_CERT_DIR") else {
            eprintln!("AA_HU_CERT_DIR not set — skipping the real-certificate handshake");
            return;
        };
        let id = HuIdentity::from_dir(Path::new(&dir)).expect("load identity");
        let PrivateKeyDer::Pkcs1(k) = &id.key else { panic!("aasdk ships a PKCS#1 key") };
        let kp = ring::rsa::KeyPair::from_der(k.secret_pkcs1_der()).expect("RSA key");
        let rsa_pub = ring::signature::KeyPair::public_key(&kp).as_ref().to_vec();
        let seen = handshake_with(&id, Some(rsa_pub));
        assert_eq!(seen, id.certs[0].to_vec());
    }

    /// Against OpenSSL's s_server (the phone runs BoringSSL, an OpenSSL fork):
    /// `AA_OPENSSL_TEST=1 AA_HU_CERT_DIR=<dir> cargo test -p rascar-headunit openssl -- --nocapture`.
    /// s_server demands a client certificate (-Verify) and always checks the
    /// CertificateVerify signature; `-www` answers an HTTP GET sent through
    /// our record layer, proving application data flows both ways.
    #[test]
    fn handshake_against_openssl_s_server_if_requested() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        let (Some(dir), Some(_)) = (std::env::var_os("AA_HU_CERT_DIR"), std::env::var_os("AA_OPENSSL_TEST")) else {
            eprintln!("AA_OPENSSL_TEST / AA_HU_CERT_DIR not set — skipping the OpenSSL interop test");
            return;
        };
        let id = HuIdentity::from_dir(Path::new(&dir)).expect("load identity");
        let tmp = std::env::temp_dir().join(format!("rascar-aa-ossl-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let ck = rcgen::generate_simple_self_signed(vec!["phone".into()]).unwrap();
        std::fs::write(tmp.join("srv.crt"), ck.cert.pem()).unwrap();
        std::fs::write(tmp.join("srv.key"), ck.key_pair.serialize_pem()).unwrap();
        let port = 20000 + (std::process::id() % 20000) as u16;
        let mut child = Command::new("openssl")
            .args(["s_server", "-tls1_2", "-Verify", "1", "-www", "-naccept", "1", "-accept"])
            .arg(port.to_string())
            .arg("-cert").arg(tmp.join("srv.crt"))
            .arg("-key").arg(tmp.join("srv.key"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("openssl in PATH");
        let mut tcp = None;
        for _ in 0..50 {
            if let Ok(t) = TcpStream::connect(("127.0.0.1", port)) {
                tcp = Some(t);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut tcp = tcp.expect("s_server did not listen");
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut hu = HuTls::new(&id).unwrap();
        tcp.write_all(&hu.start().unwrap()).unwrap();
        let mut buf = vec![0u8; 65536];
        while hu.is_handshaking() {
            let n = tcp.read(&mut buf).unwrap();
            assert!(n > 0, "s_server closed during the handshake");
            let out = hu.feed(&buf[..n]).unwrap();
            tcp.write_all(&out).unwrap();
        }
        tcp.write_all(&hu.encrypt(b"GET / HTTP/1.0\r\n\r\n").unwrap()).unwrap();
        let mut reply = Vec::new();
        loop {
            match tcp.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => reply.extend(hu.decrypt(&buf[..n]).unwrap_or_default()),
            }
        }
        let _ = child.kill();
        let text = String::from_utf8_lossy(&reply);
        assert!(text.starts_with("HTTP/1.0 200"), "unexpected reply: {text:.200}");
        // s_server's status page lists the client certificate it received.
        assert!(text.contains("JVC Kenwood"), "client certificate not seen by s_server: {text:.2000}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_identity_names_every_directory() {
        let err = HuIdentity::discover(&[PathBuf::from("/nonexistent/a"), PathBuf::from("/nonexistent/b")])
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("/nonexistent/a") && err.contains("/nonexistent/b"), "{err}");
    }
}

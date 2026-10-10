//! Minimal update-server client for the agent.
//!
//! The server is optional and self-hosted; the HTTP contract it must
//! implement is documented in `docs/ota.md`. Connection rules: read
//! `/etc/rascar/update-server.json` ([`ServerJson`]), try `lanUrl` first (short
//! connect timeout, for a server on the home network) and `remoteUrl` when
//! that cannot be reached. `remoteUrl` uses normal TLS verification, or, when
//! `certSha256` is set, accepts exactly that (self-signed) certificate. The
//! agent logs in with the car account (`POST /api/auth/login`) and logs in
//! again once on 401/403 (a permission granted after login only shows up in a
//! fresh token).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

const LAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(20);

/// `/etc/rascar/update-server.json` (root-only: it holds the car's password).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerJson {
    /// Tried first, with a 1.5 s connect timeout (e.g. `http://nas.local`).
    pub lan_url: Option<String>,
    /// Used when `lanUrl` is unset or unreachable (e.g. over 4G).
    pub remote_url: Option<String>,
    /// Hex SHA-256 of the remote server's certificate (DER) to pin instead of
    /// verifying it against the public CAs.
    pub cert_sha256: Option<String>,
    pub username: String,
    pub password: String,
}

pub struct UpdateServer {
    cfg: ServerJson,
    lan: Option<(String, reqwest::Client)>,
    remote: Option<(String, reqwest::Client)>,
    token: Mutex<Option<String>>,
    prefer_remote: Mutex<bool>,
}

/// A request to send; rebuilt for each attempt (the body is plain bytes).
#[derive(Clone)]
pub struct Req {
    pub method: reqwest::Method,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl Req {
    pub fn get(path: impl Into<String>, timeout: Duration) -> Self {
        Self { method: reqwest::Method::GET, path: path.into(), headers: vec![], body: None, timeout }
    }

    pub fn post(path: impl Into<String>, body: Vec<u8>, timeout: Duration) -> Self {
        Self { method: reqwest::Method::POST, path: path.into(), headers: vec![], body: Some(body), timeout }
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
}

impl UpdateServer {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: ServerJson = serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        Self::new(cfg)
    }

    pub fn new(cfg: ServerJson) -> Result<Self> {
        let lan = match &cfg.lan_url {
            Some(u) => Some((
                u.trim_end_matches('/').to_string(),
                reqwest::Client::builder().connect_timeout(LAN_CONNECT_TIMEOUT).build()?,
            )),
            None => None,
        };
        let remote = match (&cfg.remote_url, &cfg.cert_sha256) {
            (Some(u), Some(pin)) => Some((
                u.trim_end_matches('/').to_string(),
                reqwest::Client::builder()
                    .use_preconfigured_tls(pinned_tls(pin)?)
                    .connect_timeout(REMOTE_CONNECT_TIMEOUT)
                    .build()?,
            )),
            (Some(u), None) => Some((
                u.trim_end_matches('/').to_string(),
                reqwest::Client::builder().connect_timeout(REMOTE_CONNECT_TIMEOUT).build()?,
            )),
            (None, _) => None,
        };
        if lan.is_none() && remote.is_none() {
            anyhow::bail!("update server settings have neither lanUrl nor remoteUrl");
        }
        Ok(Self { cfg, lan, remote, token: Mutex::new(None), prefer_remote: Mutex::new(false) })
    }

    async fn order(&self) -> Vec<&(String, reqwest::Client)> {
        let mut v: Vec<&(String, reqwest::Client)> = self.lan.iter().chain(self.remote.iter()).collect();
        if *self.prefer_remote.lock().await {
            v.reverse();
        }
        v
    }

    /// Send `req`, LAN first. Falls back to the other endpoint only when the
    /// connection could not be made. The response is returned whatever its
    /// status (the caller interprets it), except 401/403 which trigger one
    /// fresh login and one retry.
    pub async fn send(&self, req: &Req) -> Result<reqwest::Response> {
        let mut last = anyhow::anyhow!("no update server endpoint");
        for (base, client) in self.order().await {
            match self.send_on(base, client, req).await {
                Ok(r) => {
                    *self.prefer_remote.lock().await = self.remote.as_ref().is_some_and(|(b, _)| b == base);
                    return Ok(r);
                }
                Err(e) if is_connect(&e) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    async fn send_on(&self, base: &str, client: &reqwest::Client, req: &Req) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            // Bound first: a guard in a `match` scrutinee lives for the whole
            // match, and `login` takes the same lock (that deadlocked).
            let cached = self.token.lock().await.clone();
            let token = match cached {
                Some(t) => t,
                None => self.login(base, client).await?,
            };
            let mut b = client.request(req.method.clone(), format!("{base}{}", req.path)).bearer_auth(&token).timeout(req.timeout);
            for (k, v) in &req.headers {
                b = b.header(k, v);
            }
            if let Some(body) = &req.body {
                b = b.body(body.clone());
            }
            let resp = b.send().await?;
            let stale = matches!(resp.status().as_u16(), 401 | 403);
            if stale && attempt == 0 {
                *self.token.lock().await = None;
                continue;
            }
            return Ok(resp);
        }
        unreachable!("the loop always returns on its second pass")
    }

    async fn login(&self, base: &str, client: &reqwest::Client) -> Result<String> {
        #[derive(Deserialize)]
        struct Login {
            token: String,
        }
        let resp = client
            .post(format!("{base}/api/auth/login"))
            .json(&serde_json::json!({ "username": self.cfg.username, "password": self.cfg.password }))
            .timeout(LOGIN_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("update server login failed ({})", resp.status());
        }
        let t = resp.json::<Login>().await?.token;
        *self.token.lock().await = Some(t.clone());
        Ok(t)
    }
}

/// A connection that could not be made (as opposed to one that failed after
/// reaching the server): only these move on to the other endpoint.
pub fn is_connect(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect())
}

// ── Certificate pinning ─────────────────────────────────────────────────────

fn pinned_tls(pin_hex: &str) -> Result<rustls::ClientConfig> {
    let pin: [u8; 32] = crate::util::unhex(pin_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("certSha256 must be 32 bytes"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { pin, provider }))
        .with_no_client_auth())
}

#[derive(Debug)]
struct PinnedCert {
    pin: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref()).as_ref() == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate does not match the pinned update server certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal HTTP/1.1 update server: `/api/auth/login` hands out `t1`, `t2`, …;
    /// `/x` answers 401 to `t1` (expired) and 200 to anything later.
    async fn fake_server(logins: Arc<AtomicUsize>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let logins = logins.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() < i + 4 + len {
                            continue;
                        }
                        let (status, body) = if head.starts_with("post /api/auth/login") {
                            let n = logins.fetch_add(1, Ordering::SeqCst) + 1;
                            ("200 OK", format!("{{\"token\":\"t{n}\"}}"))
                        } else if head.contains("authorization: bearer t1") {
                            ("401 Unauthorized", String::new())
                        } else {
                            ("200 OK", "ok".to_string())
                        };
                        // One request per connection (Connection: close).
                        let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        let _ = s.write_all(resp.as_bytes()).await;
                        return;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn cfg(lan: Option<String>) -> ServerJson {
        ServerJson { lan_url: lan, remote_url: None, cert_sha256: None, username: "car".into(), password: "pw".into() }
    }

    #[tokio::test]
    async fn logs_in_then_logs_in_again_once_on_401() {
        let logins = Arc::new(AtomicUsize::new(0));
        let g = UpdateServer::new(cfg(Some(fake_server(logins.clone()).await))).unwrap();
        let r = tokio::time::timeout(Duration::from_secs(10), g.send(&Req::get("/x", Duration::from_secs(5))))
            .await
            .expect("send must not hang (a lock held across login deadlocked it once)")
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");
        assert_eq!(logins.load(Ordering::SeqCst), 2, "one login, one re-login after the 401");
        // The fresh token is reused: no further login.
        assert_eq!(g.send(&Req::get("/x", Duration::from_secs(5))).await.unwrap().status().as_u16(), 200);
        assert_eq!(logins.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn config_without_any_endpoint_is_refused() {
        assert!(UpdateServer::new(cfg(None)).is_err());
    }

    #[test]
    fn remote_url_without_a_pin_is_accepted() {
        let c = ServerJson { remote_url: Some("https://updates.example.org".into()), ..cfg(None) };
        assert!(UpdateServer::new(c).is_ok());
    }
}

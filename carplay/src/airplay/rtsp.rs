//! RTSP/HTTP-over-TCP client for the AirPlay/CarPlay control channel, plus the
//! TLV8 and binary-plist helpers the pairing / setup messages need.
//!
//! The wire format is RTSP/1.0 request lines (`GET /info`, `POST /pair-setup`,
//! `SETUP rtsp://…`, `RECORD …`) with `CSeq`, an optional binary-plist or
//! octet-stream body, and — once pair-verify completes — HAP ChaCha20-Poly1305
//! frame encryption on the whole byte stream (2-byte LE length prefix as AAD).
//! Ported from doubletake `internal/airplay/client.go`.

use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, trace, warn};

use crate::debug::{hex_dump, truncate_text, wire_trace};
use crate::host::AirPlayEndpoint;

/// Periodic keep-alive URIs whose request/response lines are rate-limited so a
/// 2 s `/feedback` tick does not drown the handshake in the debug log.
const QUIET_URIS: &[&str] = &["/feedback"];

/// Log a request or response head + body at debug; dump the body under wire
/// trace, decoding it as a plist when it is one.
fn log_message(direction: &str, head: &str, content_type: Option<&str>, body: &[u8]) {
    debug!(
        "rtsp {direction} {}\n  (body {} bytes)",
        head.trim_end().replace("\r\n", "\n  "),
        body.len()
    );
    if !wire_trace() || body.is_empty() {
        return;
    }
    // 512 B / 4 KiB used to truncate exactly the message we most need: the car's
    // GET /info response carries `audioFormats` past both caps, so we could not
    // tell from a log whether the audio format we ask for is one it advertises.
    // These only apply when RASCAR_WIRE_TRACE is on, i.e. debug runs at the car.
    debug!("rtsp {direction} body:{}", hex_dump(body, 8192));
    let is_plist = body.starts_with(b"bplist")
        || body.starts_with(b"<?xml")
        || content_type.is_some_and(|c| c.contains("plist"));
    if is_plist {
        match plist::from_bytes::<plist::Value>(body) {
            Ok(v) => debug!("rtsp {direction} body as plist:\n{}", truncate_text(&format!("{v:#?}"), 65536)),
            Err(e) => debug!("rtsp {direction} body looked like a plist but did not decode: {e}"),
        }
    }
}

/// A parsed RTSP/HTTP response.
pub struct RtspResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Extra headers for one request (order-independent — RTSP headers are a map).
pub type Headers = Vec<(&'static str, String)>;

/// How long to wait for a response before releasing the shared connection.
///
/// Apple's receiver uses 10 s for an event reply (`kAirPlayEventTimeoutNS`) and
/// the reference transmitter uses 11 s for an RTSP request, so 10 s is generous
/// for anything the car genuinely intends to answer, while bounding a request it
/// silently ignores.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RtspClient {
    stream: TcpStream,
    peer: SocketAddr,
    cseq: i64,
    user_agent: String,

    encrypted: bool,
    write_key: Option<[u8; 32]>,
    read_key: Option<[u8; 32]>,
    write_nonce: u64,
    read_nonce: u64,

    /// Bytes received from the peer but not yet consumed by a response. Without
    /// this, any surplus the peer sends past the framed body is left in the
    /// socket (or dropped) and the NEXT `read_response` starts mid-message.
    /// In plaintext mode this holds raw socket bytes; once the channel is
    /// HAP-encrypted it holds decrypted frame plaintext.
    rx: Vec<u8>,

    /// Requests sent to a [`QUIET_URIS`] path, for rate-limiting their logs.
    quiet_count: u64,
    /// Whether the in-flight request's response should be logged.
    log_this: bool,
    /// Replies that arrived after their caller gave up (its timeout fired), by
    /// CSeq. Read while waiting for a later request; see [`Self::take_late_reply`].
    late: HashMap<i64, RtspResponse>,
}

/// Late replies kept at most; older ones are dropped.
const MAX_LATE_REPLIES: usize = 8;

/// Ceiling on an RTSP control-message body. The peer's `Content-Length` drives
/// the allocation, so it MUST be bounded: `Content-Length: 18446744073709551615`
/// would abort the process on allocation failure and `4000000000` would get the
/// board OOM-killed. 1 MiB is far above any real pairing/SETUP plist.
const MAX_BODY: usize = 1024 * 1024;
/// Ceiling on the response header block.
const MAX_HEADER: usize = 16 * 1024;

impl RtspClient {
    /// Connect to the discovered AirPlay endpoint. Handles IPv6 link-local
    /// addresses with a `%ifname` (or `%index`) scope suffix, which is the norm
    /// for CarPlay over the CDC-NCM link.
    pub async fn connect(endpoint: &AirPlayEndpoint) -> Result<Self> {
        let addr = resolve_endpoint(&endpoint.address, endpoint.port, &endpoint.interface)
            .with_context(|| format!("resolving AirPlay endpoint {}", endpoint.address))?;
        debug!("rtsp: dialing {} (from mDNS {})", addr, endpoint.address);
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("TCP connect to {addr}"))?;
        Ok(Self::from_stream(stream, addr))
    }

    /// Resolve an endpoint's address string (`fe80::…` / v4) + interface scope to a
    /// `SocketAddr` without connecting. Used by the happy-eyeballs dialer.
    pub fn resolve(address: &str, port: u16, iface: &str) -> Result<SocketAddr> {
        resolve_endpoint(address, port, iface)
    }

    /// Wrap an already-connected TCP stream (from the happy-eyeballs racer).
    pub fn from_stream(stream: TcpStream, addr: SocketAddr) -> Self {
        stream.set_nodelay(true).ok();
        Self {
            stream,
            peer: addr,
            cseq: 0,
            user_agent: "AirPlay/935.7.1".to_string(),
            encrypted: false,
            write_key: None,
            read_key: None,
            write_nonce: 0,
            read_nonce: 0,
            rx: Vec::new(),
            quiet_count: 0,
            log_this: true,
            late: HashMap::new(),
        }
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Override the `User-Agent` sent on every request. The CarPlay legacy path
    /// impersonates a specific iOS AirPlay build (`AirPlay/566.25.21`), distinct
    /// from the AppleTV/FairPlay default.
    pub fn set_user_agent(&mut self, ua: &str) {
        self.user_agent = ua.to_string();
    }

    /// Enable HAP frame encryption after pair-verify. Nonces reset to 0.
    pub fn enable_encryption(&mut self, write_key: [u8; 32], read_key: [u8; 32]) {
        // The carry buffer changes meaning here (raw bytes → decrypted frame
        // plaintext). Anything still in it is a leftover from the plaintext
        // phase and would corrupt the first encrypted response.
        if !self.rx.is_empty() {
            warn!(
                "rtsp: {} unread plaintext byte(s) discarded when enabling HAP encryption — \
                 a previous response was mis-framed",
                self.rx.len()
            );
            self.rx.clear();
        }
        self.write_key = Some(write_key);
        self.read_key = Some(read_key);
        self.write_nonce = 0;
        self.read_nonce = 0;
        self.encrypted = true;
    }

    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// CSeq of the last request sent (the one a timed-out caller abandoned).
    pub fn last_cseq(&self) -> i64 {
        self.cseq
    }

    /// The reply to request `cseq` if it arrived after its caller gave up. It is
    /// picked up while a later request reads its own reply (`/feedback` ticks at
    /// 1 Hz), so it can lag the car by up to one tick.
    pub fn take_late_reply(&mut self, cseq: i64) -> Option<RtspResponse> {
        self.late.remove(&cseq)
    }

    /// Send an RTSP/1.0 request and read the response.
    pub async fn request(
        &mut self,
        method: &str,
        uri: &str,
        content_type: &str,
        body: &[u8],
        extra: &Headers,
    ) -> Result<RtspResponse> {
        self.cseq += 1;
        let seq = self.cseq;

        let mut req = String::new();
        req.push_str(&format!("{method} {uri} RTSP/1.0\r\n"));
        req.push_str(&format!("CSeq: {seq}\r\n"));
        req.push_str(&format!("User-Agent: {}\r\n", self.user_agent));
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !content_type.is_empty() && !body.is_empty() {
            req.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        // Only on a request that actually carries a body. The reference
        // transmitter omits the header entirely for bodyless methods (RECORD,
        // POST /feedback), and a strict Apple-derived parser can treat a
        // `Content-Length` on a bodyless RTSP method as malformed.
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");

        // Periodic keep-alives: log the first 3, then every 30th (and their
        // responses), so the handshake stays readable at debug level.
        self.log_this = if QUIET_URIS.contains(&uri) {
            self.quiet_count += 1;
            crate::debug::sampled(self.quiet_count - 1, 3, 30)
        } else {
            true
        };
        if self.log_this {
            let ct = (!content_type.is_empty()).then_some(content_type);
            log_message(&format!("-> (cseq={seq}, encrypted={})", self.encrypted), &req, ct, body);
        }

        let mut data = req.into_bytes();
        data.extend_from_slice(body);

        trace!(
            "rtsp -> {method} {uri} (body={} bytes, cseq={seq}, encrypted={})",
            body.len(),
            self.encrypted
        );

        let wire = if self.encrypted {
            self.encrypt(&data)
        } else {
            data
        };
        self.stream
            .write_all(&wire)
            .await
            .context("write RTSP request")?;

        // A response read MUST be bounded. The RTSP connection is shared behind a
        // mutex, so a request the car never answers does not merely stall its own
        // caller — it pins the control channel for the rest of the session and
        // starves everything else, including the 1 Hz `/feedback` keepalive that
        // is the ONLY liveness signal a screen-only session has.
        //
        // Measured at the car 2026-09-20 (one capture run): the optional audio
        // SETUP went out as cseq=8 and was never answered; `/feedback` then never
        // ticked once in the whole session and the head unit detached 3.2 s later.
        match tokio::time::timeout(REQUEST_TIMEOUT, self.read_response_for(seq)).await {
            Ok(r) => r,
            Err(_) => {
                warn!(
                    "rtsp: no response to {method} {uri} (cseq={seq}) within {REQUEST_TIMEOUT:?} \
                     — giving up so the shared control channel is released"
                );
                anyhow::bail!("RTSP {method} {uri}: no response within {REQUEST_TIMEOUT:?}")
            }
        }
    }

    /// Read responses until the one for `seq`. A reply carrying an OLDER CSeq
    /// belongs to a request whose caller timed out: hand it to the wrong caller
    /// and every later request on the connection gets its predecessor's answer
    /// (2026-10-01 drive: the car answered the audio SETUP 3.2 s late, after our
    /// 1.5 s timeout, and `/feedback` received the SETUP reply). Set it aside.
    /// A reply with no CSeq, or a newer one, is taken as ours (old behaviour).
    async fn read_response_for(&mut self, seq: i64) -> Result<RtspResponse> {
        loop {
            let resp = self.read_response().await?;
            let got = resp.headers.get("cseq").and_then(|v| v.trim().parse::<i64>().ok());
            match got {
                Some(c) if c < seq => {
                    warn!("rtsp: late reply to cseq={c} (HTTP {}) while waiting for cseq={seq} — set aside", resp.status);
                    if self.late.len() >= MAX_LATE_REPLIES {
                        if let Some(&oldest) = self.late.keys().min() {
                            self.late.remove(&oldest);
                        }
                    }
                    self.late.insert(c, resp);
                }
                _ => return Ok(resp),
            }
        }
    }

    /// Pull more bytes from the transport into the carry buffer.
    async fn fill_more(&mut self) -> Result<()> {
        if self.encrypted {
            let frame = self.read_encrypted_frame().await?;
            self.rx.extend_from_slice(&frame);
        } else {
            let mut buf = [0u8; 4096];
            let n = self
                .stream
                .read(&mut buf)
                .await
                .context("read from control connection")?;
            if n == 0 {
                bail!("connection closed by peer while reading a response");
            }
            self.rx.extend_from_slice(&buf[..n]);
        }
        Ok(())
    }

    /// Read one complete RTSP response, leaving any surplus bytes in `self.rx`
    /// for the next call. Identical for the plaintext and HAP-encrypted paths —
    /// only `fill_more` differs.
    async fn read_response(&mut self) -> Result<RtspResponse> {
        let header_end = loop {
            // Catch a desynchronised stream at the point it happens rather than
            // letting a mid-body offset be parsed as a status line.
            if self.rx.len() >= 5 && !(self.rx.starts_with(b"RTSP/") || self.rx.starts_with(b"HTTP/"))
            {
                bail!(
                    "control connection desynchronised: expected a status line, got {:02x?}… — \
                     a previous response most likely carried a body with no Content-Length",
                    &self.rx[..5.min(self.rx.len())]
                );
            }
            if let Some(pos) = find_subslice(&self.rx, b"\r\n\r\n") {
                break pos + 4;
            }
            if self.rx.len() > MAX_HEADER {
                bail!("response header too large (> {MAX_HEADER} bytes)");
            }
            self.fill_more().await?;
        };

        let header = String::from_utf8_lossy(&self.rx[..header_end]).to_string();
        let (status, headers) = parse_http_header(&header);
        self.rx.drain(..header_end);

        let body_len = match headers.get("content-length") {
            Some(raw) => {
                let n: usize = raw.trim().parse().map_err(|_| {
                    anyhow!("response has an unparseable Content-Length: {raw:?}")
                })?;
                if n > MAX_BODY {
                    bail!(
                        "response Content-Length {n} exceeds the {MAX_BODY}-byte ceiling — \
                         refusing to allocate for it"
                    );
                }
                n
            }
            None => {
                // No Content-Length and bytes already queued means an unframed
                // body: we cannot tell where it ends, so every later request on
                // this connection would read garbage. Fail here, where the cause
                // is still visible.
                if !self.rx.is_empty() {
                    bail!(
                        "response (HTTP {status}) has no Content-Length but {} byte(s) follow \
                         the header — the body is unframed and the control connection cannot be \
                         resynchronised (chunked transfer-encoding is not supported)",
                        self.rx.len()
                    );
                }
                0
            }
        };

        // Read incrementally: never pre-allocate to a peer-supplied length.
        while self.rx.len() < body_len {
            self.fill_more().await?;
        }
        let body: Vec<u8> = self.rx.drain(..body_len).collect();

        if self.log_this {
            log_message(
                &format!("<- (HTTP {status}, encrypted={}, {} surplus byte(s) buffered)", self.encrypted, self.rx.len()),
                &header,
                headers.get("content-type").map(String::as_str),
                &body,
            );
        }

        Ok(RtspResponse {
            status,
            headers,
            body,
        })
    }

    fn encrypt(&mut self, data: &[u8]) -> Vec<u8> {
        let key = self.write_key.expect("write key set when encrypted");
        let cipher = ChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
        let mut out = Vec::with_capacity(data.len() + 32);
        let mut rest = data;
        while !rest.is_empty() {
            let chunk_len = rest.len().min(1024);
            let (chunk, tail) = rest.split_at(chunk_len);
            rest = tail;
            out.extend_from_slice(&seal_control_frame(&cipher, self.write_nonce, chunk));
            self.write_nonce += 1;
        }
        out
    }

    async fn read_encrypted_frame(&mut self) -> Result<Vec<u8>> {
        let mut len_buf = [0u8; 2];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("read frame length")?;
        let plaintext_len = u16::from_le_bytes(len_buf) as usize;
        if plaintext_len == 0 || plaintext_len > 16384 {
            bail!("suspicious HAP frame length {plaintext_len} (expected 1-1024)");
        }
        let mut ciphertext = vec![0u8; plaintext_len + 16];
        self.stream
            .read_exact(&mut ciphertext)
            .await
            .context("read frame ciphertext")?;

        let key = self.read_key.expect("read key set when encrypted");
        let cipher = ChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
        let nonce = hap_frame_nonce(self.read_nonce);
        self.read_nonce += 1;

        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &len_buf,
                },
            )
            .map_err(|_| anyhow!("HAP frame decrypt failed (nonce={})", self.read_nonce - 1))
    }
}

/// HAP control-channel per-frame nonce: `[0;4] ++ LE64(counter)`. Separate tx/rx
/// counters both start at 0 when encryption is enabled.
pub(crate) fn hap_frame_nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// Seal one HAP control-channel frame:
/// `[u16 LE plaintext_len][ciphertext][16-byte Poly1305 tag]`, with the 2-byte
/// length as AAD and the per-frame nonce from [`hap_frame_nonce`]. `chunk` must
/// be at most `u16::MAX` bytes (callers cap at 1024).
pub(crate) fn seal_control_frame(cipher: &ChaCha20Poly1305, counter: u64, chunk: &[u8]) -> Vec<u8> {
    let aad = (chunk.len() as u16).to_le_bytes();
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&hap_frame_nonce(counter)),
            Payload { msg: chunk, aad: &aad },
        )
        .expect("chacha seal");
    let mut out = Vec::with_capacity(2 + ct.len());
    out.extend_from_slice(&aad);
    out.extend_from_slice(&ct);
    out
}

/// Parse the status line + header block. `Content-Length` is deliberately NOT
/// interpreted here: the caller must distinguish "absent" from "present" (an
/// absent one cannot be silently treated as zero — see `read_response`).
fn parse_http_header(header: &str) -> (u16, HashMap<String, String>) {
    let mut status = 0u16;
    let mut headers = HashMap::new();

    let mut lines = header.split("\r\n");
    if let Some(first) = lines.next() {
        // "RTSP/1.0 200 OK" or "HTTP/1.1 200 OK"
        if let Some(code) = first.split_whitespace().nth(1) {
            status = code.parse().unwrap_or(0);
        }
    }
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k.is_empty() {
                continue;
            }
            headers.insert(k, v);
        }
    }
    (status, headers)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Look up a kernel interface index by name.
fn iface_index(name: &str) -> Result<u32> {
    let cname =
        std::ffi::CString::new(name).with_context(|| format!("interface name {name}"))?;
    let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if idx == 0 {
        bail!("if_nametoindex({name}) failed (no such interface?)");
    }
    Ok(idx)
}

/// Resolve an mDNS address string (possibly `fe80::…%ifname`) to a SocketAddr.
///
/// `iface` is the kernel interface the mDNS record was seen on (avahi field 1,
/// carried on [`AirPlayEndpoint::interface`]). An IPv6 link-local address is
/// meaningless without a scope: `TcpStream::connect` on `fe80::…` with
/// `scope_id = 0` fails with EINVAL/ENETUNREACH, so a bare link-local MUST take
/// its scope from the discovering interface. Refusing loudly beats dialing an
/// address that cannot work — that failure aborts Phase 4 before the FairPlay
/// gate is ever reached.
fn resolve_endpoint(address: &str, port: u16, iface: &str) -> Result<SocketAddr> {
    if let Some((addr, scope)) = address.split_once('%') {
        // Explicit `%scope` in the record wins over the discovering interface.
        let ip: Ipv6Addr = addr
            .parse()
            .with_context(|| format!("parse IPv6 {addr}"))?;
        let scope_id = match scope.parse::<u32>() {
            Ok(n) => n,
            Err(_) => iface_index(scope)?,
        };
        Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope_id)))
    } else if let Ok(ip) = address.parse::<Ipv6Addr>() {
        let scope_id = if is_ipv6_link_local(&ip) {
            if iface.is_empty() {
                bail!(
                    "AirPlay endpoint {address} is an IPv6 link-local address but no interface \
                     is known for it, so it cannot be dialed (a link-local needs a scope id). \
                     The mDNS record carried no interface — check that discovery ran against \
                     the CDC-NCM link and that avahi-browse reported the interface in field 1."
                );
            }
            iface_index(iface).with_context(|| {
                format!("scoping link-local {address} to the discovering interface {iface}")
            })?
        } else {
            // A global/ULA IPv6 is routable without a scope.
            0
        };
        Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope_id)))
    } else {
        // IPv4 or hostname:port form.
        format!("{address}:{port}")
            .parse::<SocketAddr>()
            .map_err(|e| anyhow!("parse address {address}:{port}: {e}"))
    }
}

/// True for `fe80::/10` (IPv6 unicast link-local).
fn is_ipv6_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn bare_link_local_without_an_interface_is_refused() {
        // Dialing this would fail with EINVAL/ENETUNREACH deep inside connect();
        // the error must name the real cause instead.
        let err = resolve_endpoint("fe80::1", 7000, "").unwrap_err().to_string();
        assert!(err.contains("link-local"), "{err}");
        assert!(err.contains("interface"), "{err}");
    }

    #[test]
    fn link_local_takes_the_scope_of_the_discovering_interface() {
        // `lo` exists on every Linux host and always has index 1.
        let addr = resolve_endpoint("fe80::1", 7000, "lo").expect("scoped via lo");
        match addr {
            SocketAddr::V6(v6) => assert_ne!(v6.scope_id(), 0),
            other => panic!("expected V6, got {other}"),
        }
    }

    #[test]
    fn global_ipv6_needs_no_scope() {
        let addr = resolve_endpoint("2001:db8::1", 7000, "").expect("global v6");
        match addr {
            SocketAddr::V6(v6) => assert_eq!(v6.scope_id(), 0),
            other => panic!("expected V6, got {other}"),
        }
    }

    #[test]
    fn explicit_percent_scope_still_wins() {
        let addr = resolve_endpoint("fe80::1%1", 7000, "").expect("explicit scope");
        match addr {
            SocketAddr::V6(v6) => assert_eq!(v6.scope_id(), 1),
            other => panic!("expected V6, got {other}"),
        }
    }

    #[test]
    fn ipv4_is_unaffected() {
        let addr = resolve_endpoint("192.0.2.10", 7000, "").expect("v4");
        assert_eq!(addr.port(), 7000);
        assert!(addr.is_ipv4());
    }

    #[test]
    fn control_frame_layout_and_roundtrip() {
        // The exact wire framing the encrypted RTSP control channel uses (and the
        // catplay `AirPlayCipherCodec` interops with): [u16 LE len][ct][16B tag],
        // AAD = the length, nonce = [0;4] ++ LE64(counter).
        let key = [0x42u8; 32];
        let cipher = ChaCha20Poly1305::new_from_slice(&key).unwrap();
        let pt = vec![0xABu8; 40];
        let frame = seal_control_frame(&cipher, 0, &pt);
        assert_eq!(u16::from_le_bytes([frame[0], frame[1]]) as usize, pt.len());
        assert_eq!(frame.len(), 2 + pt.len() + 16, "len prefix + ciphertext + tag");

        let aad = (pt.len() as u16).to_le_bytes();
        let back = cipher
            .decrypt(Nonce::from_slice(&hap_frame_nonce(0)), Payload { msg: &frame[2..], aad: &aad })
            .expect("decrypt frame 0");
        assert_eq!(back, pt);

        // The counter binds the nonce: decrypting frame 0 as if it were frame 1 fails.
        assert!(
            cipher
                .decrypt(Nonce::from_slice(&hap_frame_nonce(1)), Payload { msg: &frame[2..], aad: &aad })
                .is_err(),
            "nonce counter must be authenticated"
        );
    }

    #[test]
    fn hap_frame_nonce_layout() {
        assert_eq!(hap_frame_nonce(0), [0u8; 12]);
        let n1 = hap_frame_nonce(1);
        assert_eq!(&n1[0..4], &[0, 0, 0, 0]);
        assert_eq!(&n1[4..12], &1u64.to_le_bytes());
    }

    #[test]
    fn link_local_detection_covers_the_whole_fe80_10_block() {
        assert!(is_ipv6_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_ipv6_link_local(&"febf::1".parse().unwrap()));
        assert!(!is_ipv6_link_local(&"fec0::1".parse().unwrap()));
        assert!(!is_ipv6_link_local(&"2001:db8::1".parse().unwrap()));
        assert!(!is_ipv6_link_local(&"fd00::1".parse().unwrap()));
    }
}

// ---------------------------------------------------------------------------
// TLV8 (HomeKit-style pairing encoding). Ported from doubletake pairing.go.
// ---------------------------------------------------------------------------

pub mod tlv8 {
    use std::collections::HashMap;

    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0a;
    pub const FLAGS: u8 = 0x13;

    /// Encode TLV8 items in the given order, fragmenting values > 255 bytes.
    pub fn encode(items: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (tag, value) in items {
            if value.is_empty() {
                buf.push(*tag);
                buf.push(0);
                continue;
            }
            let mut rest = value.as_slice();
            while !rest.is_empty() {
                let chunk_len = rest.len().min(255);
                buf.push(*tag);
                buf.push(chunk_len as u8);
                buf.extend_from_slice(&rest[..chunk_len]);
                rest = &rest[chunk_len..];
            }
        }
        buf
    }

    /// Decode TLV8, rejoining fragmented values.
    ///
    /// HAP only continues a value across repeated items when the PREVIOUS item
    /// of that tag was exactly 255 bytes (the maximum an item can carry). Two
    /// logically distinct items that happen to share a tag must therefore stay
    /// distinct — blindly concatenating every repeat would splice unrelated
    /// values together, which matters as soon as a caller validates `STATE`.
    ///
    /// A non-fragment repeat keeps the FIRST occurrence (HAP has no notion of a
    /// later item overriding an earlier one).
    pub fn decode(data: &[u8]) -> HashMap<u8, Vec<u8>> {
        let mut result: HashMap<u8, Vec<u8>> = HashMap::new();
        // Per tag: length of the last item seen. Only 255 means "continues".
        let mut last_len: HashMap<u8, usize> = HashMap::new();
        let mut i = 0;
        while i + 2 <= data.len() {
            let tag = data[i];
            let len = data[i + 1] as usize;
            i += 2;
            if len > data.len() - i {
                break;
            }
            let continues = last_len.get(&tag) == Some(&255);
            match result.entry(tag) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(data[i..i + len].to_vec());
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if continues {
                        e.get_mut().extend_from_slice(&data[i..i + len]);
                    }
                    // else: a separate item with the same tag — first one wins.
                }
            }
            last_len.insert(tag, len);
            i += len;
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip_simple() {
            let items = vec![
                (STATE, vec![0x01]),
                (METHOD, vec![0x00]),
                (PUBLIC_KEY, vec![0xaa; 32]),
            ];
            let enc = encode(&items);
            let dec = decode(&enc);
            assert_eq!(dec[&STATE], vec![0x01]);
            assert_eq!(dec[&METHOD], vec![0x00]);
            assert_eq!(dec[&PUBLIC_KEY], vec![0xaa; 32]);
        }

        #[test]
        fn repeated_tag_that_is_not_a_fragment_stays_separate() {
            // Two short items with the same tag are DISTINCT values, not one
            // fragmented value: the first must not be extended by the second.
            let buf = vec![STATE, 0x01, 0x03, STATE, 0x01, 0x05];
            let dec = decode(&buf);
            assert_eq!(dec[&STATE], vec![0x03]);
        }

        #[test]
        fn fragment_then_unrelated_repeat() {
            // 255-byte item + continuation (rejoined), then a later 1-byte item
            // of the same tag that must NOT be appended.
            let mut buf = Vec::new();
            buf.push(PUBLIC_KEY);
            buf.push(255);
            buf.extend_from_slice(&[0xaa; 255]);
            buf.push(PUBLIC_KEY);
            buf.push(4);
            buf.extend_from_slice(&[0xbb; 4]);
            buf.push(PUBLIC_KEY);
            buf.push(1);
            buf.push(0xcc);
            let dec = decode(&buf);
            assert_eq!(dec[&PUBLIC_KEY].len(), 259);
            assert_eq!(dec[&PUBLIC_KEY][255..], [0xbb; 4]);
        }

        #[test]
        fn fragmentation_over_255() {
            // A 600-byte value must fragment into 255+255+90 and rejoin exactly.
            let big: Vec<u8> = (0..600).map(|i| i as u8).collect();
            let enc = encode(&[(PUBLIC_KEY, big.clone())]);
            // 3 fragments => 3 tag/len headers.
            assert_eq!(enc.len(), big.len() + 6);
            let dec = decode(&enc);
            assert_eq!(dec[&PUBLIC_KEY], big);
        }
    }
}

// ---------------------------------------------------------------------------
// Binary plist helpers (thin wrapper over the `plist` crate).
// ---------------------------------------------------------------------------

pub mod bplist {
    use anyhow::{Context, Result};
    pub use plist::Value;

    /// Serialise a plist value to Apple binary-plist form (`application/x-apple-binary-plist`).
    pub fn to_bytes(value: &Value) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, value).context("encode binary plist")?;
        Ok(buf)
    }

    /// Parse a binary (or XML) plist blob.
    pub fn from_bytes(data: &[u8]) -> Result<Value> {
        plist::from_bytes(data).context("decode plist")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dict_roundtrip() {
            let mut dict = plist::Dictionary::new();
            dict.insert("type".into(), Value::Integer(110.into()));
            dict.insert("name".into(), Value::String("Linux".into()));
            dict.insert("data".into(), Value::Data(vec![1, 2, 3, 4]));
            let v = Value::Dictionary(dict);
            let bytes = to_bytes(&v).unwrap();
            let back = from_bytes(&bytes).unwrap();
            let d = back.as_dictionary().unwrap();
            assert_eq!(d.get("type").unwrap().as_signed_integer(), Some(110));
            assert_eq!(d.get("name").unwrap().as_string(), Some("Linux"));
            assert_eq!(d.get("data").unwrap().as_data(), Some(&[1, 2, 3, 4][..]));
        }
    }
}


#[cfg(test)]
mod request_timeout_tests {
    use super::*;

    /// A peer that ACCEPTS the connection, reads the request, and then NEVER
    /// answers — exactly what the Jaguar did to our audio SETUP (cseq=8) on
    /// 2026-09-20 (one capture run).
    ///
    /// Before `REQUEST_TIMEOUT` existed, `request()` awaited that response
    /// forever. Because the RTSP client is shared behind a mutex, one such
    /// request pinned the whole control channel: `/feedback` — the only liveness
    /// signal a screen-only CarPlay session has — never ticked once for the rest
    /// of the session. This test pins the behaviour that prevents that.
    #[tokio::test(start_paused = true)]
    async fn unanswered_request_times_out_instead_of_hanging_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The silent peer: accept, read whatever arrives, then hold the socket
        // open and say nothing. Holding it open matters — a close would surface
        // as an EOF error and take a different code path.
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
            drop(sock);
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = RtspClient::from_stream(stream, addr);

        let started = tokio::time::Instant::now();
        let result = client
            .request("SETUP", "/", "", &[], &Vec::<(&'static str, String)>::new())
            .await;

        // It must fail rather than hang, and it must do so at the deadline —
        // not merely "eventually".
        let err = match result {
            Ok(_) => panic!("an unanswered request must not succeed"),
            Err(e) => e,
        };
        let waited = started.elapsed();
        assert!(
            waited >= REQUEST_TIMEOUT,
            "returned too early ({waited:?}), so it did not actually wait for the response"
        );
        assert!(
            waited < REQUEST_TIMEOUT * 2,
            "took {waited:?}, far beyond the {REQUEST_TIMEOUT:?} deadline"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no response within"),
            "error should name the timeout, got: {msg}"
        );

        server.abort();
    }

    /// 2026-10-01 drive 2: the car answered the audio SETUP (cseq 6) 3.2 s late,
    /// after the caller's 1.5 s timeout, in the same burst as the reply to the
    /// next request. The next request must get ITS reply, and the late one must
    /// stay retrievable by CSeq.
    #[tokio::test]
    async fn late_reply_is_set_aside_not_handed_to_the_next_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await; // cseq 1: no answer yet
            let _ = sock.read(&mut buf).await; // cseq 2
            let reply = |cseq: u32, body: &str| {
                format!("RTSP/1.0 200 OK\r\nContent-Length: {}\r\nCSeq: {cseq}\r\n\r\n{body}", body.len())
            };
            let burst = reply(1, "setup") + &reply(2, "feedback");
            sock.write_all(burst.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = RtspClient::from_stream(stream, addr);
        let none = Vec::<(&'static str, String)>::new();

        let first = tokio::time::timeout(Duration::from_millis(100), client.request("SETUP", "/", "", &[], &none)).await;
        assert!(first.is_err(), "the server holds the first reply");
        assert_eq!(client.last_cseq(), 1);

        let second = client.request("POST", "/feedback", "", &[], &none).await.unwrap();
        assert_eq!(second.headers.get("cseq").map(String::as_str), Some("2"));
        assert_eq!(second.body, b"feedback");

        let late = client.take_late_reply(1).expect("late reply kept");
        assert_eq!(late.body, b"setup");
        assert!(client.take_late_reply(1).is_none());

        server.abort();
    }
}

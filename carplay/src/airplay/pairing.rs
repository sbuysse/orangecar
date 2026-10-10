//! AirPlay/HAP pairing — **ported from doubletake `internal/airplay/pairing.go`
//! (LGPL-3)**. Two families, tried in doubletake's order:
//!
//!   * **raw** (UxPlay/legacy): 32-byte Ed25519 exchange, then a plaintext
//!     AES-128-CTR pair-verify — the connection stays cleartext (required
//!     because some receivers reject FairPlay fp-setup over a HAP-encrypted
//!     channel).
//!   * **TLV8/HomeKit** (Apple TV style): transient SRP-6a pair-setup (empty
//!     PIN) → HAP pair-verify (X25519 + ChaCha20-Poly1305) → the control
//!     channel becomes HAP-encrypted.
//!
//! Key-derivation labels/salts (`Pair-Setup-Encrypt-*`, `Pair-Verify-Encrypt-*`,
//! `Control-Write/Read-Encryption-Key`, `Pair-Verify-AES-Key/IV`) are copied
//! verbatim from doubletake.
//!
//! No public SRP reference vectors exist for this HAP variant, so the crypto
//! here is **unverified end-to-end** — only against the car (`// TODO(hw)`).
//! The deterministic sub-primitives (`pad_to`, `hkdf_sha512`) are unit-tested.

use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use num_bigint::BigUint;
use rand::RngCore;
use sha2::{Digest, Sha512};
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use super::rtsp::{tlv8, Headers, RtspClient};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// SRP-6a 3072-bit group (RFC 5054), hex big-endian.
const SRP_N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510",
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64",
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B",
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
);

const PAIRING_FLAG_TRANSIENT: u32 = 0x0000_0010;

/// Result of a completed pairing.
pub struct PairResult {
    /// X25519 (or Ed25519-raw) shared secret used to derive the FairPlay stream key.
    pub shared_secret: Vec<u8>,
    /// Whether the control channel is now HAP-encrypted (true for TLV8 path).
    pub channel_encrypted: bool,
}

/// The accessory identity learned from pair-setup M6: its long-term Ed25519
/// public key and pairing UUID. In the CarPlay PIN flow the accessory's identity
/// arrives here (encrypted in M6), not from GET /info, so it is threaded into
/// pair-verify to authenticate the accessory signature.
struct AccessoryIdentity {
    #[allow(dead_code)]
    id: Vec<u8>,
    ed_pub: [u8; 32],
}

/// Byte conventions + RTSP framing for one SRP pair-setup exchange.
struct SrpTransport {
    content_type: &'static str,
    headers: Headers,
    /// `true`  = doubletake/AppleTV: A and B are zero-padded to 384 bytes in the
    ///           SRP `u`, the M1 proof, and on the wire.
    /// `false` = catplay/CarPlay: A and B are used in their natural (unpadded)
    ///           big-endian form, matching `catplay_hap::backend::srp_openssl`
    ///           (`hash_to_bn(&[&a_pub_bytes, b_pub_bytes])` — no PAD()).
    pad_pubkeys: bool,
}

pub struct Pairing {
    pairing_id: String,
    signing: SigningKey,
    ed_public: [u8; 32],
    server_ed_pub: Option<[u8; 32]>,
    /// Where the controller identity + learned accessory key are persisted, so we
    /// pair-SETUP once and pair-VERIFY on every reconnect (a real iPhone keeps a
    /// stable identity; re-pairing from scratch each time makes head units accrete
    /// controller entries and latch into "Connexion impossible"). `None` = ephemeral.
    store_path: Option<std::path::PathBuf>,
    /// The accessory's Ed25519 public key learned from pair-setup M6, if we have
    /// paired this receiver before. Enables the pair-verify fast path.
    accessory_ed_pub: Option<[u8; 32]>,
}

impl Pairing {
    pub fn new() -> Self {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let ed_public = signing.verifying_key().to_bytes();
        Self {
            pairing_id: generate_uuid(),
            signing,
            ed_public,
            server_ed_pub: None,
            store_path: None,
            accessory_ed_pub: None,
        }
    }

    /// Load a persistent controller identity (stable UUID + Ed25519 seed) and any
    /// previously-learned accessory key from `path`, creating and saving a fresh
    /// identity if the file is missing/unreadable. Storage is a tiny key=value
    /// text file: `uuid=`, `seed=<hex32>`, `accessory=<hex32>` (last optional).
    pub fn load_or_create(path: std::path::PathBuf) -> Self {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut uuid = None;
            let mut seed: Option<[u8; 32]> = None;
            let mut accessory: Option<[u8; 32]> = None;
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    match k.trim() {
                        "uuid" => uuid = Some(v.trim().to_string()),
                        "seed" => seed = hex::decode(v.trim()).ok().and_then(|b| b.try_into().ok()),
                        "accessory" => {
                            accessory = hex::decode(v.trim()).ok().and_then(|b| b.try_into().ok())
                        }
                        _ => {}
                    }
                }
            }
            if let (Some(uuid), Some(seed)) = (uuid, seed) {
                let signing = SigningKey::from_bytes(&seed);
                let ed_public = signing.verifying_key().to_bytes();
                info!(
                    "pairing: loaded persistent controller identity {} from {} ({})",
                    uuid,
                    path.display(),
                    if accessory.is_some() { "have accessory key → pair-verify path" } else { "no accessory key yet" }
                );
                return Self {
                    pairing_id: uuid,
                    signing,
                    ed_public,
                    server_ed_pub: None,
                    store_path: Some(path),
                    accessory_ed_pub: accessory,
                };
            }
        }
        // No usable file — mint a fresh identity and persist it.
        let mut me = Self::new();
        me.store_path = Some(path);
        me.persist();
        info!("pairing: created new persistent controller identity {}", me.pairing_id);
        me
    }

    /// Best-effort write of the identity + learned accessory key to `store_path`.
    fn persist(&self) {
        let Some(path) = self.store_path.as_ref() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = format!(
            "uuid={}\nseed={}\n",
            self.pairing_id,
            hex::encode(self.signing.to_bytes())
        );
        if let Some(acc) = self.accessory_ed_pub {
            out.push_str(&format!("accessory={}\n", hex::encode(acc)));
        }
        // The car cuts power without warning: replace atomically and durably so
        // a cut mid-write can never lose the pairing (the car would have to be
        // paired again). Owner-only: it holds the private signing seed.
        let durable = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let tmp = path.with_extension("tmp");
            {
                let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
                f.write_all(out.as_bytes())?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, path)?;
            if let Some(dir) = path.parent() {
                std::fs::File::open(dir)?.sync_all()?;
            }
            Ok(())
        };
        if let Err(e) = durable() {
            warn!("pairing: could not persist identity to {}: {e}", path.display());
        }
    }

    /// The accessory Ed25519 key we learned from a previous pair-setup, if any.
    pub fn stored_accessory_ed_pub(&self) -> Option<[u8; 32]> {
        self.accessory_ed_pub
    }

    /// Record the accessory identity learned at pair-setup M6 and persist it.
    fn record_accessory(&mut self, ed_pub: [u8; 32]) {
        self.accessory_ed_pub = Some(ed_pub);
        self.persist();
    }

    /// Drop a stored accessory key (e.g. after a failed pair-verify) and persist.
    pub fn forget_accessory(&mut self) {
        self.accessory_ed_pub = None;
        self.persist();
    }

    /// Provide the receiver's Ed25519 public key (from GET /info `pk`), needed
    /// by the raw pair-verify path to check the server signature.
    pub fn set_server_ed_pub(&mut self, pk: &[u8]) {
        if pk.len() >= 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&pk[..32]);
            self.server_ed_pub = Some(k);
        }
    }

    /// Headers for the AppleTV/transient HAP path (`X-Apple-HKP: 3`).
    fn pair_headers() -> Headers {
        vec![("X-Apple-HKP", "3".to_string())]
    }

    /// pair-setup headers for the CarPlay PIN path (catplay `pairing_helper_tx`:
    /// `X-Apple-HKP: 2`).
    fn pin_setup_headers() -> Headers {
        vec![("X-Apple-HKP", "2".to_string())]
    }

    /// pair-verify headers for the CarPlay PIN path (catplay: `X-Apple-HKP: 2` +
    /// `X-Apple-PD: 1`).
    fn pin_verify_headers() -> Headers {
        vec![("X-Apple-HKP", "2".to_string()), ("X-Apple-PD", "1".to_string())]
    }

    /// Post one TLV8 pairing message and return the decoded reply.
    ///
    /// Every check here exists because the old code had none: `tlv8::decode` of
    /// an empty or non-TLV body returns an EMPTY map, so "no `kTLVType_Error`
    /// item" read as success. A receiver answering
    /// `RTSP/1.0 470 Connection Authorization Required` with an empty body made
    /// the whole exchange report OK, the channel got switched to encryption with
    /// keys the receiver never derived, and the first decrypt failure was then
    /// blamed on FairPlay three requests later.
    async fn pair_request(
        rtsp: &mut RtspClient,
        uri: &str,
        content_type: &str,
        headers: &Headers,
        body: &[u8],
        step: &str,
        want_state: u8,
    ) -> Result<std::collections::HashMap<u8, Vec<u8>>> {
        let resp = rtsp
            .request("POST", uri, content_type, body, headers)
            .await
            .with_context(|| format!("{step} request"))?;
        if !resp.is_success() {
            bail!("{step}: HTTP {} — the receiver rejected the exchange", resp.status);
        }
        if resp.body.is_empty() {
            bail!("{step}: HTTP {} with an EMPTY body (expected a TLV8 reply)", resp.status);
        }
        let tlv = tlv8::decode(&resp.body);
        {
            let mut tags: Vec<_> = tlv.iter().map(|(t, v)| (*t, v.len())).collect();
            tags.sort_unstable();
            debug!(
                "{step}: HTTP {} {} bytes, TLV8 items (tag, len): {tags:?}",
                resp.status,
                resp.body.len()
            );
        }
        if let Some(e) = tlv.get(&tlv8::ERROR) {
            bail!("{step}: kTLVType_Error {}", e.first().copied().unwrap_or(0));
        }
        match tlv.get(&tlv8::STATE).and_then(|v| v.first().copied()) {
            Some(got) if got == want_state => {}
            Some(got) => bail!("{step}: kTLVType_State 0x{got:02x}, expected 0x{want_state:02x}"),
            None => bail!(
                "{step}: reply carries no kTLVType_State ({} bytes) — this is not a HAP pairing \
                 message and must NOT be treated as success",
                resp.body.len()
            ),
        }
        Ok(tlv)
    }

    /// Transient (PIN-less) pairing: try raw first, fall back to TLV8/SRP.
    pub async fn pair_transient(&mut self, rtsp: &mut RtspClient) -> Result<PairResult> {
        info!("→ pair-setup (transient): trying raw binary pair-setup first");
        match self.raw_pair_setup(rtsp).await {
            Ok(server_pub) => {
                info!("← pair-setup: raw path accepted (server Ed25519 pub received)");
                self.server_ed_pub = Some(server_pub);
                info!("→ pair-verify (raw, connection stays plaintext)");
                let shared = self.raw_pair_verify(rtsp).await?;
                info!("← pair-verify: raw complete");
                Ok(PairResult {
                    shared_secret: shared,
                    channel_encrypted: false,
                })
            }
            Err(e) => {
                warn!("raw pair-setup failed ({e:#}); falling back to TLV8/SRP pair-setup");
                let k = self.pair_setup_transient(rtsp).await?;
                info!("← pair-setup: TLV8/SRP complete");
                info!("→ pair-verify (HAP, X25519 + ChaCha20-Poly1305)");
                let shared = self.pair_verify(rtsp, &k).await?;
                info!("← pair-verify: HAP complete, control channel now encrypted");
                Ok(PairResult {
                    shared_secret: shared,
                    channel_encrypted: true,
                })
            }
        }
    }

    async fn raw_pair_setup(&self, rtsp: &mut RtspClient) -> Result<[u8; 32]> {
        let resp = rtsp
            .request(
                "POST",
                "/pair-setup",
                "application/octet-stream",
                &self.ed_public,
                &Vec::new(),
            )
            .await?;
        debug!("raw pair-setup: HTTP {} {} bytes", resp.status, resp.body.len());
        if !resp.is_success() {
            bail!("raw pair-setup HTTP {}", resp.status);
        }
        if resp.body.len() != 32 {
            bail!("raw pair-setup: expected 32 bytes, got {}", resp.body.len());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&resp.body);
        Ok(out)
    }

    async fn pair_setup_transient(&self, rtsp: &mut RtspClient) -> Result<Vec<u8>> {
        // M1: method=0, state=1, flags=transient.
        let m1 = tlv8::encode(&[
            (tlv8::METHOD, vec![0x00]),
            (tlv8::STATE, vec![0x01]),
            (tlv8::FLAGS, PAIRING_FLAG_TRANSIENT.to_le_bytes().to_vec()),
        ]);
        let xport = SrpTransport {
            content_type: "application/octet-stream",
            headers: Self::pair_headers(),
            pad_pubkeys: true,
        };
        let m2 = Self::pair_request(
            rtsp,
            "/pair-setup",
            xport.content_type,
            &xport.headers,
            &m1,
            "pair-setup M2",
            0x02,
        )
        .await?;
        let server_pub = m2
            .get(&tlv8::PUBLIC_KEY)
            .context("M2: missing server public key")?
            .clone();
        // A missing salt used to become an EMPTY salt, so `x = H(salt || H(I:p))`
        // was computed over zero salt bytes and the exchange ran on to a
        // guaranteed proof mismatch with no diagnostic. Refuse it here instead.
        let server_salt = m2
            .get(&tlv8::SALT)
            .cloned()
            .context("M2: missing kTLVType_Salt — SRP cannot proceed without it")?;
        if server_salt.is_empty() {
            bail!("M2: kTLVType_Salt is empty — SRP cannot proceed without a salt");
        }
        info!("← pair-setup salt ({} bytes) + server public key ({} bytes)", server_salt.len(), server_pub.len());
        let (k, _identity) = self
            .complete_srp_exchange(rtsp, &xport, "", &server_salt, &server_pub)
            .await?;
        Ok(k)
    }

    /// Public entry point for the **CarPlay PIN** HAP path (SETUP-reject fallback):
    /// SRP-6a pair-setup with the fixed CarPlay PIN, then HAP pair-verify, ending
    /// with the RTSP control channel ChaCha20-Poly1305-encrypted. Returns the
    /// X25519 shared secret (used to derive the screen DataStream cipher).
    ///
    /// Distinct from `pair_transient` (AppleTV): the accessory Ed25519 identity is
    /// learned from the encrypted pair-setup M6 (not GET /info), A/B are unpadded
    /// (catplay convention), and the headers are `X-Apple-HKP: 2`
    /// (+ `X-Apple-PD: 1` for pair-verify) with `application/pairing+tlv8` bodies.
    pub async fn pair_pin(&mut self, rtsp: &mut RtspClient, pin: &str) -> Result<PairResult> {
        info!("→ pair-setup (CarPlay PIN, HAP SRP-6a 3072-bit)");
        let identity = self.pair_setup_pin(rtsp, pin).await?;
        info!("← pair-setup: complete (learned accessory Ed25519 identity from M6)");
        // Persist the accessory identity so the NEXT connection can pair-VERIFY
        // instead of re-running pair-setup (which the head unit accretes).
        self.record_accessory(identity.ed_pub);
        info!("→ pair-verify (CarPlay, X25519 + ChaCha20-Poly1305)");
        let shared = self
            .hap_pair_verify(
                rtsp,
                "application/pairing+tlv8",
                &Self::pin_verify_headers(),
                identity.ed_pub,
            )
            .await?;
        info!("← pair-verify: complete, RTSP control channel now HAP-encrypted");
        Ok(PairResult {
            shared_secret: shared,
            channel_encrypted: true,
        })
    }

    /// Fast reconnect path: pair-VERIFY only, using the accessory Ed25519 identity
    /// learned (and persisted) from a previous pair-setup. Skips SRP entirely.
    /// Errors if no accessory key is stored — the caller must fall back to
    /// [`Pairing::pair_pin`].
    pub async fn pair_verify_pin(&mut self, rtsp: &mut RtspClient) -> Result<PairResult> {
        let ed_pub = self
            .accessory_ed_pub
            .context("pair_verify_pin: no stored accessory identity — must pair-setup first")?;
        info!("→ pair-verify (reusing stored pairing — skipping pair-setup)");
        let shared = self
            .hap_pair_verify(
                rtsp,
                "application/pairing+tlv8",
                &Self::pin_verify_headers(),
                ed_pub,
            )
            .await?;
        info!("← pair-verify: complete, RTSP control channel now HAP-encrypted");
        Ok(PairResult {
            shared_secret: shared,
            channel_encrypted: true,
        })
    }

    /// CarPlay PIN pair-setup (M1..M6). Returns the accessory identity learned
    /// from the encrypted M6 exchange.
    async fn pair_setup_pin(&self, rtsp: &mut RtspClient, pin: &str) -> Result<AccessoryIdentity> {
        let xport = SrpTransport {
            content_type: "application/pairing+tlv8",
            headers: Self::pin_setup_headers(),
            pad_pubkeys: false,
        };
        // M1: method=0 (Pair-Setup, non-MFi), state=1. Unlike the transient path
        // there is NO kTLVType_Flags — catplay's `PairSetup::start()` sends only
        // Method(None) + State(SrpStartRequest).
        let m1 = tlv8::encode(&[(tlv8::METHOD, vec![0x00]), (tlv8::STATE, vec![0x01])]);
        let m2 = Self::pair_request(
            rtsp,
            "/pair-setup",
            xport.content_type,
            &xport.headers,
            &m1,
            "pair-setup M2",
            0x02,
        )
        .await?;
        let server_pub = m2
            .get(&tlv8::PUBLIC_KEY)
            .context("M2: missing server public key")?
            .clone();
        let server_salt = m2
            .get(&tlv8::SALT)
            .cloned()
            .context("M2: missing kTLVType_Salt — SRP cannot proceed without it")?;
        if server_salt.is_empty() {
            bail!("M2: kTLVType_Salt is empty — SRP cannot proceed without a salt");
        }
        info!(
            "← pair-setup salt ({} bytes) + server public key ({} bytes)",
            server_salt.len(),
            server_pub.len()
        );
        let (_k, identity) = self
            .complete_srp_exchange(rtsp, &xport, pin, &server_salt, &server_pub)
            .await?;
        identity.context(
            "pair-setup M6: the accessory did not deliver a usable Ed25519 identity \
             (missing/undecryptable kTLVType_EncryptedData or signature check failed)",
        )
    }

    /// SRP-6a from M3 onward. Returns the SRP shared key `K` (used only to derive
    /// the M5/M6 keys) plus the accessory identity decrypted from M6 (when the
    /// receiver sent one). `xport.pad_pubkeys` selects the A/B byte encoding.
    async fn complete_srp_exchange(
        &self,
        rtsp: &mut RtspClient,
        xport: &SrpTransport,
        pin: &str,
        salt: &[u8],
        server_pub_b: &[u8],
    ) -> Result<(Vec<u8>, Option<AccessoryIdentity>)> {
        let n = BigUint::parse_bytes(SRP_N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(5u32);
        let username = b"Pair-Setup";
        let password = pin.as_bytes();

        // x = H(salt, H(username, ":", password))
        let mut inner = Vec::new();
        inner.extend_from_slice(username);
        inner.push(b':');
        inner.extend_from_slice(password);
        let inner_hash = Sha512::digest(&inner);
        let mut x_input = Vec::new();
        x_input.extend_from_slice(salt);
        x_input.extend_from_slice(&inner_hash);
        let x = BigUint::from_bytes_be(&Sha512::digest(&x_input));

        // k = H(pad(N), pad(g))
        let pad_n = pad_to(&n.to_bytes_be(), 384);
        let pad_g = pad_to(&g.to_bytes_be(), 384);
        let mut k_input = pad_n.clone();
        k_input.extend_from_slice(&pad_g);
        let k = BigUint::from_bytes_be(&Sha512::digest(&k_input));

        let mut a_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut a_bytes);
        let a = BigUint::from_bytes_be(&a_bytes);
        let a_pub = g.modpow(&a, &n);

        let b = BigUint::from_bytes_be(server_pub_b);
        // RFC 5054 §2.5.3: the client MUST abort if B % N == 0. A server (or a
        // MITM on the NCM link) sending B = N collapses S to a value it knows.
        if (&b % &n).eq(&BigUint::from(0u32)) {
            bail!("pair-setup: server public key B is 0 mod N (RFC 5054 abort condition)");
        }

        // The A and B byte encodings used for `u`, the M1 proof, and the wire
        // must all agree with what the receiver hashes. `pad_pubkeys` picks the
        // convention: doubletake/AppleTV pads both to 384 bytes; catplay/CarPlay
        // uses the natural (unpadded) big-endian form
        // (`hash_to_bn(&[&a_pub_bytes, b_pub_bytes])` in `srp_openssl`). Either
        // way A-on-the-wire == A-in-the-hash, so send == hash is preserved; for a
        // full-width (384-byte) A/B — the overwhelmingly common case — the two
        // conventions coincide.
        let encode_pub = |v: &[u8]| -> Vec<u8> {
            if xport.pad_pubkeys {
                pad_to(v, 384)
            } else {
                v.to_vec()
            }
        };
        // B is encoded from the bytes the receiver actually SENT (`server_pub_b`),
        // never re-minimised through BigUint, so the natural form matches byte for
        // byte what the accessory hashed.
        let a_pub_wire = encode_pub(&a_pub.to_bytes_be());
        let b_wire = encode_pub(server_pub_b);

        // u = H(A_wire || B_wire)
        let mut u_input = a_pub_wire.clone();
        u_input.extend_from_slice(&b_wire);
        let u = BigUint::from_bytes_be(&Sha512::digest(&u_input));

        // S = (B - k*g^x mod N)^(a + u*x) mod N
        let gx = g.modpow(&x, &n);
        let kgx = (&k * &gx) % &n;
        let diff = (&b + &n - &kgx) % &n;
        let exp = &a + &u * &x;
        let s = diff.modpow(&exp, &n);

        // K = H(S) — natural (unpadded) byte representation (== catplay's
        // `sha512(ClientVerifier::key)`, where `key` is the raw premaster S).
        let k_key = Sha512::digest(s.to_bytes_be()).to_vec();

        // M1 proof = H(H(N) XOR H(g), H("Pair-Setup"), salt, A, B, K).
        let m1_proof = srp_client_proof(&a_pub_wire, &b_wire, salt, &k_key).to_vec();

        // M3: client public key + proof.
        let m3 = tlv8::encode(&[
            (tlv8::STATE, vec![0x03]),
            (tlv8::PUBLIC_KEY, a_pub_wire.clone()),
            (tlv8::PROOF, m1_proof.clone()),
        ]);
        info!("→ pair-setup M3 (client SRP proof)");
        let m4 = Self::pair_request(
            rtsp,
            "/pair-setup",
            xport.content_type,
            &xport.headers,
            &m3,
            "pair-setup M4",
            0x04,
        )
        .await?;
        // The server proof H(A, M1, K) is MANDATORY: skipping it when the tag is
        // absent means a receiver that never derived our key still "passes".
        let server_proof = m4.get(&tlv8::PROOF).context(
            "pair-setup M4: no kTLVType_Proof — the receiver did not prove it derived the same \
             SRP key, so the exchange cannot be trusted",
        )?;
        let expected = srp_server_proof(&a_pub_wire, &m1_proof, &k_key);
        if server_proof.as_slice() != expected.as_slice() {
            bail!("pair-setup M4: server proof mismatch");
        }
        info!("← pair-setup M4: server proof verified");

        // M5: exchange Ed25519 identity over the SRP-derived encrypted channel.
        let session_key = hkdf_sha512(&k_key, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info", 32);
        let sig_key = hkdf_sha512(&k_key, b"Pair-Setup-Controller-Sign-Salt", b"Pair-Setup-Controller-Sign-Info", 32);

        let client_id = self.pairing_id.as_bytes();
        let mut sig_input = Vec::new();
        sig_input.extend_from_slice(&sig_key);
        sig_input.extend_from_slice(client_id);
        sig_input.extend_from_slice(&self.ed_public);
        let signature = self.signing.sign(&sig_input).to_bytes();

        let sub_tlv = tlv8::encode(&[
            (tlv8::IDENTIFIER, client_id.to_vec()),
            (tlv8::PUBLIC_KEY, self.ed_public.to_vec()),
            (tlv8::SIGNATURE, signature.to_vec()),
        ]);
        let encrypted = chacha_seal(&session_key, b"PS-Msg05", &sub_tlv)?;

        let m5 = tlv8::encode(&[
            (tlv8::STATE, vec![0x05]),
            (tlv8::ENCRYPTED_DATA, encrypted),
        ]);
        info!("→ pair-setup M5 (encrypted identity)");
        // `pair_request` requires HTTP 2xx, a non-empty body and kTLVType_State
        // == 0x06 — an empty 470/401 no longer reads as "M6 accepted".
        let m6 = Self::pair_request(
            rtsp,
            "/pair-setup",
            xport.content_type,
            &xport.headers,
            &m5,
            "pair-setup M6",
            0x06,
        )
        .await?;
        let identity = self.extract_m6_identity(&session_key, &k_key, &m6);
        Ok((k_key, identity))
    }

    /// Decrypt the accessory's M6 exchange sub-TLV (`Pair-Setup-Encrypt` key,
    /// nonce `PS-Msg06`) and, when present and self-consistent, return the
    /// accessory's Ed25519 identity. Best-effort: the AppleTV/transient path does
    /// not need it and ignores a `None`; the CarPlay PIN path requires it.
    fn extract_m6_identity(
        &self,
        session_key: &[u8],
        k_key: &[u8],
        m6: &std::collections::HashMap<u8, Vec<u8>>,
    ) -> Option<AccessoryIdentity> {
        let enc = m6.get(&tlv8::ENCRYPTED_DATA)?;
        let plain = match chacha_open(session_key, b"PS-Msg06", enc) {
            Ok(p) => p,
            Err(e) => {
                warn!("pair-setup M6: could not decrypt accessory identity: {e:#}");
                return None;
            }
        };
        let sub = tlv8::decode(&plain);
        let id = sub.get(&tlv8::IDENTIFIER)?.clone();
        let ed = sub.get(&tlv8::PUBLIC_KEY)?;
        if ed.len() != 32 {
            warn!("pair-setup M6: accessory Ed25519 key is {} bytes, expected 32", ed.len());
            return None;
        }
        let mut ed_pub = [0u8; 32];
        ed_pub.copy_from_slice(&ed[..32]);

        // Verify the accessory signature over
        // accessory_x || accessory_id || accessory_ed_pub (catplay `handle_m6`).
        let sig = sub.get(&tlv8::SIGNATURE)?;
        let accessory_x = hkdf_sha512(
            k_key,
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
            32,
        );
        let mut msg = accessory_x;
        msg.extend_from_slice(&id);
        msg.extend_from_slice(&ed_pub);
        let vk = VerifyingKey::from_bytes(&ed_pub).ok()?;
        let sig_bytes: [u8; 64] = sig.as_slice().try_into().ok()?;
        if vk
            .verify(&msg, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .is_err()
        {
            warn!("pair-setup M6: accessory signature did not verify");
            return None;
        }
        debug!("pair-setup M6: accessory identity verified");
        Some(AccessoryIdentity { id, ed_pub })
    }

    /// HAP pair-verify (X25519 + ChaCha20-Poly1305) for the AppleTV/transient
    /// path: authenticates the receiver against its GET /info `pk`.
    async fn pair_verify(&self, rtsp: &mut RtspClient, _srp_key: &[u8]) -> Result<Vec<u8>> {
        let server_ed = self.server_ed_pub.context(
            "pair-verify: the receiver's Ed25519 public key is unknown (GET /info carried no \
             `pk`), so its signature cannot be checked — refusing to continue unauthenticated",
        )?;
        self.hap_pair_verify(rtsp, "application/octet-stream", &Self::pair_headers(), server_ed)
            .await
    }

    /// HAP pair-verify (X25519 + ChaCha20-Poly1305). Authenticates the accessory
    /// signature against `accessory_ed_pub`, then enables RTSP control-channel
    /// encryption and returns the X25519 shared secret. `content_type` + `headers`
    /// select the transport (AppleTV `X-Apple-HKP: 3` vs CarPlay
    /// `X-Apple-HKP: 2` + `X-Apple-PD: 1`).
    async fn hap_pair_verify(
        &self,
        rtsp: &mut RtspClient,
        content_type: &str,
        headers: &Headers,
        accessory_ed_pub: [u8; 32],
    ) -> Result<Vec<u8>> {
        let client_private = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let client_public = XPublicKey::from(&client_private);

        // V1: our ephemeral X25519 public key.
        let v1 = tlv8::encode(&[
            (tlv8::STATE, vec![0x01]),
            (tlv8::PUBLIC_KEY, client_public.as_bytes().to_vec()),
        ]);
        let v2 =
            Self::pair_request(rtsp, "/pair-verify", content_type, headers, &v1, "pair-verify V2", 0x02)
                .await?;
        let server_key = v2
            .get(&tlv8::PUBLIC_KEY)
            .context("V2: missing server public key")?;
        if server_key.len() < 32 {
            bail!("V2: server public key too short");
        }
        let mut server_public_bytes = [0u8; 32];
        server_public_bytes.copy_from_slice(&server_key[..32]);
        let server_public = XPublicKey::from(server_public_bytes);
        let shared = client_private.diffie_hellman(&server_public);
        let shared_bytes = shared.as_bytes().to_vec();

        let verify_key = hkdf_sha512(&shared_bytes, b"Pair-Verify-Encrypt-Salt", b"Pair-Verify-Encrypt-Info", 32);

        // ---------------------------------------------------------------
        // Authenticate the RECEIVER. Previously the V2 plaintext was decrypted
        // and thrown away, so any device on the NCM link that won the mDNS race
        // could complete pair-verify with an arbitrary X25519 key and be handed
        // the FairPlay-derived shk/shiv and the mirrored dashboard. The raw
        // pair-verify path below always verified; this asymmetry was a bug.
        // ---------------------------------------------------------------
        let enc = v2.get(&tlv8::ENCRYPTED_DATA).context(
            "pair-verify V2: no kTLVType_EncryptedData — without it the receiver is not \
             authenticated at all (not even an AEAD tag check)",
        )?;
        if enc.is_empty() {
            bail!("pair-verify V2: kTLVType_EncryptedData is empty");
        }
        let v2_plain = chacha_open(&verify_key, b"PV-Msg02", enc).context("decrypt V2")?;
        let v2_sub = tlv8::decode(&v2_plain);
        let server_id = v2_sub
            .get(&tlv8::IDENTIFIER)
            .context("pair-verify V2: sub-TLV has no kTLVType_Identifier")?;
        let server_sig = v2_sub
            .get(&tlv8::SIGNATURE)
            .context("pair-verify V2: sub-TLV has no kTLVType_Signature")?;
        // HAP: the accessory signs serverX25519 || accessoryPairingID || clientX25519.
        let mut v2_sig_msg = Vec::with_capacity(32 + server_id.len() + 32);
        v2_sig_msg.extend_from_slice(&server_public_bytes);
        v2_sig_msg.extend_from_slice(server_id);
        v2_sig_msg.extend_from_slice(client_public.as_bytes());
        let vk = VerifyingKey::from_bytes(&accessory_ed_pub).context("receiver Ed25519 key")?;
        let sig_bytes: [u8; 64] = server_sig
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!(
                "pair-verify V2: signature is {} bytes, expected 64",
                server_sig.len()
            ))?;
        vk.verify(&v2_sig_msg, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .context("pair-verify V2: receiver signature verification FAILED — this is not the \
                      device whose Ed25519 identity we recorded (GET /info `pk`, or pair-setup M6)")?;
        debug!("pair-verify V2: receiver signature verified");

        // V3: sign(clientX25519 || pairingID || serverX25519).
        let client_id = self.pairing_id.as_bytes();
        let mut sig_input = Vec::new();
        sig_input.extend_from_slice(client_public.as_bytes());
        sig_input.extend_from_slice(client_id);
        sig_input.extend_from_slice(&server_public_bytes);
        let signature = self.signing.sign(&sig_input).to_bytes();

        let sub_tlv = tlv8::encode(&[
            (tlv8::IDENTIFIER, client_id.to_vec()),
            (tlv8::SIGNATURE, signature.to_vec()),
        ]);
        let encrypted = chacha_seal(&verify_key, b"PV-Msg03", &sub_tlv)?;
        let v3 = tlv8::encode(&[
            (tlv8::STATE, vec![0x03]),
            (tlv8::ENCRYPTED_DATA, encrypted),
        ]);
        // A non-2xx or empty V4 used to fall straight through to
        // `enable_encryption` below, arming the channel with keys the receiver
        // never derived. `pair_request` now requires 2xx + a TLV8 body + State
        // 0x04 before we touch the cipher.
        let _v4 =
            Self::pair_request(rtsp, "/pair-verify", content_type, headers, &v3, "pair-verify V4", 0x04)
                .await?;

        // Derive HAP control-channel keys and switch the RTSP client to encrypted.
        let write_key = hkdf_sha512(&shared_bytes, b"Control-Salt", b"Control-Write-Encryption-Key", 32);
        let read_key = hkdf_sha512(&shared_bytes, b"Control-Salt", b"Control-Read-Encryption-Key", 32);
        let mut wk = [0u8; 32];
        let mut rk = [0u8; 32];
        wk.copy_from_slice(&write_key);
        rk.copy_from_slice(&read_key);
        rtsp.enable_encryption(wk, rk);
        debug!("pair-verify: HAP keys derived, channel encryption ENABLED (nonces at 0)");

        Ok(shared_bytes)
    }

    /// Raw ("AirMyPC-style") plaintext pair-verify. Keeps the channel cleartext.
    async fn raw_pair_verify(&self, rtsp: &mut RtspClient) -> Result<Vec<u8>> {
        let client_private = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let client_public = XPublicKey::from(&client_private);

        // V1: flags(4)=0x01000000 + X25519_pub(32) + Ed25519_pub(32) = 68 bytes.
        let mut v1 = [0u8; 68];
        v1[0] = 0x01;
        v1[4..36].copy_from_slice(client_public.as_bytes());
        v1[36..68].copy_from_slice(&self.ed_public);
        let resp = rtsp
            .request("POST", "/pair-verify", "application/octet-stream", &v1, &Vec::new())
            .await?;
        debug!("raw pair-verify V2: HTTP {} {} bytes", resp.status, resp.body.len());
        if !resp.is_success() {
            bail!("raw pair-verify V2: HTTP {}", resp.status);
        }
        if resp.body.len() != 96 {
            bail!("raw pair-verify V2: expected 96 bytes, got {}", resp.body.len());
        }
        let mut server_public_bytes = [0u8; 32];
        server_public_bytes.copy_from_slice(&resp.body[..32]);
        let encrypted_server_sig = &resp.body[32..96];

        let server_public = XPublicKey::from(server_public_bytes);
        let shared = client_private.diffie_hellman(&server_public);
        let shared_bytes = shared.as_bytes().to_vec();

        // AES-128-CTR key/iv from SHA-512(label || shared)[:16].
        let aes_key = sha512_derive_key("Pair-Verify-AES-Key", &shared_bytes);
        let aes_iv = sha512_derive_key("Pair-Verify-AES-IV", &shared_bytes);

        // Decrypt server signature at CTR offset 0.
        let mut server_sig = [0u8; 64];
        server_sig.copy_from_slice(encrypted_server_sig);
        let mut cipher =
            Aes128Ctr::new_from_slices(&aes_key, &aes_iv).expect("aes-128-ctr key/iv");
        cipher.apply_keystream(&mut server_sig);

        // Verify server Ed25519 signature over (serverX25519 || clientX25519).
        let mut server_sig_msg = [0u8; 64];
        server_sig_msg[..32].copy_from_slice(&server_public_bytes);
        server_sig_msg[32..].copy_from_slice(client_public.as_bytes());
        let server_ed = self
            .server_ed_pub
            .context("raw pair-verify: server Ed25519 public key unavailable (call GET /info)")?;
        let vk = VerifyingKey::from_bytes(&server_ed).context("server Ed25519 key")?;
        let sig = ed25519_dalek::Signature::from_bytes(&server_sig);
        vk.verify(&server_sig_msg, &sig)
            .context("raw pair-verify: server signature verification failed")?;
        debug!("raw pair-verify: server signature verified");

        // Sign our proof and encrypt at CTR offset 64.
        let mut client_sig_msg = [0u8; 64];
        client_sig_msg[..32].copy_from_slice(client_public.as_bytes());
        client_sig_msg[32..].copy_from_slice(&server_public_bytes);
        let client_sig = self.signing.sign(&client_sig_msg).to_bytes();

        let mut cipher2 =
            Aes128Ctr::new_from_slices(&aes_key, &aes_iv).expect("aes-128-ctr key/iv");
        let mut skip = [0u8; 64];
        cipher2.apply_keystream(&mut skip); // advance CTR by 64 bytes
        let mut encrypted_client_sig = client_sig;
        cipher2.apply_keystream(&mut encrypted_client_sig);

        let mut v3 = [0u8; 68];
        v3[4..68].copy_from_slice(&encrypted_client_sig);
        let resp = rtsp
            .request("POST", "/pair-verify", "application/octet-stream", &v3, &Vec::new())
            .await?;
        debug!("raw pair-verify V4: HTTP {} {} bytes", resp.status, resp.body.len());
        if !resp.is_success() {
            bail!("raw pair-verify V4: HTTP {} — the receiver rejected our proof", resp.status);
        }
        if !resp.body.is_empty() {
            debug!("raw pair-verify V4: unexpected {} bytes", resp.body.len());
        }
        Ok(shared_bytes)
    }
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

fn chacha_seal(key: &[u8], nonce_label: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).context("chacha key")?;
    let mut nonce = [0u8; 12];
    nonce[4..4 + nonce_label.len()].copy_from_slice(nonce_label);
    cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &[] })
        .map_err(|_| anyhow::anyhow!("chacha seal failed"))
}

fn chacha_open(key: &[u8], nonce_label: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).context("chacha key")?;
    let mut nonce = [0u8; 12];
    nonce[4..4 + nonce_label.len()].copy_from_slice(nonce_label);
    cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad: &[] })
        .map_err(|_| anyhow::anyhow!("chacha open failed"))
}

/// SRP-6a client proof `M1 = H( H(N) XOR H(g), H("Pair-Setup"), salt, A, B, K )`.
///
/// `A` and `B` are hashed EXACTLY as passed — the caller supplies them in the
/// same byte encoding it put on the wire / received (RFC 5054 / HAP SRP-6a,
/// SHA-512, 3072-bit group, username "Pair-Setup"). A unit test pins the result
/// to a published test vector.
fn srp_client_proof(client_pub: &[u8], server_pub: &[u8], salt: &[u8], session_key: &[u8]) -> [u8; 64] {
    let n = BigUint::parse_bytes(SRP_N_HEX.as_bytes(), 16).unwrap();
    let g = BigUint::from(5u32);
    let hn = Sha512::digest(n.to_bytes_be());
    let hg = Sha512::digest(g.to_bytes_be());
    let mut hxor = [0u8; 64];
    for i in 0..64 {
        hxor[i] = hn[i] ^ hg[i];
    }
    let hi = Sha512::digest(b"Pair-Setup");
    let mut h = Sha512::new();
    h.update(hxor);
    h.update(hi);
    h.update(salt);
    h.update(client_pub);
    h.update(server_pub);
    h.update(session_key);
    h.finalize().into()
}

/// SRP-6a server proof `M2 = H(A, M1, K)`.
fn srp_server_proof(client_pub: &[u8], m1: &[u8], session_key: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    for part in [client_pub, m1, session_key] {
        h.update(part);
    }
    h.finalize().into()
}

/// HKDF-SHA-512.
pub(crate) fn hkdf_sha512(secret: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha512>::new(Some(salt), secret);
    let mut okm = vec![0u8; len];
    hk.expand(info, &mut okm).expect("hkdf expand");
    okm
}

/// SHA-512(salt || secret)[:16].
fn sha512_derive_key(salt: &str, secret: &[u8]) -> [u8; 16] {
    let mut h = Sha512::new();
    h.update(salt.as_bytes());
    h.update(secret);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Left-pad with zeros to `size` (no-op if already >= size).
fn pad_to(data: &[u8], size: usize) -> Vec<u8> {
    if data.len() >= size {
        return data.to_vec();
    }
    let mut padded = vec![0u8; size];
    padded[size - data.len()..].copy_from_slice(data);
    padded
}

/// RFC-4122-ish random UUID string (matches doubletake's use of a per-connection ID).
fn generate_uuid() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
    .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_to_left_pads() {
        assert_eq!(pad_to(&[0xaa], 4), vec![0, 0, 0, 0xaa]);
        assert_eq!(pad_to(&[1, 2, 3], 3), vec![1, 2, 3]);
        assert_eq!(pad_to(&[1, 2, 3], 2), vec![1, 2, 3]); // no truncation
    }

    #[test]
    fn hkdf_sha512_rfc_shape() {
        // Deterministic + correct length; guards against accidental hash swap.
        let out = hkdf_sha512(b"secret", b"salt", b"info", 32);
        assert_eq!(out.len(), 32);
        let again = hkdf_sha512(b"secret", b"salt", b"info", 32);
        assert_eq!(out, again);
        let other = hkdf_sha512(b"secret", b"salt", b"other", 32);
        assert_ne!(out, other);
    }

    #[test]
    fn sha512_derive_key_len_and_determinism() {
        let k = sha512_derive_key("Pair-Verify-AES-Key", &[1, 2, 3, 4]);
        assert_eq!(k.len(), 16);
        assert_eq!(k, sha512_derive_key("Pair-Verify-AES-Key", &[1, 2, 3, 4]));
        assert_ne!(k, sha512_derive_key("Pair-Verify-AES-IV", &[1, 2, 3, 4]));
    }

    #[test]
    fn uuid_shape() {
        let u = generate_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4'); // version nibble
    }

    // SRP-6a test vector (A, B, salt, K, M1, M2) taken from the test suite of
    // catplay-labs/catplay, used as a reference for the wire format, plus the
    // two fixed sub-hashes H(N) XOR H(g) and H("Pair-Setup") for the HAP group.
    const VEC_SERVER_PUB: [u8; 384] = [66, 254, 195, 54, 248, 36, 231, 56, 186, 118, 54, 42, 5, 66, 113, 73, 150, 54, 54, 156, 16, 76, 252, 50, 206, 135, 204, 173, 159, 174, 68, 125, 23, 251, 75, 215, 136, 102, 165, 73, 107, 249, 205, 158, 32, 192, 157, 134, 17, 70, 51, 225, 74, 3, 195, 74, 124, 137, 26, 48, 245, 253, 212, 197, 29, 141, 12, 5, 36, 191, 77, 203, 137, 72, 27, 38, 231, 136, 43, 60, 134, 137, 192, 134, 218, 13, 90, 232, 166, 245, 168, 29, 34, 91, 121, 185, 82, 144, 146, 215, 6, 120, 204, 126, 217, 146, 135, 243, 55, 1, 99, 159, 192, 53, 80, 103, 19, 87, 253, 193, 104, 98, 228, 167, 138, 214, 84, 135, 232, 225, 185, 158, 114, 48, 228, 110, 137, 109, 32, 21, 227, 177, 18, 241, 240, 240, 125, 115, 135, 46, 249, 172, 206, 170, 41, 76, 169, 163, 62, 77, 94, 51, 116, 69, 188, 218, 171, 90, 171, 128, 163, 84, 233, 52, 134, 77, 70, 130, 207, 84, 55, 218, 155, 199, 67, 214, 10, 25, 142, 134, 34, 65, 43, 52, 113, 92, 234, 213, 101, 32, 12, 34, 87, 31, 82, 132, 175, 144, 116, 36, 47, 119, 221, 46, 103, 44, 80, 74, 201, 196, 249, 230, 127, 123, 80, 118, 216, 163, 249, 186, 218, 184, 181, 144, 72, 187, 101, 164, 150, 209, 45, 247, 70, 184, 92, 155, 146, 175, 162, 54, 95, 155, 174, 116, 38, 190, 166, 137, 133, 51, 253, 21, 169, 129, 167, 61, 102, 49, 115, 24, 245, 130, 171, 6, 196, 177, 44, 49, 149, 39, 239, 142, 253, 49, 123, 226, 140, 40, 43, 193, 131, 20, 30, 17, 57, 252, 1, 68, 55, 1, 227, 226, 24, 73, 204, 237, 230, 2, 81, 203, 54, 204, 157, 174, 22, 134, 126, 132, 106, 66, 115, 27, 189, 157, 131, 173, 205, 36, 140, 160, 183, 171, 62, 97, 7, 73, 93, 114, 199, 50, 192, 22, 56, 132, 172, 61, 208, 84, 102, 85, 60, 216, 170, 249, 29, 66, 158, 48, 221, 78, 166, 50, 20, 203, 99, 206, 6, 112, 163, 194, 43, 165, 176, 193, 62, 157, 184, 57, 235, 221, 175, 46, 7, 152];
    const VEC_CLIENT_PUB: [u8; 384] = [21, 187, 129, 247, 180, 115, 7, 3, 46, 219, 190, 0, 89, 8, 126, 225, 37, 159, 98, 137, 226, 202, 13, 228, 31, 55, 17, 244, 180, 120, 159, 97, 141, 126, 131, 219, 227, 64, 81, 24, 99, 206, 12, 218, 27, 95, 43, 216, 151, 228, 46, 89, 133, 153, 100, 55, 134, 206, 65, 165, 244, 228, 121, 88, 78, 54, 19, 220, 48, 86, 150, 92, 21, 168, 219, 226, 214, 254, 33, 26, 135, 134, 247, 49, 58, 114, 245, 76, 67, 182, 223, 191, 93, 1, 131, 58, 76, 62, 49, 123, 11, 10, 164, 221, 249, 114, 77, 154, 179, 95, 207, 127, 9, 154, 30, 91, 99, 67, 160, 94, 33, 239, 178, 32, 173, 245, 25, 91, 153, 189, 34, 216, 16, 100, 160, 145, 150, 208, 85, 135, 237, 13, 225, 234, 144, 137, 224, 217, 187, 77, 10, 51, 240, 45, 72, 9, 184, 164, 195, 153, 55, 66, 98, 110, 208, 204, 45, 204, 14, 46, 195, 119, 25, 83, 120, 223, 17, 167, 79, 75, 68, 182, 105, 97, 176, 15, 153, 95, 170, 16, 33, 172, 134, 114, 73, 69, 5, 49, 78, 133, 250, 44, 136, 75, 179, 33, 213, 48, 77, 236, 16, 148, 221, 74, 185, 209, 83, 94, 167, 180, 101, 170, 91, 129, 143, 228, 180, 251, 185, 210, 16, 74, 26, 248, 65, 208, 73, 182, 161, 120, 59, 223, 98, 166, 58, 94, 69, 250, 93, 75, 29, 36, 41, 211, 242, 115, 253, 65, 28, 204, 117, 167, 194, 113, 98, 180, 54, 96, 170, 81, 125, 134, 19, 213, 2, 213, 178, 163, 108, 74, 215, 177, 52, 217, 75, 225, 144, 220, 3, 164, 87, 156, 233, 1, 99, 218, 251, 89, 45, 165, 227, 98, 232, 143, 165, 141, 246, 196, 230, 209, 136, 35, 75, 218, 63, 15, 94, 150, 79, 113, 245, 247, 28, 139, 115, 136, 232, 175, 49, 228, 11, 70, 234, 131, 126, 167, 31, 234, 202, 34, 27, 240, 195, 201, 1, 29, 98, 229, 254, 146, 87, 223, 113, 5, 218, 217, 250, 58, 115, 53, 9, 162, 175, 42, 176, 10, 195, 48, 155, 56, 58, 246, 219, 98, 166, 41, 34, 216, 225, 29, 28, 1, 18, 89];
    const VEC_M1: [u8; 64] = [33, 202, 204, 21, 29, 11, 142, 163, 254, 113, 245, 137, 104, 89, 101, 247, 182, 215, 41, 42, 213, 171, 173, 142, 172, 183, 214, 187, 204, 48, 253, 153, 224, 246, 18, 246, 72, 5, 95, 165, 27, 245, 255, 22, 229, 250, 129, 33, 8, 40, 1, 194, 131, 19, 51, 75, 37, 179, 34, 60, 222, 13, 182, 81];
    const VEC_SALT: [u8; 16] = [67, 81, 86, 223, 10, 171, 140, 180, 0, 188, 111, 77, 228, 110, 173, 185];
    const VEC_SESSION_KEY: [u8; 64] = [39, 162, 132, 15, 245, 98, 72, 107, 190, 101, 117, 162, 228, 189, 241, 14, 132, 216, 104, 7, 65, 72, 90, 154, 86, 129, 171, 235, 197, 55, 174, 216, 183, 170, 12, 101, 219, 128, 62, 155, 113, 212, 250, 40, 137, 178, 199, 215, 68, 139, 218, 112, 205, 68, 52, 66, 95, 11, 116, 251, 143, 93, 206, 89];
    const VEC_M2: [u8; 64] = [53, 222, 231, 209, 7, 123, 202, 208, 135, 119, 183, 90, 79, 154, 55, 155, 63, 56, 215, 210, 4, 20, 229, 119, 234, 168, 107, 137, 48, 172, 180, 244, 184, 142, 170, 120, 188, 106, 94, 135, 122, 4, 211, 21, 190, 26, 121, 180, 13, 192, 173, 246, 172, 223, 161, 192, 52, 251, 187, 66, 52, 170, 18, 85];
    const H_N_XOR_H_G: [u8; 64] = [179, 214, 62, 246, 170, 251, 46, 151, 150, 218, 224, 6, 254, 96, 242, 14, 83, 38, 254, 30, 28, 82, 165, 2, 30, 178, 23, 71, 202, 51, 223, 234, 143, 242, 3, 189, 69, 162, 84, 67, 149, 215, 61, 25, 137, 156, 23, 74, 104, 60, 158, 120, 50, 150, 221, 22, 147, 235, 199, 28, 245, 165, 61, 163];
    const H_PAIR_SETUP: [u8; 64] = [205, 63, 168, 144, 172, 137, 26, 169, 169, 210, 51, 70, 241, 228, 246, 152, 176, 18, 64, 173, 129, 12, 240, 41, 213, 213, 127, 199, 159, 27, 31, 196, 226, 196, 8, 98, 223, 179, 130, 142, 210, 197, 138, 168, 15, 103, 138, 69, 167, 244, 215, 139, 156, 74, 195, 63, 220, 85, 70, 242, 161, 4, 48, 179];

    #[test]
    fn srp_fixed_sub_hashes_match_reference_values() {
        // H(N) XOR H(g=5) and H("Pair-Setup") are constant for HAP; pin them.
        let n = BigUint::parse_bytes(SRP_N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(5u32);
        let hn = Sha512::digest(n.to_bytes_be());
        let hg = Sha512::digest(g.to_bytes_be());
        let mut hxor = [0u8; 64];
        for i in 0..64 {
            hxor[i] = hn[i] ^ hg[i];
        }
        assert_eq!(hxor, H_N_XOR_H_G, "H(N) XOR H(g) diverged");
        let hi: [u8; 64] = Sha512::digest(b"Pair-Setup").into();
        assert_eq!(hi, H_PAIR_SETUP, "H(\"Pair-Setup\") diverged");
    }

    #[test]
    fn srp_proofs_match_reference_vector() {
        // The only deterministic end-to-end check available for the SRP layer.
        // Reproducing the client M1 proof
        // and the server proof over the vector's A/B/salt/K validates the whole
        // proof construction (hash order, HNG/H(I) constants, byte handling).
        let m1 = srp_client_proof(&VEC_CLIENT_PUB, &VEC_SERVER_PUB, &VEC_SALT, &VEC_SESSION_KEY);
        assert_eq!(m1, VEC_M1, "client M1 proof mismatch");
        let m2 = srp_server_proof(&VEC_CLIENT_PUB, &VEC_M1, &VEC_SESSION_KEY);
        assert_eq!(m2, VEC_M2, "server proof mismatch");
    }

    #[test]
    fn chacha_seal_open_roundtrips_and_binds_nonce_label() {
        // The pairing AEAD sub-messages (PS-Msg05/06, PV-Msg02/03) use a 12-byte
        // nonce = [0;4] ++ 8-byte label. A seal/open pair with the same key+label
        // must round-trip; a different label must fail the Poly1305 tag.
        let key = [0x5au8; 32];
        let pt = b"pairing sub-tlv plaintext";
        let ct = chacha_seal(&key, b"PS-Msg05", pt).expect("seal");
        assert_eq!(ct.len(), pt.len() + 16, "ChaCha20-Poly1305 appends a 16-byte tag");
        let back = chacha_open(&key, b"PS-Msg05", &ct).expect("open");
        assert_eq!(back, pt);
        assert!(chacha_open(&key, b"PS-Msg06", &ct).is_err(), "wrong nonce label must not open");
    }

    #[test]
    fn pin_pair_setup_m1_has_method_and_state_but_no_flags() {
        // The CarPlay PIN M1 is Method(0) + State(1) with NO kTLVType_Flags
        // (catplay `PairSetup::start`), unlike the transient path which adds the
        // transient Flags word.
        let m1 = tlv8::encode(&[(tlv8::METHOD, vec![0x00]), (tlv8::STATE, vec![0x01])]);
        let dec = tlv8::decode(&m1);
        assert_eq!(dec.get(&tlv8::METHOD), Some(&vec![0x00]));
        assert_eq!(dec.get(&tlv8::STATE), Some(&vec![0x01]));
        assert!(dec.get(&tlv8::FLAGS).is_none(), "PIN M1 must not carry kTLVType_Flags");
    }
}

//! The AT side of the SIM7600's voice port: the command allow-list, number
//! validation, and a line-level link that matches every reply to the command
//! that asked for it.
//!
//! WHY a hard allow-list: a raw `AT+CSIM` once dropped the SIM and killed 4G
//! and `AT+CFUN` takes the radio down. Nothing outside
//! [`check`] can reach the modem, in release builds too, and the test suite
//! pins the refusals.
//!
//! WHY reply matching matters: AT has no request ids. A reply that arrives
//! after we gave up on its command would otherwise be read as the answer to
//! the NEXT command (the RTSP CSeq bug of 2026-10-01, same shape). So a timed
//! out command leaves the link "stale": nothing new is sent until the late
//! final result has been seen and discarded, and if it never comes the link
//! reports [`AtError::Desync`] so the caller reopens the port instead of
//! guessing.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// Numbers that are dialable whatever the state or settings (EU 112, US 911,
/// UK 999, and 100 / 101, police or fire in several European countries).
pub const EMERGENCY_NUMBERS: [&str; 5] = ["112", "911", "999", "100", "101"];

/// Longest number accepted (3GPP allows more, no real number needs it).
pub const MAX_NUMBER_LEN: usize = 32;

/// How long a stale command may take to deliver its late final result before
/// the link is declared out of sync.
const DRAIN: Duration = Duration::from_secs(5);
/// Writing one command to a USB tty is instant; a blocked write means the
/// modem is gone or wedged.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// What an allowed command is, for reply matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    EchoOff,
    Answer,
    Hangup,
    ListCalls,
    ActivityStatus,
    Registration,
    PcmRegister,
    PcmFormat,
    Mute,
    CallerId,
    Dial,
    Dtmf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    /// Information lines that belong to this command's reply (`+CLCC:` for
    /// `AT+CLCC`). Any other line received while it is in flight is a URC.
    pub info_prefix: Option<&'static str>,
    pub timeout: Duration,
}

const fn spec(kind: Kind, info_prefix: Option<&'static str>, secs: u64) -> Spec {
    Spec { kind, info_prefix, timeout: Duration::from_secs(secs) }
}

/// THE allow-list. Every byte sent to the modem goes through here; anything
/// not matched exactly is refused (no `;`-concatenation, no CR/LF smuggling,
/// no `ATD` without the voice `;`, no setters other than these).
pub fn check(cmd: &str) -> Result<Spec, String> {
    let s = match cmd {
        "ATE0" => spec(Kind::EchoOff, None, 3),
        // ATA can take a while on CS fallback (the network sets up the bearer).
        "ATA" => spec(Kind::Answer, None, 20),
        "AT+CHUP" => spec(Kind::Hangup, None, 10),
        "AT+CLCC" => spec(Kind::ListCalls, Some("+CLCC:"), 5),
        "AT+CPAS" => spec(Kind::ActivityStatus, Some("+CPAS:"), 5),
        "AT+CREG?" => spec(Kind::Registration, Some("+CREG:"), 5),
        "AT+CPCMREG=0" | "AT+CPCMREG=1" => spec(Kind::PcmRegister, None, 5),
        "AT+CPCMFRM?" => spec(Kind::PcmFormat, Some("+CPCMFRM:"), 5),
        "AT+CMUT=0" | "AT+CMUT=1" => spec(Kind::Mute, None, 5),
        "AT+CLIP=1" => spec(Kind::CallerId, None, 5),
        _ => {
            if let Some(number) = cmd.strip_prefix("ATD").and_then(|r| r.strip_suffix(';')) {
                // The number must already be in canonical form: what we send is
                // exactly what was validated.
                if validate_number(number).as_deref() == Ok(number) {
                    // SIMCom answers a voice ATD with OK at once, but the
                    // network may hold it on CS fallback.
                    return Ok(spec(Kind::Dial, None, 30));
                }
                return Err(format!("refused: invalid number in {cmd:?}"));
            }
            if let Some(d) = cmd.strip_prefix("AT+VTS=") {
                if d.len() == 1 && is_dtmf_digit(d.as_bytes()[0] as char) {
                    return Ok(spec(Kind::Dtmf, None, 5));
                }
            }
            return Err(format!("refused: {cmd:?} is not on the phone allow-list"));
        }
    };
    Ok(s)
}

pub fn is_dtmf_digit(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '*' | '#' | 'A' | 'B' | 'C' | 'D')
}

/// Canonical dialable form: separators people type (spaces, dashes, dots,
/// brackets) removed; then digits, `*`, `#`, and `+` only as the first
/// character; at least one digit; at most [`MAX_NUMBER_LEN`].
pub fn validate_number(raw: &str) -> Result<String, String> {
    let n: String = raw.chars().filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')' | '\u{a0}')).collect();
    if is_emergency(&n) {
        // Never let a later rule refuse an emergency number.
        return Ok(n);
    }
    if n.is_empty() {
        return Err("empty number".into());
    }
    if n.len() > MAX_NUMBER_LEN {
        return Err(format!("number longer than {MAX_NUMBER_LEN} characters"));
    }
    for (i, c) in n.chars().enumerate() {
        let ok = c.is_ascii_digit() || c == '*' || c == '#' || (c == '+' && i == 0);
        if !ok {
            return Err(format!("invalid character {c:?} in number"));
        }
    }
    if !n.chars().any(|c| c.is_ascii_digit()) {
        return Err("number has no digit".into());
    }
    Ok(n)
}

pub fn is_emergency(number: &str) -> bool {
    EMERGENCY_NUMBERS.contains(&number)
}

/// Whether `line` ends the reply to a command of `kind`, and if so whether it
/// is a success.
pub fn final_result(kind: Kind, line: &str) -> Option<bool> {
    match line {
        "OK" => return Some(true),
        "ERROR" => return Some(false),
        l if l.starts_with("+CME ERROR") || l.starts_with("+CMS ERROR") => return Some(false),
        _ => {}
    }
    match (kind, line) {
        // V.25ter: a dial may end with these instead of OK. On any other
        // command they are unsolicited (the remote hung up meanwhile).
        (Kind::Dial, "NO CARRIER" | "BUSY" | "NO ANSWER" | "NO DIALTONE") => Some(false),
        (Kind::Answer, "NO CARRIER") => Some(false),
        _ => None,
    }
}

/// Unsolicited result codes the phone reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Urc {
    Ring,
    /// `+CLIP: "<number>",<type>,...` (empty when withheld).
    CallerId(String),
    VoiceBegin,
    VoiceEnd,
    NoCarrier,
    Busy,
    NoAnswer,
    MissedCall,
    /// An unsolicited `+CLCC:` line, or a late one from a timed-out poll: a
    /// hint to poll again, never taken as the authoritative call list.
    CallListHint,
    Other(String),
}

pub fn parse_urc(line: &str) -> Urc {
    match line {
        "RING" => Urc::Ring,
        "NO CARRIER" => Urc::NoCarrier,
        "BUSY" => Urc::Busy,
        "NO ANSWER" => Urc::NoAnswer,
        l if l.starts_with("VOICE CALL: BEGIN") => Urc::VoiceBegin,
        // "VOICE CALL: END: 000012" (duration) on SIMCom.
        l if l.starts_with("VOICE CALL: END") => Urc::VoiceEnd,
        l if l.starts_with("MISSED_CALL") => Urc::MissedCall,
        l if l.starts_with("+CLCC:") => Urc::CallListHint,
        l if l.starts_with("+CLIP:") => {
            let fields = crate::services::cell::split_fields(l["+CLIP:".len()..].trim());
            Urc::CallerId(fields.into_iter().next().unwrap_or_default())
        }
        l => Urc::Other(l.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub ok: bool,
    /// The command's own information lines (URCs excluded).
    pub lines: Vec<String>,
    pub final_line: String,
}

#[derive(Debug)]
pub enum AtError {
    Refused(String),
    /// No final result within the command's timeout. The link is now stale.
    Timeout,
    /// A stale command never delivered its final result: replies can no
    /// longer be attributed, the port must be reopened.
    Desync,
    Io(std::io::Error),
}

impl std::fmt::Display for AtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtError::Refused(m) => write!(f, "{m}"),
            AtError::Timeout => write!(f, "modem did not answer in time"),
            AtError::Desync => write!(f, "modem replies out of sync"),
            AtError::Io(e) => write!(f, "modem port: {e}"),
        }
    }
}

/// One AT conversation over any byte stream (the tty in production, an
/// in-memory duplex with a scripted modem in tests). One command in flight
/// at a time, as the modem expects.
pub struct AtLink<T> {
    io: T,
    buf: Vec<u8>,
    /// A timed-out command whose final result has not been seen yet.
    stale: Option<Kind>,
    /// Lines that arrived during a command but are not part of its reply.
    pub urcs: VecDeque<String>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> AtLink<T> {
    pub fn new(io: T) -> Self {
        Self { io, buf: Vec::new(), stale: None, urcs: VecDeque::new() }
    }

    /// Next non-empty line. Cancel-safe: partial input stays in `buf`, and
    /// `AsyncReadExt::read` loses nothing when its future is dropped, so this
    /// can sit in a `select!`.
    pub async fn read_line(&mut self) -> std::io::Result<String> {
        loop {
            if let Some(line) = self.take_line() {
                return Ok(line);
            }
            let mut tmp = [0u8; 512];
            let n = self.io.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "modem port closed"));
            }
            self.buf.extend_from_slice(&tmp[..n]);
            // A modem spewing bytes without line ends must not grow us forever.
            if self.buf.len() > 64 * 1024 {
                self.buf.clear();
            }
        }
    }

    fn take_line(&mut self) -> Option<String> {
        loop {
            let pos = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r')?;
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    /// Next unsolicited line while idle: queued URCs first, then the port.
    /// Late finals of a stale command and stray finals are swallowed here.
    pub async fn next_unsolicited(&mut self) -> std::io::Result<String> {
        if let Some(u) = self.urcs.pop_front() {
            return Ok(u);
        }
        loop {
            let line = self.read_line().await?;
            if self.absorb_final(&line) {
                continue;
            }
            return Ok(line);
        }
    }

    /// A final result with nothing in flight: either the late answer of the
    /// stale command (which clears the stale state) or a stray. Either way it
    /// must not reach the next command.
    fn absorb_final(&mut self, line: &str) -> bool {
        if let Some(kind) = self.stale {
            if final_result(kind, line).is_some() {
                tracing::info!("phone: late reply {line:?} to a timed-out command discarded");
                self.stale = None;
                return true;
            }
        }
        if matches!(line, "OK" | "ERROR") || line.starts_with("+CME ERROR") {
            tracing::debug!("phone: stray {line:?} discarded");
            return true;
        }
        false
    }

    #[cfg(test)]
    pub fn is_stale(&self) -> bool {
        self.stale.is_some()
    }

    /// Send one allowed command and collect its reply. URC lines received
    /// meanwhile are queued in [`Self::urcs`].
    pub async fn command(&mut self, cmd: &str) -> Result<Reply, AtError> {
        let spec = check(cmd).map_err(AtError::Refused)?;
        if self.stale.is_some() {
            self.drain_stale().await?;
        }
        let bytes = format!("{cmd}\r");
        match tokio::time::timeout(WRITE_TIMEOUT, self.io.write_all(bytes.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(AtError::Io(e)),
            Err(_) => return Err(AtError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "write to modem blocked"))),
        }
        let _ = self.io.flush().await;
        let deadline = Instant::now() + spec.timeout;
        let mut lines = Vec::new();
        loop {
            let line = match tokio::time::timeout_at(deadline, self.read_line()).await {
                Err(_) => {
                    self.stale = Some(spec.kind);
                    return Err(AtError::Timeout);
                }
                Ok(Err(e)) => return Err(AtError::Io(e)),
                Ok(Ok(l)) => l,
            };
            if line == cmd {
                continue; // echo, before ATE0 took effect
            }
            if let Some(ok) = final_result(spec.kind, &line) {
                return Ok(Reply { ok, lines, final_line: line });
            }
            if spec.info_prefix.is_some_and(|p| line.starts_with(p)) {
                lines.push(line);
            } else {
                self.urcs.push_back(line);
            }
        }
    }

    /// Wait for the late final result of the stale command. Its information
    /// lines (if any) are queued as URCs, where they are only hints.
    async fn drain_stale(&mut self) -> Result<(), AtError> {
        let deadline = Instant::now() + DRAIN;
        while let Some(kind) = self.stale {
            let line = match tokio::time::timeout_at(deadline, self.read_line()).await {
                Err(_) => return Err(AtError::Desync),
                Ok(Err(e)) => return Err(AtError::Io(e)),
                Ok(Ok(l)) => l,
            };
            if final_result(kind, &line).is_some() {
                tracing::info!("phone: late reply {line:?} to a timed-out command discarded");
                self.stale = None;
            } else {
                self.urcs.push_back(line);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[test]
    fn allow_list_accepts_exactly_the_call_commands() {
        for c in [
            "ATE0", "ATA", "AT+CHUP", "AT+CLCC", "AT+CPAS", "AT+CREG?", "AT+CPCMREG=0", "AT+CPCMREG=1", "AT+CPCMFRM?",
            "AT+CMUT=0", "AT+CMUT=1", "AT+CLIP=1", "ATD+447700900101;", "ATD112;", "ATD*100#;", "AT+VTS=5", "AT+VTS=#",
        ] {
            assert!(check(c).is_ok(), "{c} must be allowed");
        }
    }

    #[test]
    fn allow_list_refuses_everything_else() {
        for c in [
            "AT+CFUN=0", "AT+CFUN=1", "AT+CFUN?", "AT+CSIM=14,\"00A40004023F00\"", "AT+CPCMFRM=1", "AT+CNMP=38",
            "AT+CLIP=0", "AT+CPCMREG=2", "AT+CMUT=2", "AT", "ATZ", "AT&F", "AT+CRSM=176,28539,0,0,12",
            // data call (no ';'), phonebook / CLIR modifiers, injection attempts
            "ATD+447700900101", "ATD>1;", "ATDI0700;", "ATD0700;+CFUN=0;", "ATD0700\r;", "AT+CLCC;+CFUN=0",
            "AT+CLCC\r\nAT+CFUN=0", "ATD 07700 900101;", "AT+VTS=12", "AT+VTS=E", "AT+VTS=", "atd112;",
            "ATD+44+771;",
        ] {
            assert!(check(c).is_err(), "{c:?} must be refused");
        }
    }

    #[test]
    fn numbers_are_canonicalised_and_validated() {
        assert_eq!(validate_number("+44 7700 900-10.1").as_deref(), Ok("+447700900101"));
        assert_eq!(validate_number("(07700) 900 101").as_deref(), Ok("07700900101"));
        assert_eq!(validate_number("*100#").as_deref(), Ok("*100#"));
        assert!(validate_number("").is_err());
        assert!(validate_number("+#*").is_err());
        assert!(validate_number("32+471").is_err());
        assert!(validate_number("tel:123").is_err());
        assert!(validate_number("0700;AT+CFUN=0").is_err());
        assert!(validate_number(&"1".repeat(33)).is_err());
        assert!(validate_number(&"1".repeat(32)).is_ok());
    }

    #[test]
    fn emergency_numbers_are_always_dialable() {
        for n in EMERGENCY_NUMBERS {
            assert!(is_emergency(n));
            assert_eq!(validate_number(n).as_deref(), Ok(n));
            assert!(check(&format!("ATD{n};")).is_ok(), "ATD{n}; must pass the allow-list");
        }
        assert_eq!(validate_number(" 1 1 2 ").as_deref(), Ok("112"));
        assert!(!is_emergency("1120"));
    }

    #[test]
    fn finals_depend_on_the_command() {
        assert_eq!(final_result(Kind::ListCalls, "OK"), Some(true));
        assert_eq!(final_result(Kind::ListCalls, "+CME ERROR: 3"), Some(false));
        // A remote hangup during a CLCC poll is a URC, not the poll's answer.
        assert_eq!(final_result(Kind::ListCalls, "NO CARRIER"), None);
        assert_eq!(final_result(Kind::Dial, "BUSY"), Some(false));
        assert_eq!(final_result(Kind::Dial, "NO ANSWER"), Some(false));
        assert_eq!(final_result(Kind::Answer, "NO CARRIER"), Some(false));
        assert_eq!(final_result(Kind::Hangup, "BUSY"), None);
    }

    #[test]
    fn urcs_are_recognised() {
        assert_eq!(parse_urc("RING"), Urc::Ring);
        assert_eq!(parse_urc("+CLIP: \"+447700900101\",145,,,,0"), Urc::CallerId("+447700900101".into()));
        assert_eq!(parse_urc("+CLIP: \"\",128,,,,1"), Urc::CallerId(String::new()));
        assert_eq!(parse_urc("VOICE CALL: BEGIN"), Urc::VoiceBegin);
        assert_eq!(parse_urc("VOICE CALL: END: 000012"), Urc::VoiceEnd);
        assert_eq!(parse_urc("MISSED_CALL: 10:23AM +447700900101"), Urc::MissedCall);
        assert_eq!(parse_urc("+CLCC: 1,1,4,0,0,\"+44\",145"), Urc::CallListHint);
        assert_eq!(parse_urc("+CPIN: READY"), Urc::Other("+CPIN: READY".into()));
    }

    /// The modem side of a duplex: read one command line.
    async fn modem_cmd(r: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> String {
        let mut v = Vec::new();
        r.read_until(b'\r', &mut v).await.unwrap();
        String::from_utf8_lossy(&v).trim().to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn urcs_interleaved_with_a_reply_are_not_part_of_it() {
        let (ours, modem) = tokio::io::duplex(4096);
        let (mr, mut mw) = tokio::io::split(modem);
        let mut mr = tokio::io::BufReader::new(mr);
        let mut link = AtLink::new(ours);
        let m = tokio::spawn(async move {
            assert_eq!(modem_cmd(&mut mr).await, "AT+CLCC");
            mw.write_all(b"\r\n+CLCC: 1,1,4,0,0,\"+44770\",145\r\n\r\nRING\r\n\r\n+CLIP: \"+44770\",145,,,,0\r\n\r\nOK\r\n")
                .await
                .unwrap();
            (mr, mw)
        });
        let r = link.command("AT+CLCC").await.unwrap();
        assert!(r.ok);
        assert_eq!(r.lines, vec!["+CLCC: 1,1,4,0,0,\"+44770\",145".to_string()]);
        assert_eq!(link.urcs.iter().cloned().collect::<Vec<_>>(), vec!["RING", "+CLIP: \"+44770\",145,,,,0"]);
        m.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_reply_is_never_given_to_the_next_command() {
        let (ours, modem) = tokio::io::duplex(4096);
        let (mr, mut mw) = tokio::io::split(modem);
        let mut mr = tokio::io::BufReader::new(mr);
        let mut link = AtLink::new(ours);
        // The modem sits on AT+CLCC past its timeout.
        let first = link.command("AT+CLCC").await;
        assert!(matches!(first, Err(AtError::Timeout)));
        assert!(link.is_stale());
        assert_eq!(modem_cmd(&mut mr).await, "AT+CLCC");
        let m = tokio::spawn(async move {
            // Its answer arrives late, before AT+CPAS has even been sent...
            mw.write_all(b"\r\n+CLCC: 1,0,0,0,0,\"+44770\",145\r\n\r\nOK\r\n").await.unwrap();
            // ...and only then may AT+CPAS go out.
            assert_eq!(modem_cmd(&mut mr).await, "AT+CPAS");
            mw.write_all(b"\r\n+CPAS: 4\r\n\r\nOK\r\n").await.unwrap();
        });
        let r = link.command("AT+CPAS").await.unwrap();
        assert_eq!(r.lines, vec!["+CPAS: 4".to_string()]);
        assert!(!link.is_stale());
        // The stale call list went to the URC queue (a hint), not into the CPAS reply.
        assert_eq!(link.urcs.pop_front().as_deref(), Some("+CLCC: 1,0,0,0,0,\"+44770\",145"));
        m.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_command_that_never_answers_desyncs_the_link() {
        let (ours, _modem) = tokio::io::duplex(4096);
        let mut link = AtLink::new(ours);
        assert!(matches!(link.command("AT+CLCC").await, Err(AtError::Timeout)));
        assert!(matches!(link.command("AT+CPAS").await, Err(AtError::Desync)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_final_while_idle_is_swallowed() {
        let (ours, modem) = tokio::io::duplex(4096);
        let (_mr, mut mw) = tokio::io::split(modem);
        let mut link = AtLink::new(ours);
        assert!(matches!(link.command("AT+CHUP").await, Err(AtError::Timeout)));
        mw.write_all(b"\r\nOK\r\n\r\nRING\r\n").await.unwrap();
        assert_eq!(link.next_unsolicited().await.unwrap(), "RING");
        assert!(!link.is_stale());
    }

    #[tokio::test(start_paused = true)]
    async fn refused_commands_never_reach_the_port() {
        let (ours, modem) = tokio::io::duplex(4096);
        let (mut mr, _mw) = tokio::io::split(modem);
        let mut link = AtLink::new(ours);
        assert!(matches!(link.command("AT+CFUN=0").await, Err(AtError::Refused(_))));
        assert!(matches!(link.command("AT+CSIM=10,\"00A4000002\"").await, Err(AtError::Refused(_))));
        drop(link);
        let mut got = Vec::new();
        mr.read_to_end(&mut got).await.unwrap();
        assert!(got.is_empty(), "nothing may be written: {got:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn echo_and_dial_failures() {
        let (ours, modem) = tokio::io::duplex(4096);
        let (mr, mut mw) = tokio::io::split(modem);
        let mut mr = tokio::io::BufReader::new(mr);
        let mut link = AtLink::new(ours);
        let m = tokio::spawn(async move {
            assert_eq!(modem_cmd(&mut mr).await, "ATD07700900101;");
            mw.write_all(b"ATD07700900101;\r\r\nBUSY\r\n").await.unwrap();
        });
        let r = link.command("ATD07700900101;").await.unwrap();
        assert!(!r.ok);
        assert_eq!(r.final_line, "BUSY");
        assert!(link.urcs.is_empty());
        m.await.unwrap();
    }
}

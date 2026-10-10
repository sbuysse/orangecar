//! One session on the modem's voice AT port: init, then a loop over URCs,
//! API requests and a 500 ms tick (CLCC poll, heartbeats, linger). Generic
//! over the byte stream, the audio pump and the car link so the tests drive
//! it with a scripted modem and recorders.
//!
//! Everything is level-triggered from the tracker's state in [`Engine::reconcile`]:
//! audio runs iff the call is active/held, the car gets `call 1` iff a call
//! is dialing/alerting/active/held and `ring 1` iff one rings. A lost URC or a
//! missed edge is corrected on the next tick instead of leaving audio or the
//! car stuck.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{info, warn};

use super::at::{self, AtError, AtLink, Reply, Urc};
use super::audio::AudioCtl;
use super::calls::{self, CallRecord, Event, State, Tracker};
use super::car::{CarLink, Signaller};
use super::{ApiError, CallView, Envelope, PhoneStatus, Request, Shared};

const TICK: Duration = Duration::from_millis(500);
/// CLCC poll period while a call exists (the backstop for lost URCs).
const CLCC_PERIOD: Duration = Duration::from_secs(1);
/// CLCC poll period while idle. Which AT port SIMCom routes URCs to is not
/// verified on our unit (AT+CATR); without this an incoming call whose RING
/// went to interface 2 would never be seen.
const IDLE_CLCC_PERIOD: Duration = Duration::from_secs(3);
/// CS registration re-read while idle.
const CREG_PERIOD: Duration = Duration::from_secs(30);
/// AT+CPCMREG=1 is refused before VOICE CALL: BEGIN (ModemManager #797);
/// retry once a second, a bounded number of times.
const AUDIO_ATTEMPTS: u32 = 10;

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// The port went away (modem unplugged/reset).
    Gone(String),
    /// The modem stopped answering coherently: reopen.
    Error(String),
    /// The service was dropped.
    Closed,
}

pub struct Engine<T, A, C> {
    link: AtLink<T>,
    tracker: Tracker,
    signaller: Signaller,
    audio: A,
    car: C,
    shared: Arc<Shared>,
    history: mpsc::UnboundedSender<CallRecord>,
    contacts_path: Option<PathBuf>,
    meeting: fn() -> bool,
    rate: u32,
    pcm_registered: bool,
    audio_attempts: u32,
    last_audio_attempt: Option<Instant>,
    want_clcc: bool,
    last_clcc: Option<Instant>,
    last_creg: Option<Instant>,
    voice_domain: &'static str,
}

impl<T, A, C> Engine<T, A, C>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
    A: AudioCtl,
    C: CarLink,
{
    pub fn new(
        io: T,
        audio: A,
        car: C,
        shared: Arc<Shared>,
        history: mpsc::UnboundedSender<CallRecord>,
        contacts_path: Option<PathBuf>,
        meeting: fn() -> bool,
    ) -> Self {
        Engine {
            link: AtLink::new(io),
            tracker: Tracker::default(),
            signaller: Signaller::default(),
            audio,
            car,
            shared,
            history,
            contacts_path,
            meeting,
            rate: 8000,
            pcm_registered: false,
            audio_attempts: 0,
            last_audio_attempt: None,
            want_clcc: false,
            last_clcc: None,
            last_creg: None,
            voice_domain: "unknown",
        }
    }

    /// Run until the port fails or the request channel closes. The receiver
    /// is handed back so the supervisor can keep answering requests (503)
    /// while no modem is open.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Envelope>) -> (Exit, mpsc::Receiver<Envelope>) {
        let exit = match self.init().await {
            Ok(()) => self.serve(&mut rx).await,
            Err(e) => e,
        };
        self.shutdown().await;
        (exit, rx)
    }

    async fn init(&mut self) -> Result<(), Exit> {
        match self.cmd("ATE0").await? {
            Some(r) if r.ok => {}
            Some(r) => return Err(Exit::Error(format!("ATE0 answered {}", r.final_line))),
            None => return Err(Exit::Error("modem does not answer ATE0".into())),
        }
        // Caller id for RING. Not saved across a modem reset: re-armed on every open.
        if !self.cmd("AT+CLIP=1").await?.is_some_and(|r| r.ok) {
            warn!("phone: AT+CLIP=1 refused, incoming numbers will come from CLCC only");
        }
        // 16 kHz PCM only if something set AT+CPCMFRM=1 (not persistent); we never set it.
        if let Some(r) = self.cmd("AT+CPCMFRM?").await? {
            self.rate = pcm_rate(&r);
        }
        info!("phone: PCM format {} Hz", self.rate);
        self.read_registration().await?;
        // A call may already exist (app restarted mid-call).
        self.poll_clcc().await?;
        self.reconcile().await
    }

    async fn serve(&mut self, rx: &mut mpsc::Receiver<Envelope>) -> Exit {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let step: Result<(), Exit> = tokio::select! {
                line = self.link.next_unsolicited() => match line {
                    Ok(l) => { self.on_line(&l); Ok(()) }
                    Err(e) => Err(Exit::Gone(e.to_string())),
                },
                env = rx.recv() => match env {
                    None => Err(Exit::Closed),
                    Some(env) => match self.handle(env.req).await {
                        Ok(res) => { let _ = env.reply.send(res); Ok(()) }
                        Err(exit) => { let _ = env.reply.send(Err(ApiError::new(503, "modem lost"))); Err(exit) }
                    },
                },
                _ = tick.tick() => self.on_tick().await,
            };
            if let Err(exit) = step {
                return exit;
            }
            if let Err(exit) = self.reconcile().await {
                return exit;
            }
        }
    }

    async fn on_tick(&mut self) -> Result<(), Exit> {
        self.tracker.tick(Instant::now());
        let period = if self.tracker.live().is_some() { CLCC_PERIOD } else { IDLE_CLCC_PERIOD };
        let poll_due = self.last_clcc.is_none_or(|t| t.elapsed() >= period);
        if self.want_clcc || poll_due {
            self.poll_clcc().await?;
        }
        if self.tracker.live().is_none() && self.last_creg.is_none_or(|t| t.elapsed() >= CREG_PERIOD) {
            self.read_registration().await?;
        }
        Ok(())
    }

    /// Send one command; URCs that came with it are applied. `Ok(None)` =
    /// timed out (the link drains the late reply before the next command).
    async fn cmd(&mut self, c: &str) -> Result<Option<Reply>, Exit> {
        let r = self.link.command(c).await;
        while let Some(l) = self.link.urcs.pop_front() {
            self.on_line(&l);
        }
        match r {
            Ok(reply) => Ok(Some(reply)),
            Err(AtError::Timeout) => {
                warn!("phone: {c} timed out");
                Ok(None)
            }
            Err(AtError::Refused(m)) => {
                // A bug on our side: never sent.
                warn!("phone: {m}");
                Ok(None)
            }
            Err(AtError::Desync) => Err(Exit::Error("modem replies out of sync".into())),
            Err(AtError::Io(e)) => Err(Exit::Gone(e.to_string())),
        }
    }

    fn on_line(&mut self, line: &str) {
        match at::parse_urc(line) {
            Urc::Ring => {
                self.apply(Event::Ring);
                self.want_clcc = true;
            }
            Urc::CallerId(n) => self.apply(Event::CallerId(n)),
            Urc::VoiceBegin => {
                info!("phone: VOICE CALL: BEGIN");
                self.apply(Event::VoiceBegin);
                self.want_clcc = true;
            }
            Urc::VoiceEnd | Urc::NoCarrier | Urc::Busy | Urc::NoAnswer => {
                info!("phone: call ended ({line})");
                self.apply(Event::Ended);
                self.want_clcc = true;
            }
            Urc::MissedCall | Urc::CallListHint => self.want_clcc = true,
            Urc::Other(l) => tracing::debug!("phone: unhandled line {l:?}"),
        }
    }

    fn apply(&mut self, ev: Event) {
        if let Some(rec) = self.tracker.on_event(ev, Instant::now()) {
            info!("phone: call {} {} ended after {} s{}", rec.direction.as_str(), rec.number, rec.duration_s, if rec.missed { " (missed)" } else { "" });
            let _ = self.history.send(rec);
        }
    }

    async fn poll_clcc(&mut self) -> Result<(), Exit> {
        self.want_clcc = false;
        let r = self.cmd("AT+CLCC").await?;
        self.last_clcc = Some(Instant::now());
        if let Some(r) = r.filter(|r| r.ok) {
            let entries = r.lines.iter().filter_map(|l| calls::parse_clcc(l)).collect();
            self.apply(Event::Snapshot(entries));
        }
        Ok(())
    }

    async fn read_registration(&mut self) -> Result<(), Exit> {
        self.last_creg = Some(Instant::now());
        if let Some(r) = self.cmd("AT+CREG?").await? {
            self.voice_domain = voice_domain(&r);
        }
        Ok(())
    }

    async fn handle(&mut self, req: Request) -> Result<Result<(), ApiError>, Exit> {
        match req {
            Request::Dial(raw) => {
                let number = match at::validate_number(&raw) {
                    Ok(n) => n,
                    Err(e) => return Ok(Err(ApiError::new(400, &e))),
                };
                let emergency = at::is_emergency(&number);
                if self.tracker.live().is_some() {
                    if !emergency {
                        return Ok(Err(ApiError::new(409, "a call is already in progress")));
                    }
                    // An emergency call never waits behind another call.
                    warn!("phone: emergency dial {number}: hanging up the current call first");
                    self.apply(Event::LocalHangup);
                    self.cmd("AT+CHUP").await?;
                    self.apply(Event::Ended);
                }
                info!("phone: dialing {number}{}", if emergency { " (EMERGENCY)" } else { "" });
                let r = self.cmd(&format!("ATD{number};")).await?;
                self.want_clcc = true;
                Ok(match r {
                    Some(r) if r.ok => {
                        self.apply(Event::Dialed(number));
                        Ok(())
                    }
                    Some(r) => Err(ApiError::new(502, &format!("dial failed: {}", r.final_line))),
                    None => Err(ApiError::new(504, "modem did not confirm the dial")),
                })
            }
            Request::Answer => {
                if self.tracker.state() != Some(State::Incoming) {
                    return Ok(Err(ApiError::new(409, "no incoming call")));
                }
                let r = self.cmd("ATA").await?;
                self.want_clcc = true;
                Ok(reply_result(r, "answer"))
            }
            Request::Hangup => {
                // Always sent, even with no call known: it is the panic button
                // and AT+CHUP with no call is harmless.
                self.apply(Event::LocalHangup);
                let r = self.cmd("AT+CHUP").await?;
                self.want_clcc = true;
                if r.as_ref().is_some_and(|r| r.ok) {
                    self.apply(Event::Ended);
                }
                Ok(reply_result(r, "hang up"))
            }
            Request::Dtmf(digits) => {
                if self.tracker.state() != Some(State::Active) {
                    return Ok(Err(ApiError::new(409, "no active call")));
                }
                if digits.is_empty() || digits.len() > 32 || !digits.chars().all(at::is_dtmf_digit) {
                    return Ok(Err(ApiError::new(400, "digits: 1-32 of 0-9 * # A-D")));
                }
                for d in digits.chars() {
                    let r = self.cmd(&format!("AT+VTS={d}")).await?;
                    if let Err(e) = reply_result(r, "DTMF") {
                        return Ok(Err(e));
                    }
                }
                Ok(Ok(()))
            }
            Request::Mute(on) => {
                if self.tracker.live().is_none() {
                    return Ok(Err(ApiError::new(409, "no call")));
                }
                let r = self.cmd(if on { "AT+CMUT=1" } else { "AT+CMUT=0" }).await?;
                let res = reply_result(r, "mute");
                if res.is_ok() {
                    self.apply(Event::Muted(on));
                }
                Ok(res)
            }
        }
    }

    /// Bring audio, car signalling and the published status in line with the
    /// tracker.
    async fn reconcile(&mut self) -> Result<(), Exit> {
        self.lookup_name().await;
        let state = self.tracker.state();
        let wants_audio = matches!(state, Some(State::Active | State::Held));
        if wants_audio && !self.audio.running() {
            let due = self.audio_attempts < AUDIO_ATTEMPTS && self.last_audio_attempt.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
            if due {
                self.audio_attempts += 1;
                self.last_audio_attempt = Some(Instant::now());
                if !self.pcm_registered {
                    match self.cmd("AT+CPCMREG=1").await? {
                        Some(r) if r.ok => self.pcm_registered = true,
                        Some(r) => warn!("phone: AT+CPCMREG=1 answered {} (attempt {})", r.final_line, self.audio_attempts),
                        // Unknown outcome: treat as on, so the end of the call turns it off.
                        None => self.pcm_registered = true,
                    }
                }
                if self.pcm_registered {
                    if let Err(e) = self.audio.start(self.rate).await {
                        warn!("phone: call audio failed to start: {e:#}");
                    }
                }
                if self.audio_attempts == AUDIO_ATTEMPTS && !self.audio.running() {
                    warn!("phone: giving up on call audio for this call");
                }
            }
        } else if !wants_audio {
            if self.audio.running() {
                self.audio.stop();
            }
            if self.pcm_registered {
                self.pcm_registered = false;
                if !self.cmd("AT+CPCMREG=0").await?.is_some_and(|r| r.ok) {
                    warn!("phone: AT+CPCMREG=0 not confirmed");
                }
            }
            self.audio_attempts = 0;
            self.last_audio_attempt = None;
        }

        let state = self.tracker.state();
        let want_call = matches!(state, Some(State::Dialing | State::Alerting | State::Active | State::Held));
        let want_ring = state == Some(State::Incoming);
        for m in self.signaller.step(want_call, want_ring, (self.meeting)(), Instant::now()) {
            if !m.ends_with('1') || !self.shared.owns_call.load(Ordering::Relaxed) {
                info!("phone: car ← {m}");
            }
            self.car.send(m).await;
        }
        self.shared.owns_call.store(self.signaller.owns_call(), Ordering::Relaxed);
        self.publish().await;
        Ok(())
    }

    async fn lookup_name(&mut self) {
        let Some(number) = self.tracker.live().filter(|c| !c.name_looked_up && !c.number.is_empty()).map(|c| c.number.clone()) else {
            return;
        };
        let name = match &self.contacts_path {
            // A few-KB local config file: no timeout (a timer racing the
            // blocking-pool read only adds a way to lose the name).
            Some(p) => tokio::fs::read_to_string(p).await.ok().and_then(|text| contact_name(&text, &number)),
            None => None,
        };
        if let Some(c) = self.tracker.call.as_mut().filter(|c| c.is_live() && c.number == number) {
            c.name = name;
            c.name_looked_up = true;
        }
    }

    async fn publish(&self) {
        let now = Instant::now();
        let call = self.tracker.call.as_ref().map(|c| CallView {
            id: c.id,
            state: c.state.as_str(),
            direction: c.direction.as_str(),
            number: c.number.clone(),
            name: c.name.clone(),
            started_at: c.started_wall.to_rfc3339(),
            duration_s: c.duration(now).as_secs(),
            muted: c.muted,
        });
        let status = PhoneStatus { available: true, modem: "ready", voice_domain: self.voice_domain, call };
        *self.shared.status.write().await = status;
    }

    /// The session is over (modem gone, error or shutdown): stop audio, end
    /// the call in the history and release the car.
    async fn shutdown(&mut self) {
        self.audio.stop();
        self.apply(Event::Ended);
        for m in self.signaller.step(false, false, (self.meeting)(), Instant::now()) {
            info!("phone: car ← {m} (session ended)");
            self.car.send(m).await;
        }
        self.shared.owns_call.store(false, Ordering::Relaxed);
    }
}

fn reply_result(r: Option<Reply>, what: &str) -> Result<(), ApiError> {
    match r {
        Some(r) if r.ok => Ok(()),
        Some(r) => Err(ApiError::new(502, &format!("{what} failed: {}", r.final_line))),
        None => Err(ApiError::new(504, &format!("{what}: modem did not answer"))),
    }
}

/// `+CPCMFRM: 1` → 16 kHz, anything else 8 kHz (the module default).
fn pcm_rate(r: &Reply) -> u32 {
    let one = r.ok && r.lines.iter().any(|l| l.strip_prefix("+CPCMFRM:").map(str::trim) == Some("1"));
    if one { 16000 } else { 8000 }
}

/// `+CREG: <n>,<stat>`: registered (1 home / 5 roaming) on the CS domain →
/// "cs". VoLTE would need `AT+CIREG?`, which is not on the allow-list yet,
/// so "volte" is never reported for now.
fn voice_domain(r: &Reply) -> &'static str {
    let stat = r
        .lines
        .iter()
        .find_map(|l| l.strip_prefix("+CREG:"))
        .map(|rest| crate::services::cell::split_fields(rest.trim()))
        .and_then(|f| f.get(1).cloned());
    match stat.as_deref() {
        Some("1") | Some("5") if r.ok => "cs",
        _ => "unknown",
    }
}

/// Name of the contact whose `phone` matches (contacts.json: `[{name, phone}]`).
pub fn contact_name(contacts_json: &str, number: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(contacts_json).ok()?;
    v.as_array()?
        .iter()
        .find(|c| c["phone"].as_str().is_some_and(|p| calls::same_number(p, number)))
        .and_then(|c| c["name"].as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    // ── Recorders ─────────────────────────────────────────────
    #[derive(Clone, Default)]
    struct RecAudio {
        log: Arc<Mutex<Vec<String>>>,
        on: Arc<Mutex<bool>>,
    }
    impl AudioCtl for RecAudio {
        async fn start(&mut self, rate: u32) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(format!("start {rate}"));
            *self.on.lock().unwrap() = true;
            Ok(())
        }
        fn stop(&mut self) {
            if std::mem::replace(&mut *self.on.lock().unwrap(), false) {
                self.log.lock().unwrap().push("stop".into());
            }
        }
        fn running(&self) -> bool {
            *self.on.lock().unwrap()
        }
    }

    #[derive(Clone, Default)]
    struct RecCar(Arc<Mutex<Vec<&'static str>>>);
    impl CarLink for RecCar {
        async fn send(&mut self, msg: &'static str) {
            self.0.lock().unwrap().push(msg);
        }
    }

    // ── Scripted modem ────────────────────────────────────────
    #[derive(Clone, Default)]
    struct Modem {
        cmds: Arc<Mutex<Vec<String>>>,
        clcc: Arc<Mutex<Vec<String>>>,
        /// Final line for the next ATD (default OK).
        dial_final: Arc<Mutex<Option<String>>>,
        /// Lines slipped into the middle of the next CLCC reply.
        clcc_interleave: Arc<Mutex<Vec<String>>>,
    }

    impl Modem {
        fn set_clcc(&self, lines: &[&str]) {
            *self.clcc.lock().unwrap() = lines.iter().map(|s| s.to_string()).collect();
        }
        fn reply(&self, cmd: &str) -> Vec<String> {
            self.cmds.lock().unwrap().push(cmd.to_string());
            let ok = vec!["OK".to_string()];
            match cmd {
                "AT+CLCC" => {
                    let mut v = self.clcc.lock().unwrap().clone();
                    let extra: Vec<String> = std::mem::take(&mut *self.clcc_interleave.lock().unwrap());
                    v.extend(extra);
                    v.push("OK".into());
                    v
                }
                "AT+CPCMFRM?" => vec!["+CPCMFRM: 0".into(), "OK".into()],
                "AT+CREG?" => vec!["+CREG: 0,1".into(), "OK".into()],
                "AT+CHUP" => {
                    self.clcc.lock().unwrap().clear();
                    vec!["VOICE CALL: END: 000004".into(), "OK".into()]
                }
                c if c.starts_with("ATD") => match self.dial_final.lock().unwrap().take() {
                    Some(f) => vec![f],
                    None => {
                        let n = &c[3..c.len() - 1];
                        self.set_clcc(&[&format!("+CLCC: 1,0,2,0,0,\"{n}\",145")]);
                        ok
                    }
                },
                _ => ok,
            }
        }
    }

    struct Harness {
        modem: Modem,
        audio: RecAudio,
        car: RecCar,
        shared: Arc<Shared>,
        tx: mpsc::Sender<Envelope>,
        history: mpsc::UnboundedReceiver<CallRecord>,
        inject: mpsc::UnboundedSender<String>,
        kill: Option<oneshot::Sender<()>>,
        engine: tokio::task::JoinHandle<(Exit, mpsc::Receiver<Envelope>)>,
    }

    fn contacts_file() -> PathBuf {
        // One file per harness: tests run in parallel, and a rewrite truncating
        // a file another engine is reading made its lookup see "".
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rascar-phone-contacts-{}-{n}.json", std::process::id()));
        std::fs::write(&p, r#"[{"name":"Alice","phone":"+447700900101","initial":"A"},{"name":"Bob","phone":"+447700900202"}]"#).unwrap();
        p
    }

    fn no_meeting() -> bool {
        false
    }

    fn start() -> Harness {
        let (ours, theirs) = tokio::io::duplex(8192);
        let modem = Modem::default();
        let (inject, mut inject_rx) = mpsc::unbounded_channel::<String>();
        let (kill, mut kill_rx) = oneshot::channel::<()>();
        let m = modem.clone();
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(theirs);
            let mut r = tokio::io::BufReader::new(r);
            loop {
                let mut line = Vec::new();
                tokio::select! {
                    n = r.read_until(b'\r', &mut line) => {
                        if n.unwrap_or(0) == 0 { return; }
                        let cmd = String::from_utf8_lossy(&line).trim().to_string();
                        for l in m.reply(&cmd) {
                            w.write_all(format!("\r\n{l}\r\n").as_bytes()).await.unwrap();
                        }
                    }
                    Some(l) = inject_rx.recv() => w.write_all(format!("\r\n{l}\r\n").as_bytes()).await.unwrap(),
                    _ = &mut kill_rx => return, // drops the stream: EOF for the engine
                }
            }
        });
        let audio = RecAudio::default();
        let car = RecCar::default();
        let shared = Arc::new(Shared::default());
        let (htx, history) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::channel(8);
        let engine = Engine::new(ours, audio.clone(), car.clone(), shared.clone(), htx, Some(contacts_file()), no_meeting);
        let engine = tokio::spawn(engine.run(rx));
        Harness { modem, audio, car, shared, tx, history, inject, kill: Some(kill), engine }
    }

    impl Harness {
        async fn request(&self, req: Request) -> Result<(), ApiError> {
            let (reply, rx) = oneshot::channel();
            self.tx.send(Envelope { req, reply }).await.unwrap();
            rx.await.unwrap()
        }
        fn urc(&self, l: &str) {
            self.inject.send(l.to_string()).unwrap();
        }
        async fn call_state(&self) -> Option<&'static str> {
            self.shared.status.read().await.call.as_ref().map(|c| c.state)
        }
        async fn until_state(&self, want: Option<&'static str>) {
            for _ in 0..200 {
                if self.call_state().await == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("call state never became {want:?} (is {:?}); cmds {:?}", self.call_state().await, self.cmds());
        }
        async fn until(&self, what: &str, f: impl Fn(&Self) -> bool) {
            for _ in 0..200 {
                if f(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("never: {what}; cmds {:?} audio {:?} car {:?}", self.cmds(), self.audio.log.lock().unwrap(), self.car.0.lock().unwrap());
        }
        fn cmds(&self) -> Vec<String> {
            self.modem.cmds.lock().unwrap().clone()
        }
        fn car_log(&self) -> Vec<&'static str> {
            self.car.0.lock().unwrap().clone()
        }
        fn audio_log(&self) -> Vec<String> {
            self.audio.log.lock().unwrap().clone()
        }
    }

    /// `want` appears in `got` in this order (other commands in between, e.g. CLCC polls).
    fn subsequence(got: &[String], want: &[&str]) -> bool {
        let mut it = got.iter();
        want.iter().all(|w| it.any(|g| g == w))
    }

    #[tokio::test(start_paused = true)]
    async fn mo_call_answered_then_remote_hangup() {
        let mut h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        h.request(Request::Dial("+44 7700 900101".into())).await.unwrap();
        h.until_state(Some("dialing")).await;
        assert_eq!(h.shared.status.read().await.call.as_ref().unwrap().name.as_deref(), Some("Alice"));
        h.until("call 1 heartbeat", |h| h.car_log().contains(&"call 1")).await;
        h.modem.set_clcc(&["+CLCC: 1,0,3,0,0,\"+447700900101\",145"]);
        h.until_state(Some("alerting")).await;
        assert!(h.audio_log().is_empty(), "no audio before the call is answered");
        h.modem.set_clcc(&["+CLCC: 1,0,0,0,0,\"+447700900101\",145"]);
        h.urc("VOICE CALL: BEGIN");
        h.until_state(Some("active")).await;
        h.until("audio", |h| h.audio_log() == vec!["start 8000"]).await;
        // Heartbeats keep going during the call.
        let beats = h.car_log().iter().filter(|m| **m == "call 1").count();
        tokio::time::sleep(Duration::from_secs(7)).await;
        assert!(h.car_log().iter().filter(|m| **m == "call 1").count() >= beats + 2);
        // Remote hangup.
        h.modem.set_clcc(&[]);
        h.urc("VOICE CALL: END: 000010");
        h.urc("NO CARRIER");
        h.until_state(Some("ended")).await;
        h.until("audio stopped", |h| h.audio_log() == vec!["start 8000", "stop"]).await;
        h.until_state(None).await;
        assert!(subsequence(
            &h.cmds(),
            &["ATE0", "AT+CLIP=1", "AT+CPCMFRM?", "AT+CREG?", "AT+CLCC", "ATD+447700900101;", "AT+CPCMREG=1", "AT+CPCMREG=0"]
        ), "{:?}", h.cmds());
        assert_eq!(h.car_log().last(), Some(&"call 0"));
        assert_eq!(h.car_log().iter().filter(|m| **m == "call 0").count(), 1);
        let rec = h.history.recv().await.unwrap();
        assert_eq!((rec.direction, rec.missed, rec.number.as_str(), rec.name.as_str()), (calls::Direction::Out, false, "+447700900101", "Alice"));
        assert!(rec.duration_s >= 7);
    }

    #[tokio::test(start_paused = true)]
    async fn mo_call_busy_and_no_answer() {
        let mut h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        *h.modem.dial_final.lock().unwrap() = Some("BUSY".into());
        let e = h.request(Request::Dial("07700900101".into())).await.unwrap_err();
        assert_eq!(e.status, 502);
        assert!(e.error.contains("BUSY"));
        assert_eq!(h.call_state().await, None);
        assert!(h.car_log().is_empty());

        // Accepted, rings out, nobody answers.
        h.request(Request::Dial("07700900101".into())).await.unwrap();
        h.modem.set_clcc(&["+CLCC: 1,0,3,0,0,\"07700900101\",129"]);
        h.until_state(Some("alerting")).await;
        h.modem.set_clcc(&[]);
        h.urc("NO ANSWER");
        h.until_state(Some("ended")).await;
        h.until_state(None).await;
        assert!(h.audio_log().is_empty());
        assert!(!h.cmds().contains(&"AT+CPCMREG=1".to_string()));
        // Dialing/alerting already hold the telephony stream (ring-back), then release it once.
        assert_eq!(h.car_log().first(), Some(&"call 1"));
        assert_eq!(h.car_log().last(), Some(&"call 0"));
        assert_eq!(h.car_log().iter().filter(|m| **m == "call 0").count(), 1);
        let rec = h.history.recv().await.unwrap();
        assert_eq!((rec.direction, rec.missed, rec.duration_s), (calls::Direction::Out, false, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn mt_call_answered_with_urcs_interleaved_in_a_reply() {
        let mut h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        // The RING and CLIP land in the middle of a CLCC reply.
        h.modem.set_clcc(&["+CLCC: 1,1,4,0,0,\"+447700900202\",145"]);
        *h.modem.clcc_interleave.lock().unwrap() = vec!["RING".into(), "+CLIP: \"+447700900202\",145,,,,0".into()];
        h.urc("RING");
        h.until_state(Some("incoming")).await;
        for _ in 0..20 {
            if h.shared.status.read().await.call.as_ref().and_then(|c| c.name.clone()).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let call = h.shared.status.read().await.call.clone().unwrap();
        assert_eq!((call.direction, call.number.as_str(), call.name.as_deref()), ("in", "+447700900202", Some("Bob")));
        h.until("ring 1", |h| h.car_log().contains(&"ring 1")).await;
        assert!(!h.car_log().contains(&"call 1"), "ringing is not a call yet");
        h.request(Request::Answer).await.unwrap();
        h.modem.set_clcc(&["+CLCC: 1,1,0,0,0,\"+447700900202\",145"]);
        h.urc("VOICE CALL: BEGIN");
        h.until_state(Some("active")).await;
        h.until("audio", |h| h.audio_log() == vec!["start 8000"]).await;
        let log = h.car_log();
        let r0 = log.iter().position(|m| *m == "ring 0").expect("ring 0");
        let c1 = log.iter().position(|m| *m == "call 1").expect("call 1");
        assert!(r0 < c1, "{log:?}");
        // Mute and DTMF go through.
        h.request(Request::Mute(true)).await.unwrap();
        assert!(h.shared.status.read().await.call.as_ref().unwrap().muted);
        h.request(Request::Dtmf("1#".into())).await.unwrap();
        assert!(h.request(Request::Dtmf("1;AT".into())).await.is_err());
        // We hang up.
        h.request(Request::Hangup).await.unwrap();
        h.until("audio stopped", |h| h.audio_log() == vec!["start 8000", "stop"]).await;
        h.until_state(None).await;
        assert!(subsequence(&h.cmds(), &["ATA", "AT+CPCMREG=1", "AT+CMUT=1", "AT+VTS=1", "AT+VTS=#", "AT+CHUP", "AT+CPCMREG=0"]), "{:?}", h.cmds());
        assert!(!h.cmds().iter().any(|c| c.contains(';') && c.starts_with("AT+VTS")));
        assert_eq!(h.car_log().last(), Some(&"call 0"));
        let rec = h.history.recv().await.unwrap();
        assert_eq!((rec.direction, rec.missed, rec.name.as_str()), (calls::Direction::In, false, "Bob"));
    }

    #[tokio::test(start_paused = true)]
    async fn mt_call_missed() {
        let mut h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        h.modem.set_clcc(&["+CLCC: 1,1,4,0,0,\"+447700900202\",145"]);
        h.urc("RING");
        h.urc("+CLIP: \"+447700900202\",145,,,,0");
        h.until_state(Some("incoming")).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(h.car_log().iter().filter(|m| **m == "ring 1").count() >= 3, "ring heartbeat every 3 s: {:?}", h.car_log());
        h.modem.set_clcc(&[]);
        h.urc("MISSED_CALL: 10:23AM +447700900202");
        h.until_state(Some("ended")).await;
        assert_eq!(h.car_log().last(), Some(&"ring 0"));
        assert!(!h.car_log().contains(&"call 1"));
        assert!(h.audio_log().is_empty());
        let rec = h.history.recv().await.unwrap();
        assert!(rec.missed);
        assert_eq!(rec.name, "Bob");
        // Answering a call that is gone is refused, not sent.
        h.until_state(None).await;
        assert_eq!(h.request(Request::Answer).await.unwrap_err().status, 409);
        assert!(!h.cmds().contains(&"ATA".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn modem_gone_mid_call() {
        let mut h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        // App restarted mid-call: the first CLCC already lists an active call.
        h.modem.set_clcc(&["+CLCC: 1,0,0,0,0,\"+447700900101\",145"]);
        h.until_state(Some("active")).await;
        h.until("audio", |h| h.audio_log() == vec!["start 8000"]).await;
        h.kill.take().unwrap().send(()).unwrap();
        let (exit, _rx) = (&mut h.engine).await.unwrap();
        assert!(matches!(exit, Exit::Gone(_)), "{exit:?}");
        assert_eq!(h.audio_log(), vec!["start 8000", "stop"]);
        assert_eq!(h.car_log().last(), Some(&"call 0"));
        assert!(!h.shared.owns_call.load(Ordering::Relaxed));
        assert_eq!(h.history.recv().await.unwrap().direction, calls::Direction::Out);
    }

    #[tokio::test(start_paused = true)]
    async fn emergency_dial_always_goes_through() {
        let h = start();
        h.until("init", |h| h.cmds().contains(&"AT+CLCC".to_string())).await;
        h.request(Request::Dial("+447700900101".into())).await.unwrap();
        h.until_state(Some("dialing")).await;
        // A normal number is refused during a call...
        assert_eq!(h.request(Request::Dial("07700900202".into())).await.unwrap_err().status, 409);
        // ...112 hangs the current call up and dials.
        h.request(Request::Dial("112".into())).await.unwrap();
        assert!(subsequence(&h.cmds(), &["ATD+447700900101;", "AT+CHUP", "ATD112;"]), "{:?}", h.cmds());
        assert!(!h.cmds().contains(&"ATD07700900202;".to_string()));
        for _ in 0..40 {
            if h.shared.status.read().await.call.as_ref().is_some_and(|c| c.number == "112" && c.state == "dialing") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("112 call not shown: {:?}", h.shared.status.read().await.call);
    }

    #[test]
    fn helpers_parse_modem_answers() {
        let r = |lines: &[&str]| Reply { ok: true, lines: lines.iter().map(|s| s.to_string()).collect(), final_line: "OK".into() };
        assert_eq!(pcm_rate(&r(&["+CPCMFRM: 1"])), 16000);
        assert_eq!(pcm_rate(&r(&["+CPCMFRM: 0"])), 8000);
        assert_eq!(pcm_rate(&r(&[])), 8000);
        assert_eq!(voice_domain(&r(&["+CREG: 0,1"])), "cs");
        assert_eq!(voice_domain(&r(&["+CREG: 0,5"])), "cs");
        assert_eq!(voice_domain(&r(&["+CREG: 0,2"])), "unknown");
        let contacts = r#"[{"name":"Alice","phone":"+447700900101"}]"#;
        assert_eq!(contact_name(contacts, "07700900101").as_deref(), Some("Alice"));
        assert_eq!(contact_name(contacts, "07701000000"), None);
        assert_eq!(contact_name("not json", "07700900101"), None);
    }
}

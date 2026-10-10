#!/usr/bin/env python3
"""
carplay_receiver_sim.py — a CarPlay RECEIVER (head unit) for the bench.

Why this exists
---------------
`rascar-carplay --carplay-endpoint HOST:PORT` runs the real sender path with no
board and no car. What was missing was something for it to talk to, so the only
way to test "does the session survive?" was to drive to the car.

This simulates the Jaguar F-PACE closely enough to exercise the whole sender
path, and — crucially — it reproduces the Jaguar's FATAL behaviour, so the bug
is observable on a laptop:

  The head unit arbitrates audio as a RESOURCE. `modesChanged` assigns
  resourceID 2 (MainAudio) to an entity: 1 = controller (phone), 2 = accessory
  (car). If the sender assigns MainAudio to the ACCESSORY and then SETUPs a
  MainAudio stream (type 100) anyway, the real car never answers that request,
  stops reading the screen TCP, and tears the session down ~2 s later.

  `--jaguar-audio-trap` (default on) reproduces exactly that.

It stays unencrypted by answering the first `SETUP /setup` with 200 rather than
`466 Key Management Error`, which skips the HAP pairing layer. Everything after
that — RTSP, plists, modesChanged, stream SETUP, the 128-byte framed H.264
screen channel — is the same code the car drives.

`--expect-handback` checks "give the screen to the car's native UI, and come
back": the sender must stop its screen stream and TEARDOWN it (type 110) BEFORE
announcing MainScreen as the accessory's (modesChanged entity 2); nothing may
touch the screen while it is the car's. ~3 s later the sim sends `requestUI`
(the driver touching the car's CarPlay icon) and expects modesChanged
MainScreen 1 followed by a fresh screen SETUP and frames.

Exit code 0 = the session met the survival bar; non-zero = it did not.
"""

import argparse
import plistlib
import socket
import struct
import sys
import threading
import time

LOG_LOCK = threading.Lock()
T0 = time.monotonic()


def log(msg):
    with LOG_LOCK:
        print(f"[{time.monotonic() - T0:7.3f}s] {msg}", flush=True)


class Stats:
    def __init__(self):
        self.frames = 0
        self.bytes = 0
        self.first_frame_at = None
        self.last_frame_at = None
        self.session_end_at = None
        self.end_reason = None
        self.audio_trap_fired = False
        self.main_audio_entity = None
        self.screen_entity = None
        # Telephony (car microphone for meetings)
        self.phone_call_history = []      # phoneCall entity per modesChanged
        self.audio_modes_history = []     # MainAudio entity per modesChanged
        self.modes_after_change = []      # (reason, MainAudio entities of the modesChanged that followed)
        self.audio_setups = []            # audioType per MainAudio SETUP
        self.audio_teardowns = 0
        self.telephony_rx_packets = 0     # sender -> car, on a telephony stream
        self.media_rx_packets = 0
        self.mic_tx_packets = 0           # car mic -> sender
        self.requests = []
        self.change_modes_replies = []    # (reasonStr, status, MainAudio entity) per car changeModes
        # Screen hand-back
        self.timeline = []                # (t, event, detail): screen_setup / screen_teardown / modes_screen / requestUI
        self.screen_with_car = False      # MainScreen as last announced by the sender
        self.screen_perm_entity = None
        self.frames_while_car = 0         # frames processed after the screen was announced as the car's
        self.setups_while_car = 0
        self.frames_after_resume = 0      # frames on a screen stream opened after a requestUI
        self.resumed = False
        self.data_closed_before_teardown = []   # per screen TEARDOWN: had the sender closed the data socket?
        self.change_modes_screen = []     # (reply MainScreen entity, announced MainScreen entity)


STATS = Stats()

# entity ids, per the sender's own constants
ENTITY_NONE, ENTITY_CONTROLLER, ENTITY_ACCESSORY = 0, 1, 2
RESOURCE_MAIN_SCREEN, RESOURCE_MAIN_AUDIO = 1, 2
STREAM_MAIN_AUDIO, STREAM_SCREEN = 100, 110


def mark(event, detail=None):
    STATS.timeline.append((time.monotonic() - T0, event, detail))


def bplist(obj):
    return plistlib.dumps(obj, fmt=plistlib.FMT_BINARY)


def info_plist(width, height):
    """Mimics the Jaguar's GET /info, including the bits the sender parses."""
    return {
        "name": "Jaguar F-PACE",
        "model": "JLR,1",
        "manufacturer": "Jaguar",
        "deviceID": "aa:bb:cc:dd:ee:ff",
        "sourceVersion": "280.33.12.1",
        "keepAliveSendStatsAsBody": True,
        # Verbatim from the car (2026-09-29 pcap): every number is a STRING.
        "audioFormats": [
            {"type": "100", "audioType": "compatibility", "audioInputFormats": "0x54", "audioOutputFormats": "0x854"},
            {"type": "101", "audioType": "compatibility", "audioOutputFormats": "0x8000"},
            {"type": "100", "audioType": "media", "audioOutputFormats": "0x8000"},
            {"type": "100", "audioType": "alert", "audioOutputFormats": "0x800"},
            {"type": "100", "audioType": "telephony", "audioInputFormats": "0x54", "audioOutputFormats": "0x54"},
            {"type": "100", "audioType": "speechRecognition", "audioInputFormats": "0x40", "audioOutputFormats": "0x40"},
            {"type": "100", "audioType": "default", "audioInputFormats": "0x50", "audioOutputFormats": "0x50"},
            {"type": "101", "audioType": "default", "audioOutputFormats": "0x800"},
        ],
        "audioLatencies": [
            {"type": STREAM_MAIN_AUDIO, "audioType": "media",
             "inputLatencyMicros": 0, "outputLatencyMicros": 100000},
        ],
        "displays": [
            {"widthPixels": width, "heightPixels": height,
             "widthPhysical": 0, "heightPhysical": 0,
             "maxFPS": 30, "features": 14, "primaryInputDevice": 1,
             "uuid": "e5f7a5a0-0000-0000-0000-000000000001"}
        ],
        "modes": {
            "appStates": [
                {"appStateID": 1, "speechMode": -1},
                {"appStateID": 2, "state": False},
                {"appStateID": 3, "state": False},
            ],
            "resources": [
                {"resourceID": RESOURCE_MAIN_SCREEN, "transferType": 1,
                 "transferPriority": 100, "takeConstraint": 100, "borrowConstraint": 100},
                {"resourceID": RESOURCE_MAIN_AUDIO, "transferType": 1,
                 "transferPriority": 100, "takeConstraint": 100, "borrowConstraint": 100},
            ],
        },
    }


PCM_RATES = {1 << 4: 16000, 1 << 6: 24000, 1 << 8: 32000, 1 << 14: 48000}


class AudioStream:
    """A MainAudio stream: counts the sender's RTP, and for telephony plays the
    car's microphone (a 440 Hz tone, 16-bit big-endian mono, plaintext RTP,
    352 frames per packet) to the dataPort the sender put in its SETUP."""

    def __init__(self, telephony, rate, mic_dst):
        self.telephony = telephony
        self.stop = threading.Event()
        self.rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.rx.bind(("127.0.0.1", 0))
        self.rx.settimeout(0.5)
        self.port = self.rx.getsockname()[1]
        threading.Thread(target=self._count, daemon=True).start()
        if telephony and mic_dst:
            threading.Thread(target=self._mic, args=(rate, mic_dst), daemon=True).start()

    def _count(self):
        while not self.stop.is_set():
            try:
                self.rx.recv(4096)
            except OSError:
                continue
            if self.telephony:
                STATS.telephony_rx_packets += 1
            else:
                STATS.media_rx_packets += 1

    def _mic(self, rate, dst):
        import math
        tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        seq, ts, n = 0, 0, 0
        period = 352 / rate
        nxt = time.monotonic()
        while not self.stop.is_set():
            pcm = b"".join(struct.pack(">h", int(8000 * math.sin(2 * math.pi * 440 * (ts + i) / rate))) for i in range(352))
            hdr = struct.pack(">BBHII", 2 << 6, 100, seq & 0xFFFF, ts & 0xFFFFFFFF, 0)
            tx.sendto(hdr + pcm, dst)
            STATS.mic_tx_packets += 1
            seq += 1
            ts += 352
            nxt += period
            time.sleep(max(0.0, nxt - time.monotonic()))

    def close(self):
        self.stop.set()


class ScreenChannel(threading.Thread):
    """Accepts the sender's screen TCP connection and consumes framed H.264."""

    daemon = True

    def __init__(self, sock):
        super().__init__()
        self.sock = sock
        self.stop = threading.Event()
        self.closed_by_sender = threading.Event()

    def run(self):
        try:
            conn, addr = self.sock.accept()
        except OSError:
            return
        log(f"screen: sender connected from {addr}")
        conn.settimeout(1.0)
        buf = b""
        while not self.stop.is_set():
            try:
                chunk = conn.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                log("screen: sender closed the data channel")
                self.closed_by_sender.set()
                break
            buf += chunk
            STATS.bytes += len(chunk)
            # 128-byte header: bodySize u32 LE @0, opcode @4
            while len(buf) >= 128:
                body = struct.unpack_from("<I", buf, 0)[0]
                if len(buf) < 128 + body:
                    break
                op = buf[4]
                buf = buf[128 + body:]
                if op == 0:  # VideoFrame
                    STATS.frames += 1
                    if STATS.screen_with_car:
                        STATS.frames_while_car += 1
                    if STATS.resumed:
                        STATS.frames_after_resume += 1
                    now = time.monotonic() - T0
                    if STATS.first_frame_at is None:
                        STATS.first_frame_at = now
                        log("screen: FIRST VideoFrame")
                    STATS.last_frame_at = now
                    if STATS.frames % 150 == 0:
                        log(f"screen: {STATS.frames} frames")
        try:
            conn.close()
        except OSError:
            pass


class Session:
    def __init__(self, conn, args):
        self.conn = conn
        self.args = args
        self.screen = None
        self.screen_port = None
        self.audio = None
        self.dead = False
        self.event_conn = None
        self.event_lock = threading.Lock()
        self.event_cseq = 0
        self.event_port, self._event_sock = self._listen()
        self.keepalive_port, self._ka_sock = self._listen()
        for s_, name in ((self._event_sock, "event"), (self._ka_sock, "keepAlive")):
            threading.Thread(target=self._hold, args=(s_, name), daemon=True).start()

    @staticmethod
    def _listen():
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        s.listen(2)
        return s.getsockname()[1], s

    def _hold(self, sock, name):
        """Accept and hold a side channel open; the car keeps these for the session."""
        sock.settimeout(1.0)
        conns = []
        while not self.dead:
            try:
                c, _ = sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            log(f"   {name} channel: sender connected")
            conns.append(c)
            if name == "event":
                self.event_conn = c
                threading.Thread(target=self._car_borrows_audio, daemon=True).start()
        for c in conns:
            try:
                c.close()
            except OSError:
                pass

    def send_event_command(self, obj):
        """car -> phone `POST /command` on the event channel; returns the reply plist."""
        c = self.event_conn
        if c is None:
            raise OSError("no event channel")
        with self.event_lock:
            self.event_cseq += 1
            body = bplist(obj)
            head = (f"POST /command RTSP/1.0\r\nCSeq: {self.event_cseq}\r\n"
                    f"Content-Type: application/x-apple-binary-plist\r\n"
                    f"Content-Length: {len(body)}\r\n\r\n").encode()
            c.settimeout(3.0)
            c.sendall(head + body)
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = c.recv(4096)
                if not chunk:
                    raise OSError("event channel closed")
                data += chunk
            hdr, rest = data.split(b"\r\n\r\n", 1)
            length = 0
            for ln in hdr.decode("latin1").split("\r\n"):
                if ln.lower().startswith("content-length:"):
                    length = int(ln.split(":", 1)[1])
            while len(rest) < length:
                rest += c.recv(4096)
            return plistlib.loads(rest[:length]) if length else {}

    def _request_ui_later(self, delay):
        """The driver touches the car's CarPlay icon: the car sends requestUI."""
        time.sleep(delay)
        if self.dead:
            return
        log("   car -> requestUI (CarPlay icon touched on the car's native UI)")
        mark("requestUI")
        try:
            self.send_event_command({"type": "requestUI"})
        except OSError as e:
            log(f"   requestUI: no reply ({e})")

    def _car_borrows_audio(self):
        """What the Jaguar did 31 s into the 2026-09-29 session: borrow MainAudio
        ("Request Main Audio", transferType 3, priority 500), release it 1 s later."""
        requests = [
            ("Request Main Audio", {"transferPriority": 500, "resourceID": RESOURCE_MAIN_AUDIO,
                                    "transferType": 3, "unborrowConstraint": 500}),
            ("Release Main Audio Request", {"resourceID": RESOURCE_MAIN_AUDIO, "transferType": 4}),
        ]
        time.sleep(4.0)
        for reason, res in requests:
            try:
                announced_screen = ENTITY_ACCESSORY if STATS.screen_with_car else ENTITY_CONTROLLER
                seen = len(STATS.audio_modes_history)
                reply = self.send_event_command(
                    {"type": "changeModes", "params": {"reasonStr": reason, "resources": [res]}})
            except OSError as e:
                log(f"   changeModes {reason!r}: no reply ({e})")
                STATS.change_modes_replies.append((reason, None, None))
                return
            audio = next((r.get("entity") for r in (reply.get("params") or {}).get("resources", [])
                          if r.get("resourceID") == RESOURCE_MAIN_AUDIO), None)
            screen = next((r.get("entity") for r in (reply.get("params") or {}).get("resources", [])
                           if r.get("resourceID") == RESOURCE_MAIN_SCREEN), None)
            STATS.change_modes_screen.append((screen, announced_screen))
            log(f"   changeModes {reason!r} -> status {reply.get('status')} MainAudio entity {audio}")
            STATS.change_modes_replies.append((reason, reply.get("status"), audio))
            time.sleep(1.0)
            # catplay follows every accepted changeModes with its own modesChanged.
            # Only the FIRST one answers this request (a meeting may send its own right after).
            STATS.modes_after_change.append((reason, STATS.audio_modes_history[seen:seen + 1]))

    # ---- RTSP plumbing -------------------------------------------------
    def read_request(self):
        """Returns (method, uri, headers, body) or None on EOF."""
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = self.conn.recv(4096)
            if not chunk:
                return None
            data += chunk
        head, rest = data.split(b"\r\n\r\n", 1)
        lines = head.decode("latin1").split("\r\n")
        method, uri, _ = lines[0].split(" ", 2)
        headers = {}
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                headers[k.strip().lower()] = v.strip()
        n = int(headers.get("content-length", "0"))
        while len(rest) < n:
            chunk = self.conn.recv(65536)
            if not chunk:
                break
            rest += chunk
        return method, uri, headers, rest[:n]

    def respond(self, cseq, body=None, status="200 OK"):
        if self.dead:
            return
        payload = b"" if body is None else bplist(body)
        head = f"RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: AirTunes/280.33.12.1\r\n"
        if payload:
            head += "Content-Type: application/x-apple-binary-plist\r\n"
        head += f"Content-Length: {len(payload)}\r\n\r\n"
        self.conn.sendall(head.encode("latin1") + payload)

    # ---- the Jaguar's fatal behaviour ---------------------------------
    def fire_audio_trap(self):
        STATS.audio_trap_fired = True
        log("!! AUDIO TRAP: sender SETUP a MainAudio stream (type 100) after giving "
            "MainAudio to the ACCESSORY in modesChanged — this is what the Jaguar "
            "never answers. Going silent, then tearing the session down (as the car does).")
        self.dead = True  # never answer this request

        def teardown():
            time.sleep(self.args.trap_teardown_secs)
            log("!! AUDIO TRAP: dropping the session (car pulls its USB gadget here)")
            STATS.session_end_at = time.monotonic() - T0
            STATS.end_reason = "audio-resource-contradiction"
            if self.screen:
                self.screen.stop.set()
            try:
                self.conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                self.conn.close()
            except OSError:
                pass

        threading.Thread(target=teardown, daemon=True).start()

    # ---- request handling ---------------------------------------------
    def handle(self, method, uri, headers, body):
        cseq = headers.get("cseq", "0")
        req = plistlib.loads(body) if body[:8] == b"bplist00" else None
        label = f"{method} {uri}"
        STATS.requests.append(label)
        log(f"<- {label} (cseq={cseq})")

        if uri.startswith("/setup") and method == "SETUP" and req and "streams" not in req:
            # Answer 200 (not 466) so the session stays unencrypted. The event
            # channel must be a REAL listening port: the sender dials it and then
            # serves POST /command on it (requestUI/changeModes come back that way),
            # so returning 0 makes it abort with ECONNREFUSED.
            self.respond(cseq, {"eventPort": self.event_port,
                                "timingPort": 0,
                                "keepAlivePort": self.keepalive_port})
            return

        if method == "GET" and uri.startswith("/info"):
            self.respond(cseq, info_plist(self.args.width, self.args.height))
            return

        if method == "POST" and uri.startswith("/pair-"):
            self.respond(cseq, None, status="470 Connection Authorization Required")
            return

        if method == "RECORD":
            self.respond(cseq, None)
            return

        if method == "POST" and uri.startswith("/feedback"):
            self.respond(cseq, {})
            return

        if method == "POST" and uri.startswith("/command"):
            if req and req.get("type") == "modesChanged":
                for a in (req.get("params") or {}).get("appStates", []):
                    if a.get("appStateID") == 2:
                        STATS.phone_call_history.append(a.get("entity"))
                for r in (req.get("params") or {}).get("resources", []):
                    if r.get("resourceID") == RESOURCE_MAIN_AUDIO:
                        STATS.main_audio_entity = r.get("entity")
                        STATS.audio_modes_history.append(r.get("entity"))
                    if r.get("resourceID") == RESOURCE_MAIN_SCREEN:
                        STATS.screen_entity = r.get("entity")
                        STATS.screen_perm_entity = r.get("permanentEntity")
                log(f"   modesChanged: MainScreen->entity {STATS.screen_entity}/{STATS.screen_perm_entity}, "
                    f"MainAudio->entity {STATS.main_audio_entity}")
                to_car = STATS.screen_entity == ENTITY_ACCESSORY
                if to_car != STATS.screen_with_car:
                    mark("modes_screen", f"{STATS.screen_entity}/{STATS.screen_perm_entity}")
                    STATS.screen_with_car = to_car
                    STATS.resumed = False
                    if to_car:
                        log(f"   MainScreen is now the CAR's (screen stream open: {self.screen is not None})")
                        if self.args.expect_handback:
                            threading.Thread(target=self._request_ui_later,
                                             args=(self.args.request_ui_after,), daemon=True).start()
            self.respond(cseq, {})
            return

        if method == "SETUP" and req and "streams" in req:
            st = req["streams"][0]
            stype = st.get("type")
            if stype == STREAM_SCREEN:
                mark("screen_setup")
                if STATS.screen_with_car:
                    STATS.setups_while_car += 1
                    log("   !! screen SETUP while MainScreen is announced as the CAR's")
                elif any(e == "requestUI" for _, e, _ in STATS.timeline):
                    STATS.resumed = True
                self.screen_port, self.screen = self.open_screen()
                log(f"   screen stream -> dataPort {self.screen_port}")
                self.respond(cseq, {"streams": [{"type": STREAM_SCREEN,
                                                 "dataPort": self.screen_port}]})
                return
            if stype == STREAM_MAIN_AUDIO:
                if (self.args.jaguar_audio_trap
                        and STATS.main_audio_entity != ENTITY_CONTROLLER):
                    self.fire_audio_trap()
                    return
                atype = st.get("audioType", "default")
                STATS.audio_setups.append(atype)
                telephony = atype == "telephony"
                mic_dst = ("127.0.0.1", st["dataPort"]) if telephony and st.get("input") and st.get("dataPort") else None
                if telephony and not mic_dst:
                    log("   !! telephony SETUP without input/dataPort — no microphone possible")
                rate = PCM_RATES.get(st.get("audioFormat"), 16000)
                if self.args.audio_setup_delay > 0:
                    # The Jaguar on 2026-10-01: reply 3.2 s late, in-line (head of line).
                    log(f"   delaying the audio SETUP reply by {self.args.audio_setup_delay}s")
                    time.sleep(self.args.audio_setup_delay)
                if self.audio:
                    self.audio.close()
                self.audio = AudioStream(telephony, rate, mic_dst)
                log(f"   audio stream accepted: audioType={atype} format={st.get('audioFormat')} "
                    f"input={st.get('input')} mic->{mic_dst} (dataPort {self.audio.port})")
                self.respond(cseq, {"streams": [{"type": STREAM_MAIN_AUDIO,
                                                 "dataPort": self.audio.port, "controlPort": 7011}]})
                return
            self.respond(cseq, {"streams": []})
            return

        if method == "TEARDOWN":
            types = [x.get("type") for x in (req or {}).get("streams", [])]
            if STREAM_SCREEN in types:
                mark("screen_teardown")
                closed = False
                if self.screen:
                    closed = self.screen.closed_by_sender.wait(0.5)
                    self.screen.stop.set()
                    self.screen = None
                STATS.data_closed_before_teardown.append(closed)
                log(f"   screen stream torn down (sender had closed the data channel: {closed})")
            if STREAM_MAIN_AUDIO in types:
                STATS.audio_teardowns += 1
                if self.audio:
                    self.audio.close()
                    self.audio = None
                log("   audio stream torn down")
            self.respond(cseq, None)
            return

        self.respond(cseq, {})

    def open_screen(self):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        port = s.getsockname()[1]
        ch = ScreenChannel(s)
        ch.start()
        return port, ch

    def serve(self):
        self.conn.settimeout(1.0)
        while not self.dead:
            try:
                r = self.read_request()
            except socket.timeout:
                continue
            except OSError:
                break
            if r is None:
                break
            try:
                self.handle(*r)
            except Exception as e:  # keep the receiver alive, like a real one
                log(f"   handler error: {e!r}")
        if STATS.session_end_at is None:
            STATS.session_end_at = time.monotonic() - T0
            STATS.end_reason = STATS.end_reason or "control-connection-closed"
        if self.screen:
            self.screen.stop.set()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=7000)
    ap.add_argument("--width", type=int, default=1200)
    ap.add_argument("--height", type=int, default=512)
    ap.add_argument("--run-secs", type=float, default=30.0,
                    help="how long the receiver stays up")
    ap.add_argument("--min-survival-secs", type=float, default=20.0,
                    help="session must live at least this long")
    ap.add_argument("--min-frames", type=int, default=100)
    ap.add_argument("--trap-teardown-secs", type=float, default=2.3,
                    help="Jaguar drops the link this long after the unanswered SETUP")
    ap.add_argument("--jaguar-audio-trap", dest="jaguar_audio_trap",
                    action="store_true", default=True)
    ap.add_argument("--no-jaguar-audio-trap", dest="jaguar_audio_trap",
                    action="store_false")
    ap.add_argument("--expect-meeting", action="store_true",
                    help="assert a telephony switch and back (the sender got 'call 1' heartbeats)")
    ap.add_argument("--expect-handback", action="store_true",
                    help="assert a screen hand-back to the car and a come-back on requestUI")
    ap.add_argument("--request-ui-after", type=float, default=3.0,
                    help="seconds after the hand-back before the car sends requestUI")
    ap.add_argument("--audio-setup-delay", type=float, default=0.0,
                    help="answer the MainAudio SETUP this many seconds late (the car did 3.2 s)")
    ap.add_argument("--expect-media-audio", action="store_true",
                    help="assert media audio packets actually arrived")
    args = ap.parse_args()

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", args.port))
    srv.listen(4)
    srv.settimeout(args.run_secs)
    log(f"receiver listening on 127.0.0.1:{args.port} "
        f"(jaguar_audio_trap={args.jaguar_audio_trap}, display {args.width}x{args.height})")

    deadline = time.monotonic() + args.run_secs
    sess = None
    try:
        conn, addr = srv.accept()
        log(f"control connection from {addr}")
        sess = Session(conn, args)
        t = threading.Thread(target=sess.serve, daemon=True)
        t.start()
        while time.monotonic() < deadline and t.is_alive():
            time.sleep(0.2)
        if t.is_alive():
            log("run window elapsed — receiver still up and session still alive")
            STATS.session_end_at = time.monotonic() - T0
            STATS.end_reason = "receiver-run-window-elapsed"
            sess.dead = True
    except socket.timeout:
        log("no sender connected within the run window")

    # ---- verdict --------------------------------------------------------
    start = 0.0
    life = (STATS.session_end_at or 0.0) - start
    print()
    print("=" * 66)
    print(f"  session lifetime : {life:.2f}s  (bar: {args.min_survival_secs:.1f}s)")
    print(f"  end reason       : {STATS.end_reason}")
    print(f"  video frames     : {STATS.frames}  (bar: {args.min_frames})")
    print(f"  bytes on screen  : {STATS.bytes}")
    print(f"  MainAudio entity : {STATS.main_audio_entity} "
          f"(1=controller/phone, 2=accessory/car)")
    print(f"  audio trap fired : {STATS.audio_trap_fired}")
    print(f"  audio SETUPs     : {STATS.audio_setups} · teardowns {STATS.audio_teardowns}")
    print(f"  phoneCall entity : {STATS.phone_call_history}")
    print(f"  audio packets    : media {STATS.media_rx_packets} · telephony {STATS.telephony_rx_packets}"
          f" · car mic sent {STATS.mic_tx_packets}")
    print(f"  car changeModes  : {STATS.change_modes_replies}")
    print(f"  modesChanged after changeModes: {STATS.modes_after_change}")
    print(f"  screen timeline  : {[(round(t, 2), e, d) for t, e, d in STATS.timeline]}")
    print(f"  screen handback  : frames while car's {STATS.frames_while_car} · SETUPs while car's "
          f"{STATS.setups_while_car} · frames after come-back {STATS.frames_after_resume} · "
          f"data closed before TEARDOWN {STATS.data_closed_before_teardown}")
    print("=" * 66)

    ok = True
    expected = [("Request Main Audio", 0, ENTITY_ACCESSORY), ("Release Main Audio Request", 0, ENTITY_CONTROLLER)]
    if STATS.change_modes_replies != expected:
        print(f"FAIL: the car's MainAudio borrow/release was not accepted as expected {expected}.")
        ok = False
    expected_after = [("Request Main Audio", [ENTITY_ACCESSORY]), ("Release Main Audio Request", [ENTITY_CONTROLLER])]
    if STATS.modes_after_change != expected_after:
        print(f"FAIL: modesChanged after the car's changeModes {STATS.modes_after_change}, expected {expected_after}.")
        ok = False
    if STATS.audio_trap_fired:
        print("FAIL: the sender asked for a MainAudio stream it had just given to the car.")
        ok = False
    if life < args.min_survival_secs:
        print(f"FAIL: session died after {life:.2f}s (< {args.min_survival_secs}s).")
        ok = False
    if STATS.frames < args.min_frames:
        print(f"FAIL: only {STATS.frames} video frames (< {args.min_frames}).")
        ok = False
    if args.expect_meeting:
        if "telephony" not in STATS.audio_setups:
            print("FAIL: no telephony stream was set up during the meeting.")
            ok = False
        if 1 not in STATS.phone_call_history:
            print("FAIL: modesChanged never announced the call (phoneCall -> controller).")
            ok = False
        if STATS.telephony_rx_packets < 50 or STATS.mic_tx_packets < 50:
            print("FAIL: the telephony stream did not carry audio both ways.")
            ok = False
        if STATS.audio_setups[-1:] != ["media"] or STATS.phone_call_history[-1:] != [0]:
            print("FAIL: media audio / phoneCall=none not restored after the meeting.")
            ok = False
    if args.expect_media_audio and STATS.media_rx_packets < 50:
        print(f"FAIL: only {STATS.media_rx_packets} media audio packets arrived.")
        ok = False
    bad_snap = [p for p in STATS.change_modes_screen if p[0] != p[1]]
    if bad_snap:
        print(f"FAIL: ChangeModesResponse MainScreen contradicts the announced owner: {bad_snap}")
        ok = False
    if STATS.setups_while_car or STATS.frames_while_car:
        print("FAIL: the screen was streamed/SETUP while announced as the car's.")
        ok = False
    if args.expect_handback:
        ev = [e if d is None else f"{e}:{d}" for _, e, d in STATS.timeline]

        def after(seq, start=0):
            """index of the first occurrence of each item in order, or None."""
            i = start
            for want in seq:
                try:
                    i = ev.index(want, i) + 1
                except ValueError:
                    return None
            return i

        give = after(["screen_setup", "screen_teardown", f"modes_screen:{ENTITY_ACCESSORY}/{ENTITY_ACCESSORY}"])
        if give is None:
            print(f"FAIL: no screen TEARDOWN before modesChanged(MainScreen=car): {ev}")
            ok = False
        else:
            # between the teardown and the hand-back announcement: nothing else
            # `give` is just past the MainScreen=car that follows the teardown (an
            # earlier one belongs to the initial screen sync at session start).
            k = give - 1
            if ev[k - 1] != "screen_teardown":
                print(f"FAIL: modesChanged(MainScreen=car) not immediately after the screen TEARDOWN: {ev}")
                ok = False
            back = after(["requestUI", f"modes_screen:{ENTITY_CONTROLLER}/{ENTITY_CONTROLLER}", "screen_setup"], give)
            if back is None:
                print(f"FAIL: no modesChanged(MainScreen=phone) + new screen SETUP after requestUI: {ev}")
                ok = False
        if not all(STATS.data_closed_before_teardown):
            print("FAIL: the sender still had its screen data channel open at TEARDOWN.")
            ok = False
        if STATS.frames_after_resume < 30:
            print(f"FAIL: only {STATS.frames_after_resume} frames after the come-back (< 30).")
            ok = False
        if (STATS.screen_entity, STATS.screen_perm_entity) != (ENTITY_CONTROLLER, ENTITY_CONTROLLER):
            print("FAIL: MainScreen not back to the phone (1/1) at the end.")
            ok = False
    print("PASS: session survived and streamed." if ok else "RESULT: session did NOT survive.")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

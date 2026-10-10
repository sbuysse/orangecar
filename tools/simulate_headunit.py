#!/usr/bin/env python3
"""
simulate_headunit.py — Android Auto head unit (car) simulator.

Runs on the laptop. Connects to the OrangePi (18D1:4EE1) via USB,
performs the AOA2 handshake, then drives the Android Auto protocol
as the car's head unit. Received H.264 video is piped to ffplay.

Usage:
    python3 tools/simulate_headunit.py

Requirements:
    pip install pyusb
    apt install ffmpeg  (for ffplay)
"""

import ssl
import struct
import subprocess
import sys
import time

import usb.core
import usb.util

# ── USB identifiers ────────────────────────────────────────────────────────────

GOOGLE_VID      = 0x18D1
PID_DEFAULT     = 0x4EE1   # initial enumeration
PID_ACCESSORY   = 0x2D00   # after AOA2 switch

# AOA2 vendor request codes
AOA_GET_PROTOCOL    = 51
AOA_SEND_STRING     = 52
AOA_START_ACCESSORY = 53

# AOA accessory identification strings (index → value)
AOA_STRINGS = {
    0: "Google",           # manufacturer
    1: "Android Auto",     # model
    2: "Android Auto",     # description
    3: "2.0.0",            # version
    4: "",                 # URI
    5: "",                 # serial
}

# ── AA frame flags ─────────────────────────────────────────────────────────────

F_FIRST     = 0x01
F_LAST      = 0x02
F_CONTROL   = 0x04
F_ENCRYPTED = 0x08

# ── AA control channel message types ──────────────────────────────────────────

MSG_VERSION_REQUEST       = 1
MSG_VERSION_RESPONSE      = 2
MSG_SSL_HANDSHAKE         = 3
MSG_AUTH_COMPLETE         = 4
MSG_SERVICE_DISC_REQUEST  = 5
MSG_SERVICE_DISC_RESPONSE = 6
MSG_CHANNEL_OPEN_REQUEST  = 7
MSG_CHANNEL_OPEN_RESPONSE = 8
MSG_PING_REQUEST          = 11
MSG_PING_RESPONSE         = 12
MSG_BYEBYE_REQUEST        = 15
MSG_BYEBYE_RESPONSE       = 16

# ── AA channel IDs ─────────────────────────────────────────────────────────────

CH_CONTROL     = 0
CH_INPUT       = 1
CH_SENSOR      = 2
CH_VIDEO       = 3
CH_MEDIA_AUDIO = 4
CH_SYS_AUDIO   = 5
CH_MIC         = 7

# ── Non-control channel message types ─────────────────────────────────────────

MSG_MEDIA_DATA           = 0x0000
MSG_MEDIA_SETUP_REQUEST  = 0x8000
MSG_MEDIA_SETUP_RESPONSE = 0x8003

MAX_FRAME_PAYLOAD = 0x4000   # 16384 bytes

# ══════════════════════════════════════════════════════════════════════════════
# USB / AOA layer
# ══════════════════════════════════════════════════════════════════════════════

def find_device(vid: int, pid: int, timeout: float = 20.0) -> usb.core.Device:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        dev = usb.core.find(idVendor=vid, idProduct=pid)
        if dev is not None:
            return dev
        time.sleep(0.5)
    raise TimeoutError(f"Device {vid:04x}:{pid:04x} not found within {timeout:.0f}s")


def aoa2_handshake(dev: usb.core.Device):
    """Send AOA2 control requests to switch the device into accessory mode."""
    # Detach kernel driver if needed (Linux only)
    try:
        if dev.is_kernel_driver_active(0):
            dev.detach_kernel_driver(0)
    except Exception:
        pass

    # GET_PROTOCOL — IN: device returns u16 LE version
    resp = dev.ctrl_transfer(0xC0, AOA_GET_PROTOCOL, 0, 0, 2, timeout=2000)
    version = struct.unpack("<H", bytes(resp))[0]
    print(f"  AOA protocol version: {version}")
    if version < 1:
        raise RuntimeError("Device does not support AOA")

    # SEND_STRING — OUT: send each identification string (null-terminated)
    for idx, text in AOA_STRINGS.items():
        data = (text + "\x00").encode()
        dev.ctrl_transfer(0x40, AOA_SEND_STRING, 0, idx, data, timeout=1000)
        print(f"  SEND_STRING[{idx}] = {text!r}")

    # START_ACCESSORY — OUT: trigger re-enumeration
    dev.ctrl_transfer(0x40, AOA_START_ACCESSORY, 0, 0, None, timeout=1000)
    print("  START_ACCESSORY sent")


def get_bulk_endpoints(dev: usb.core.Device):
    """Return (ep_out, ep_in) for interface 0 of the accessory device."""
    try:
        if dev.is_kernel_driver_active(0):
            dev.detach_kernel_driver(0)
    except Exception:
        pass

    usb.util.claim_interface(dev, 0)
    cfg  = dev.get_active_configuration()
    intf = cfg[(0, 0)]

    ep_out = usb.util.find_descriptor(
        intf,
        custom_match=lambda e: (
            usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_OUT
        ),
    )
    ep_in = usb.util.find_descriptor(
        intf,
        custom_match=lambda e: (
            usb.util.endpoint_direction(e.bEndpointAddress) == usb.util.ENDPOINT_IN
        ),
    )
    if not ep_out or not ep_in:
        raise RuntimeError("Could not find bulk endpoints")
    return ep_out, ep_in


# ══════════════════════════════════════════════════════════════════════════════
# AA transport — framing
# ══════════════════════════════════════════════════════════════════════════════

def _has_total_len(flags: int) -> bool:
    """True when this frame header carries the 4-byte total-message length.

    Only a genuine first-of-many frame does: FIRST set, LAST clear. A single
    frame is FIRST|LAST and carries no total. Both Rust sides gate on exactly
    this (headunit/src/transport.rs encode_frame / decode_frame); adding the
    field to single frames desyncs every message by 4 bytes.
    """
    return bool(flags & F_FIRST) and not (flags & F_LAST)


def _encode_frame(channel: int, flags: int, payload: bytes, total_len: int | None) -> bytes:
    if _has_total_len(flags):
        if total_len is None:
            total_len = len(payload)
        hdr = struct.pack(">BBHI", channel, flags, len(payload), total_len)
    else:
        hdr = struct.pack(">BBH", channel, flags, len(payload))
    return hdr + payload


def send_message(ep_out, channel: int, data: bytes, encrypted: bool = False):
    """Fragment *data* into AA frames and write them to the bulk OUT endpoint."""
    total = len(data)
    offset = 0
    while offset < total:
        is_first = (offset == 0)
        # First frame header is 8 bytes (4 extra for total_len)
        max_chunk = (MAX_FRAME_PAYLOAD - 4) if (is_first and total > MAX_FRAME_PAYLOAD) else MAX_FRAME_PAYLOAD
        chunk = data[offset:offset + max_chunk]
        is_last = (offset + len(chunk)) >= total

        flags = 0
        if is_first:   flags |= F_FIRST
        if is_last:    flags |= F_LAST
        if encrypted:  flags |= F_ENCRYPTED

        frame = _encode_frame(channel, flags, chunk, total if is_first else None)
        ep_out.write(frame, timeout=5000)
        offset += len(chunk)


class Reassembler:
    """Per-channel frame reassembler."""
    def __init__(self):
        self._bufs: dict[int, bytearray] = {}

    def feed(self, channel: int, flags: int, payload: bytes) -> bytes | None:
        if flags & F_FIRST:
            self._bufs[channel] = bytearray(payload)
        else:
            self._bufs.setdefault(channel, bytearray()).extend(payload)
        if flags & F_LAST:
            return bytes(self._bufs.pop(channel, bytearray(payload)))
        return None


class FrameReader:
    """Decodes AA frames from a byte buffer kept across USB transfers.

    One bulk transfer is not one frame: it can carry several messages
    back-to-back, and a frame can straddle two transfers. Parsing exactly one
    header per read and throwing away the remainder silently drops whatever was
    coalesced behind it. This mirrors the Rust `read_message`, which decodes
    from its buffer *before* issuing another read.
    """

    READ_SIZE = 16384

    def __init__(self, ep_in, timeout: int = 10000):
        self.ep_in = ep_in
        self.timeout = timeout
        self._buf = bytearray()

    def next_frame(self) -> tuple[int, int, bytes]:
        """Return the next complete (channel, flags, payload), reading as needed."""
        while True:
            frame = self._decode_one()
            if frame is not None:
                return frame
            chunk = bytes(self.ep_in.read(self.READ_SIZE, timeout=self.timeout))
            if chunk:
                self._buf.extend(chunk)

    def _decode_one(self) -> tuple[int, int, bytes] | None:
        """Pop one frame off the buffer, or None if it is not complete yet."""
        if len(self._buf) < 4:
            return None
        channel = self._buf[0]
        flags   = self._buf[1]
        plen    = struct.unpack(">H", self._buf[2:4])[0]
        offset  = 8 if _has_total_len(flags) else 4
        if len(self._buf) < offset:
            return None
        end = offset + plen
        if len(self._buf) < end:
            return None
        payload = bytes(self._buf[offset:end])
        del self._buf[:end]
        return channel, flags, payload


def recv_message(reader: FrameReader, asm: Reassembler) -> tuple[int, bytes, bool]:
    """Read and reassemble frames until a complete message arrives.
    Returns (channel, data, was_encrypted).
    """
    while True:
        ch, flags, payload = reader.next_frame()
        enc = bool(flags & F_ENCRYPTED)
        msg = asm.feed(ch, flags, payload)
        if msg is not None:
            return ch, msg, enc


# ══════════════════════════════════════════════════════════════════════════════
# AA control message helpers
# ══════════════════════════════════════════════════════════════════════════════

def ctrl_encode(msg_type: int, payload: bytes = b"") -> bytes:
    return struct.pack(">H", msg_type) + payload


def ctrl_decode(data: bytes) -> tuple[int, bytes]:
    if len(data) < 2:
        return 0, b""
    return struct.unpack(">H", data[:2])[0], data[2:]


# ══════════════════════════════════════════════════════════════════════════════
# TLS helper (ssl.MemoryBIO — TLS client)
# ══════════════════════════════════════════════════════════════════════════════

class TLSState:
    def __init__(self):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode    = ssl.CERT_NONE
        self.in_bio  = ssl.MemoryBIO()
        self.out_bio = ssl.MemoryBIO()
        self._obj    = ctx.wrap_bio(self.in_bio, self.out_bio,
                                    server_side=False, server_hostname=None)
        self.done    = False

    # ── handshake step ──────────────────────────────────────────────────────

    def step(self) -> bytes:
        """Run do_handshake(); returns any outgoing bytes (or b'' if none)."""
        try:
            self._obj.do_handshake()
            self.done = True
        except ssl.SSLWantReadError:
            pass
        return self._drain_out()

    def feed(self, data: bytes) -> bytes:
        """Feed incoming TLS bytes; returns outgoing bytes after processing."""
        self.in_bio.write(data)
        return self.step()

    # ── post-handshake encrypt/decrypt ─────────────────────────────────────

    def encrypt(self, plaintext: bytes) -> bytes:
        self._obj.write(plaintext)
        return self._drain_out()

    def decrypt(self, ciphertext: bytes) -> bytes | None:
        """Feed ciphertext; return the plaintext it produced, or None.

        None means "no application data yet" — the bytes were a TLS record that
        carries no payload (session ticket, key update) or an incomplete record.
        That is NOT the same as an empty message, which is why a single
        truncating read() returning b"" was wrong: callers turned it into a fake
        message type 0 and silently ignored it, which can spin forever.

        Drains until SSLWantReadError because one AA message can hold several
        TLS records and read() returns one record's worth at a time.
        """
        self.in_bio.write(ciphertext)
        out = bytearray()
        while True:
            try:
                chunk = self._obj.read(65536)
            except ssl.SSLWantReadError:
                break
            except ssl.SSLZeroReturnError:
                break
            if not chunk:
                break
            out.extend(chunk)
        return bytes(out) if out else None

    def _drain_out(self) -> bytes:
        n = self.out_bio.pending
        return self.out_bio.read(n) if n else b""


# ══════════════════════════════════════════════════════════════════════════════
# High-level AA protocol
# ══════════════════════════════════════════════════════════════════════════════

class HeadUnit:
    def __init__(self, ep_out, ep_in):
        self.ep_out = ep_out
        self.ep_in  = ep_in
        self.reader = FrameReader(ep_in)
        self.asm    = Reassembler()
        self.tls    = TLSState()

    # ── raw send ────────────────────────────────────────────────────────────

    def _send_ctrl(self, msg_type: int, payload: bytes = b"", encrypted: bool = False):
        data = ctrl_encode(msg_type, payload)
        if encrypted:
            data = self.tls.encrypt(data)
        send_message(self.ep_out, CH_CONTROL, data, encrypted=encrypted)

    def _send_channel(self, channel: int, msg_type: int, payload: bytes, encrypted: bool = True):
        data = struct.pack(">H", msg_type) + payload
        if encrypted:
            data = self.tls.encrypt(data)
        send_message(self.ep_out, channel, data, encrypted=encrypted)

    # ── raw recv ────────────────────────────────────────────────────────────

    def recv(self) -> tuple[int, int, bytes]:
        """Receive one complete (and decrypted) message.
        Returns (channel, msg_type, payload).
        """
        while True:
            ch, raw, was_enc = recv_message(self.reader, self.asm)
            if was_enc:
                plain = self.tls.decrypt(raw)
                if plain is None:
                    # TLS record with no application data — not a message.
                    # Keep reading rather than reporting a bogus type 0.
                    continue
                raw = plain
            if len(raw) < 2:
                return ch, 0, raw
            mt = struct.unpack(">H", raw[:2])[0]
            return ch, mt, raw[2:]

    # ── TLS handshake ────────────────────────────────────────────────────────

    def do_tls_handshake(self):
        print("  Starting TLS client handshake...")
        # Initiate: this produces the ClientHello
        out = self.tls.step()
        if out:
            self._send_ctrl(MSG_SSL_HANDSHAKE, out, encrypted=False)
            print(f"  → SSL ClientHello ({len(out)} bytes)")

        while not self.tls.done:
            ch, raw, _ = recv_message(self.reader, self.asm)
            mt, payload = ctrl_decode(raw)
            if ch != CH_CONTROL or mt != MSG_SSL_HANDSHAKE:
                print(f"  [TLS] unexpected ch={ch} type={mt:#06x} — skipping")
                continue
            print(f"  ← SSL data ({len(payload)} bytes)")
            out = self.tls.feed(payload)
            if out:
                self._send_ctrl(MSG_SSL_HANDSHAKE, out, encrypted=False)
                print(f"  → SSL response ({len(out)} bytes)")

        print("  TLS handshake complete")

    # ── high-level steps ─────────────────────────────────────────────────────

    def version_exchange(self):
        payload = struct.pack(">HH", 1, 7)   # major=1, minor=7
        self._send_ctrl(MSG_VERSION_REQUEST, payload, encrypted=False)
        print("  → VERSION_REQUEST 1.7")
        while True:
            ch, mt, pl = self.recv()
            if ch == CH_CONTROL and mt == MSG_VERSION_RESPONSE:
                # Length-check before unpacking: a short response used to abort
                # the whole simulator with a struct.error traceback.
                if len(pl) < 6:
                    raise IOError(
                        f"VERSION_RESPONSE too short: {len(pl)} bytes, need 6"
                    )
                maj, mn, status = struct.unpack(">HHH", pl[:6])
                print(f"  ← VERSION_RESPONSE {maj}.{mn} status={status}")
                if status != 0:
                    raise RuntimeError(
                        f"bridge reported version status {status} (expected 0 = MATCH)"
                    )
                return
            print(f"  [version] unexpected ch={ch} type={mt:#06x}")

    def auth_complete(self):
        self._send_ctrl(MSG_AUTH_COMPLETE, b"", encrypted=True)
        print("  → AUTH_COMPLETE")

    def service_discovery(self):
        print("  Waiting for SERVICE_DISCOVERY_REQUEST...")
        while True:
            ch, mt, pl = self.recv()
            if ch == CH_CONTROL:
                if mt == MSG_SERVICE_DISC_REQUEST:
                    print(f"  ← SERVICE_DISCOVERY_REQUEST ({len(pl)} bytes)")
                    break
                if mt == MSG_PING_REQUEST:
                    self._send_ctrl(MSG_PING_RESPONSE, pl, encrypted=True)
            else:
                print(f"  [sdr] ignoring ch={ch} type={mt:#06x}")

        # Respond with minimal OK protobuf (field 1 = status = 0)
        self._send_ctrl(MSG_SERVICE_DISC_RESPONSE, bytes([0x08, 0x00]), encrypted=True)
        print("  → SERVICE_DISCOVERY_RESPONSE (OK)")

    def open_channels(self, n: int = 6):
        print(f"  Waiting for {n} CHANNEL_OPEN_REQUESTs...")
        opened = 0
        while opened < n:
            ch, mt, pl = self.recv()
            if ch == CH_CONTROL:
                if mt == MSG_CHANNEL_OPEN_REQUEST:
                    # payload: 0x08 <channel_id>
                    ch_id = pl[1] if len(pl) >= 2 else 0
                    print(f"  ← CHANNEL_OPEN_REQUEST ch={ch_id}")
                    # respond with channel_id + status OK
                    resp = bytes([0x08, ch_id, 0x10, 0x00])
                    self._send_ctrl(MSG_CHANNEL_OPEN_RESPONSE, resp, encrypted=True)
                    print(f"  → CHANNEL_OPEN_RESPONSE ch={ch_id} OK")
                    opened += 1
                elif mt == MSG_PING_REQUEST:
                    self._send_ctrl(MSG_PING_RESPONSE, pl, encrypted=True)
            else:
                print(f"  [open] ignoring ch={ch} type={mt:#06x}")

    def request_video(self):
        """Send a video media setup request (480p 30fps)."""
        # protobuf: field1=resolution(1=480p), field2=fps(1=30), field5=dpi(160)
        pb = bytes([
            0x08, 0x01,        # field 1 = resolution = 1 (480p)
            0x10, 0x01,        # field 2 = fps = 1 (30 fps)
            0x28, 0xa0, 0x01,  # field 5 = dpi = 160 (varint)
        ])
        self._send_channel(CH_VIDEO, MSG_MEDIA_SETUP_REQUEST, pb, encrypted=True)
        print("  → VIDEO_SETUP_REQUEST (480p 30fps)")

    def stream_video(self):
        """Receive video frames and pipe H.264 to ffplay. Blocks until done."""
        ffplay = subprocess.Popen(
            [
                "ffplay",
                "-f", "h264",
                "-fflags", "nobuffer",
                "-flags", "low_delay",
                "-framedrop",
                "-window_title", "RasCar Android Auto",
                "-",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        print("  ffplay started — waiting for H.264 frames (Ctrl+C to stop)...")
        frames_rx = 0
        try:
            while True:
                try:
                    ch, mt, pl = self.recv()
                except usb.core.USBTimeoutError:
                    continue

                if ch == CH_CONTROL:
                    if mt == MSG_PING_REQUEST:
                        self._send_ctrl(MSG_PING_RESPONSE, pl, encrypted=True)
                    elif mt == MSG_BYEBYE_REQUEST:
                        print("\n  ← BYEBYE_REQUEST — shutting down")
                        self._send_ctrl(MSG_BYEBYE_RESPONSE, b"", encrypted=True)
                        break

                elif ch == CH_VIDEO:
                    if mt == MSG_MEDIA_SETUP_RESPONSE:
                        print(f"  ← VIDEO_SETUP_RESPONSE ({len(pl)} bytes)")
                    elif mt == MSG_MEDIA_DATA:
                        # pl = timestamp(8 BE) | H.264 Annex B data
                        h264 = pl[8:]
                        if h264 and ffplay.stdin:
                            try:
                                ffplay.stdin.write(h264)
                                ffplay.stdin.flush()
                            except BrokenPipeError:
                                print("\n  ffplay closed")
                                return
                            frames_rx += 1
                            if frames_rx % 30 == 0:
                                print(f"\r  frames received: {frames_rx}", end="", flush=True)

                elif ch == CH_SENSOR:
                    pass  # driving status etc. — ignore

        except KeyboardInterrupt:
            print(f"\n  Stopped — {frames_rx} frames received")
        finally:
            if ffplay.stdin:
                ffplay.stdin.close()
            ffplay.wait()


# ══════════════════════════════════════════════════════════════════════════════
# Main
# ══════════════════════════════════════════════════════════════════════════════

def main():
    print("╔══════════════════════════════════════════════╗")
    print("║   RasCar — Android Auto Head Unit Simulator  ║")
    print("╚══════════════════════════════════════════════╝\n")

    # ── Phase 1: AOA2 handshake ────────────────────────────────────────────
    print("[1] Looking for OrangePi (18D1:4EE1)...")
    dev = find_device(GOOGLE_VID, PID_DEFAULT)
    try:
        print(f"  Found: {dev.manufacturer or '?'} {dev.product or '?'}")
    except Exception:
        print("  Found device (no string descriptors)")

    print("[2] Sending AOA2 handshake...")
    aoa2_handshake(dev)

    # ── Phase 2: Wait for re-enumeration as accessory ─────────────────────
    print("[3] Waiting for accessory device (18D1:2D00)...")
    dev = find_device(GOOGLE_VID, PID_ACCESSORY, timeout=20.0)
    print("  Device re-enumerated as accessory!")

    ep_out, ep_in = get_bulk_endpoints(dev)
    print(f"  ep_out=0x{ep_out.bEndpointAddress:02x}  ep_in=0x{ep_in.bEndpointAddress:02x}")

    hu = HeadUnit(ep_out, ep_in)

    # ── Phase 3: Version exchange (unencrypted) ───────────────────────────
    print("\n[4] Version exchange...")
    hu.version_exchange()

    # ── Phase 4: TLS handshake ─────────────────────────────────────────────
    print("\n[5] TLS handshake...")
    hu.do_tls_handshake()

    # ── Phase 5: Auth complete ─────────────────────────────────────────────
    print("\n[6] Auth complete...")
    hu.auth_complete()

    # ── Phase 6: Service discovery ─────────────────────────────────────────
    print("\n[7] Service discovery...")
    hu.service_discovery()

    # ── Phase 7: Channel opens ─────────────────────────────────────────────
    print("\n[8] Channel open handshake...")
    hu.open_channels(n=6)

    # ── Phase 8: Request video ─────────────────────────────────────────────
    print("\n[9] Requesting video stream...")
    hu.request_video()

    # ── Phase 9: Receive and display video ────────────────────────────────
    print("\n[10] Streaming...")
    hu.stream_video()

    # ── Cleanup ────────────────────────────────────────────────────────────
    usb.util.release_interface(dev, 0)
    print("\nDone.")


if __name__ == "__main__":
    main()

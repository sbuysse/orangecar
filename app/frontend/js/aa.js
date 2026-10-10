/**
 * Android Auto page: shows the phone's projected screen and sends touches back.
 *
 * rascar-aa (headunit/src/bin/rascar_aa.rs) forwards the phone's H.264 access
 * units unchanged over a WebSocket; each binary message is
 *   [0] kind (1 = video) · [1] flags (bit0 CONFIG = SPS/PPS, bit1 KEY = IDR)
 *   [2..10] timestamp µs (u64 BE) · [10..] Annex-B bytes.
 * They are decoded with WebCodecs (no `description` → Annex-B input; Chromium
 * 146 on the Pi decodes H.264 in software, measured 2026-10-05). Only the
 * part of the frame the phone draws into (`crop`, from the status message) is
 * shown, letterboxed to keep its aspect ratio. Pointer events become
 * {"type":"touch"} messages with coordinates normalised over that content.
 *
 * Query parameters: ?ws=ws://host:port/ overrides the server, ?debug=1 shows stats.
 */
(function () {
    'use strict';

    const params = new URLSearchParams(location.search);
    const WS_URL = params.get('ws') || `ws://${location.hostname || '127.0.0.1'}:8091/`;
    const FLAG_CONFIG = 0x01;
    const FLAG_KEY = 0x02;
    const RETRY_MS = 2000;
    /// A decoder this many frames behind for STUCK_MS is restarted. A joining
    /// page legitimately queues a whole cached GOP at once (catch-up burst),
    /// so a deep queue alone means nothing — only a queue that stays deep.
    const MAX_QUEUE = 60;
    const STUCK_MS = 3000;
    let behindSince = 0;

    const canvas = document.getElementById('screen');
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    const overlay = document.getElementById('overlay');
    const statsEl = document.getElementById('stats');
    const log = (m) => { console.log('[aa]', m); window.RasCar?.kioskLog?.(`aa: ${m}`); };

    let ws = null;
    let decoder = null;
    let codec = null;            // 'avc1.PPCCLL' of the configured decoder
    let config = null;           // last SPS/PPS (prepended to every keyframe)
    let waitingKey = true;       // drop deltas until a keyframe after (re)configure
    let status = { state: 'connecting', streaming: false, crop: { x: 0, y: 0, w: 1280, h: 530 }, width: 1280, height: 720 };
    let firstFrameShown = false;

    const stats = { received: 0, decoded: 0, skipped: 0, errors: 0, bytes: 0, fps: 0, decodeMs: 0, lastTs: performance.now(), frames: 0 };
    window.aaStats = stats;      // read by benchmarks over DevTools
    if (params.get('debug')) statsEl.classList.remove('hidden');

    // ── Overlay ──────────────────────────────────────────────────────────

    function show(icon, title, detail, isError) {
        document.getElementById('icon').className = icon;
        document.getElementById('title').textContent = title;
        document.getElementById('detail').textContent = detail || '';
        overlay.classList.toggle('error', !!isError);
        overlay.classList.remove('hidden');
    }

    function renderStatus() {
        const phone = status.phone || 'your phone';
        if (!ws || ws.readyState !== WebSocket.OPEN) {
            show('fas fa-plug-circle-xmark', 'Android Auto service not running', 'Retrying…', true);
        } else if (status.state === 'waiting') {
            show('fab fa-usb', 'Connect your phone',
                'Plug the phone into a USB port of the RasCar with a data cable and unlock it.'
                + (status.error ? `\nLast session: ${status.error}` : ''));
        } else if (status.state === 'closed') {
            show('fas fa-circle-exclamation', 'Android Auto stopped', status.error || 'The phone ended the session.', true);
        } else if (!status.streaming || !firstFrameShown) {
            const step = { version: 'Saying hello to', tls: 'Securing the link with', discovery: 'Talking to', running: 'Starting Android Auto on' }[status.state] || 'Connecting to';
            show('fab fa-android', 'Android Auto', `${step} ${phone}…`);
        } else {
            overlay.classList.add('hidden');
        }
    }

    // ── Layout: canvas = phone content, letterboxed in the frame ─────────

    function layout() {
        const c = status.crop;
        if (canvas.width !== c.w || canvas.height !== c.h) {
            canvas.width = c.w;
            canvas.height = c.h;
        }
        const scale = Math.min(innerWidth / c.w, innerHeight / c.h);
        canvas.style.width = `${Math.floor(c.w * scale)}px`;
        canvas.style.height = `${Math.floor(c.h * scale)}px`;
    }
    addEventListener('resize', layout);

    // ── Decoding ─────────────────────────────────────────────────────────

    const hex2 = (b) => b.toString(16).padStart(2, '0').toUpperCase();

    /** Find NAL units in an Annex-B buffer: [{type, start, end}] (start = first header byte). */
    function nals(buf) {
        const out = [];
        for (let i = 0; i + 3 < buf.length; i++) {
            if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 1) {
                if (out.length) out[out.length - 1].end = (buf[i - 1] === 0 ? i - 1 : i);
                out.push({ type: buf[i + 3] & 0x1f, start: i + 3, end: buf.length });
                i += 2;
            }
        }
        return out;
    }

    /** RFC 6381 codec string from the SPS: avc1.<profile><constraints><level>. */
    function codecFromConfig(buf) {
        const sps = nals(buf).find(n => n.type === 7);
        if (!sps || sps.end - sps.start < 4) return null;
        return `avc1.${hex2(buf[sps.start + 1])}${hex2(buf[sps.start + 2])}${hex2(buf[sps.start + 3])}`;
    }

    function onFrame(frame) {
        const c = status.crop;
        // Draw only the content rectangle; the margins the phone leaves black are cut off.
        ctx.drawImage(frame, c.x, c.y, c.w, c.h, 0, 0, canvas.width, canvas.height);
        frame.close();
        stats.decoded++;
        stats.frames++;
        if (!firstFrameShown) {
            firstFrameShown = true;
            canvas.classList.add('live');
            log(`first frame shown (${codec}, ${frame.codedWidth}x${frame.codedHeight})`);
            renderStatus();
        }
    }

    function resetDecoder(reason) {
        if (reason) log(`decoder reset: ${reason}`);
        if (decoder && decoder.state !== 'closed') { try { decoder.close(); } catch (e) { /* already closed */ } }
        decoder = null;
        codec = null;
        waitingKey = true;
    }

    function requestKeyframe() {
        if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: 'keyframe' }));
    }

    /* Synchronous on purpose: the IDR follows its SPS/PPS immediately, and
       awaiting isConfigSupported() here would drop it (decoder not ready yet)
       and leave the screen black until the phone's next IDR, which can be
       minutes away. Support was checked up front (see checkSupport). */
    function configure(newCodec) {
        resetDecoder();
        if (!newCodec) return;
        decoder = new VideoDecoder({
            output: onFrame,
            error: (e) => {
                stats.errors++;
                resetDecoder(`error ${e.message}`);
                requestKeyframe();
            },
        });
        try {
            decoder.configure({ codec: newCodec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
        } catch (e) {
            resetDecoder(`configure ${newCodec} threw ${e.message}`);
            show('fas fa-circle-exclamation', 'Cannot decode the phone video', String(e.message || e), true);
            return;
        }
        codec = newCodec;
        waitingKey = true;
        log(`decoder configured: ${newCodec}`);
    }

    async function checkSupport() {
        try {
            const r = await VideoDecoder.isConfigSupported({ codec: 'avc1.42E01F', codedWidth: 1280, codedHeight: 720 });
            if (!r.supported) log('H.264 baseline reported unsupported by VideoDecoder');
        } catch (e) { log(`isConfigSupported failed: ${e.message}`); }
    }

    function concat(a, b) {
        const out = new Uint8Array(a.length + b.length);
        out.set(a, 0);
        out.set(b, a.length);
        return out;
    }

    function onVideo(buf) {
        const bytes = new Uint8Array(buf);
        if (bytes.length < 10 || bytes[0] !== 1) return;
        const flags = bytes[1];
        const ts = Number(new DataView(buf).getBigUint64(2));
        const data = bytes.subarray(10);
        stats.received++;
        stats.bytes += data.length;

        if (flags & FLAG_CONFIG) {
            config = data.slice();
            const c = codecFromConfig(config);
            if (c && c !== codec) configure(c);
            waitingKey = true;
            return;
        }
        if (!decoder || decoder.state !== 'configured') { stats.skipped++; return; }
        const key = !!(flags & FLAG_KEY);
        if (waitingKey && !key) { stats.skipped++; return; }
        if (decoder.decodeQueueSize > MAX_QUEUE) {
            const now = performance.now();
            if (!behindSince) behindSince = now;
            if (now - behindSince > STUCK_MS) {
                behindSince = 0;
                stats.skipped++;
                resetDecoder(`decoder ${decoder.decodeQueueSize} frames behind for ${STUCK_MS} ms`);
                if (config) configure(codecFromConfig(config));
                requestKeyframe();
                return;
            }
        } else {
            behindSince = 0;
        }
        try {
            decoder.decode(new EncodedVideoChunk({
                type: key ? 'key' : 'delta',
                timestamp: ts,
                data: key && config ? concat(config, data) : data,
            }));
            waitingKey = false;
        } catch (e) {
            stats.errors++;
            resetDecoder(`decode threw ${e.message}`);
            requestKeyframe();
        }
    }

    // ── Touch ────────────────────────────────────────────────────────────

    function sendTouch(action, e) {
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        const r = canvas.getBoundingClientRect();
        if (!r.width || !r.height) return;
        const x = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
        const y = Math.min(1, Math.max(0, (e.clientY - r.top) / r.height));
        ws.send(JSON.stringify({ type: 'touch', action, id: e.pointerId, x, y }));
    }

    canvas.addEventListener('pointerdown', (e) => {
        canvas.setPointerCapture(e.pointerId);
        sendTouch('down', e);
        e.preventDefault();
    });
    canvas.addEventListener('pointermove', (e) => {
        if (canvas.hasPointerCapture(e.pointerId)) sendTouch('move', e);
    });
    canvas.addEventListener('pointerup', (e) => sendTouch('up', e));
    canvas.addEventListener('pointercancel', (e) => sendTouch('cancel', e));
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());

    // ── WebSocket ────────────────────────────────────────────────────────

    function connect() {
        ws = new WebSocket(WS_URL);
        ws.binaryType = 'arraybuffer';
        ws.onopen = () => { log(`connected to ${WS_URL}`); renderStatus(); };
        ws.onmessage = (ev) => {
            if (typeof ev.data !== 'string') { onVideo(ev.data); return; }
            let msg;
            try { msg = JSON.parse(ev.data); } catch (e) { return; }
            if (msg.type === 'status') {
                const wasStreaming = status.streaming;
                status = msg;
                layout();
                if (!msg.streaming && wasStreaming) {
                    firstFrameShown = false;
                    canvas.classList.remove('live');
                }
                renderStatus();
            } else if (msg.type === 'touch-echo') {
                window.aaLastTouchEcho = msg;   // replay mode: what the phone would receive
                if (params.get('debug')) console.log('[aa] touch-echo', JSON.stringify(msg));
            }
        };
        ws.onclose = () => {
            ws = null;
            resetDecoder();
            config = null;
            firstFrameShown = false;
            canvas.classList.remove('live');
            renderStatus();
            setTimeout(connect, RETRY_MS);
        };
        ws.onerror = () => { /* onclose follows and retries */ };
    }

    setInterval(() => {
        const now = performance.now();
        stats.fps = Math.round(stats.frames * 1000 / (now - stats.lastTs));
        stats.frames = 0;
        stats.lastTs = now;
        if (!statsEl.classList.contains('hidden')) {
            statsEl.textContent = `${status.state} · ${codec || '-'} · ${stats.fps} fps · queue ${decoder ? decoder.decodeQueueSize : '-'}`
                + ` · rx ${stats.received} dec ${stats.decoded} skip ${stats.skipped} err ${stats.errors}`;
        }
    }, 1000);

    if (typeof VideoDecoder === 'undefined') {
        show('fas fa-circle-exclamation', 'This browser cannot decode video', 'WebCodecs (VideoDecoder) is unavailable.', true);
        log('WebCodecs unavailable');
        return;
    }
    layout();
    renderStatus();
    checkSupport();
    connect();
})();

/**
 * RasCar Media — fullscreen view, 16 FFT visualizer modes, karaoke, playlists.
 *
 * Visualizer uses real Web Audio API frequency data from the radio/media stream.
 * Each of the 16 modes is a distinct canvas draw function operating on FFT bins.
 */
(function() {
    // ── State ────────────────────────────────────────────────────
    // LED bars by default; the chosen mode is remembered on this screen.
    const VIZ_MODE_KEY = 'rascar-viz-mode';
    let vizMode = 6;
    try { const m = parseInt(localStorage.getItem(VIZ_MODE_KEY), 10); if (m >= 0 && m < 16) vizMode = m; } catch (e) { /* storage unavailable: default */ }
    const VIZ_MODES = [
        'Bars', 'Wave', 'Circles', 'Mirrored', 'Radial', 'Bars+Peaks',
        'LED Bars', 'Fire', 'Half Circle', 'Waterfall', 'Quad Mirror',
        'Radial Wedges', 'Tunnel', 'Orbs', 'Spiral', 'Scope'
    ];
    let karaokeActive = false;
    let fsAutoHideTimer = null;
    let fsControlsVisible = true;
    // Lyric lines as delivered by /api/media/lyrics: {time_sec: number|null, text}.
    let lyricLines = [];
    let lyricIndex = 0;
    let lyricTimer = null;
    // True when the fetched lyrics carry real LRC timestamps we can sync against.
    let lyricsSynced = false;

    // Visualizer engine state (shared between home + fullscreen)
    let audioCtx = null;
    let analyser = null;
    let source = null;
    let vizRunning = false;
    let freqData = new Uint8Array(128);   // what the modes draw (audible band, stretched)
    let rawFreq = new Uint8Array(128);    // analyser output, full 0..Nyquist
    // Upper edge of what internet radio actually carries (MP3/AAC low-pass).
    const AUDIBLE_TOP_HZ = 16000;
    let timeData = new Uint8Array(128);


    // ── Init ─────────────────────────────────────────────────────
    function init() {
        refreshVizAvailability();
        startVizLoop();
        // Pause the rAF loop when the tab/screen is hidden; resume when shown.
        document.addEventListener('visibilitychange', () => {
            if (document.hidden) stopVizLoop();
            else startVizLoop();
        });
    }

    // True when we have a live audio source feeding the analyser.
    let audioConnected = false;
    // The captured stream, kept so its tracks can be stopped on switch/teardown.
    let currentStream = null;
    // The element `currentStream` was captured from, so it can be reused on resume.
    let currentStreamEl = null;

    // Frames from the backend (services/visualizer.rs): the spectrum of
    // everything the Pi plays — YouTube Music, radio, anything — like the phone
    // visualizer of AndroidAuto2. Preferred over the page's own analyser.
    let backendVizAt = 0;
    const BACKEND_VIZ_STALE_MS = 1000;
    let backendVizWasLive = false;
    const backendVizLive = () => Date.now() - backendVizAt < BACKEND_VIZ_STALE_MS;
    const hasAudio = () => audioConnected || backendVizLive();

    // The backend sends ~30 frames/s but the car's screen is captured at 20 fps
    // (RASCAR_VIDEO_FPS): drawing each frame as it lands made 1 in 3 vanish
    // unevenly, and the bars juddered. Every rAF draws a blend between the last
    // two frames instead, so any capture rate samples smooth motion, one frame late.
    let vizFrom = { f: new Float32Array(128), w: new Float32Array(128) };
    let vizTo = { f: new Float32Array(128), w: new Float32Array(128) };
    let vizToAt = 0;
    let vizPeriod = 33;

    function onVizFrame(msg) {
        if (!Array.isArray(msg.f) || !Array.isArray(msg.w)) return;
        if (freqData.length !== msg.f.length || vizTo.f.length !== msg.f.length) {
            freqData = new Uint8Array(msg.f.length); timeData = new Uint8Array(msg.w.length);
            vizFrom = { f: new Float32Array(msg.f.length), w: new Float32Array(msg.w.length) };
            vizTo = { f: new Float32Array(msg.f.length), w: new Float32Array(msg.w.length) };
        }
        const now = performance.now();
        // Start from what is on screen now, so a late or early frame never jumps.
        vizFrom.f.set(freqData);
        vizFrom.w.set(timeData);
        vizTo.f.set(msg.f);
        vizTo.w.set(msg.w);
        const gap = now - vizToAt;
        if (gap > 5 && gap < 200) vizPeriod += (gap - vizPeriod) * 0.2;
        vizToAt = now;
        backendVizAt = Date.now();
        if (!backendVizWasLive) { backendVizWasLive = true; refreshVizAvailability(); }
        startVizLoop();
    }

    // Should the visualizer be drawing right now?
    function vizShouldRun() {
        if (document.hidden) return false;
        if (!hasAudio()) {
            if (backendVizWasLive) { backendVizWasLive = false; refreshVizAvailability(); }
            return false;
        }
        const fsActive = document.getElementById('media-fullscreen')?.classList.contains('active');
        // Home canvas only matters on the media page (page 4); fullscreen has its own flag.
        const onMediaPage = (typeof currentPage !== 'undefined') ? currentPage === 4 : true;
        return fsActive || onMediaPage;
    }

    // ── Audio connection (called by radio.js / app.js) ───────────
    function connectToAudio(audioEl) {
        try {
            if (!audioCtx) {
                audioCtx = new (window.AudioContext || window.webkitAudioContext)();
                analyser = audioCtx.createAnalyser();
                analyser.fftSize = 256;
                analyser.smoothingTimeConstant = 0.75;
                freqData = new Uint8Array(analyser.frequencyBinCount);
                rawFreq = new Uint8Array(analyser.frequencyBinCount);
                timeData = new Uint8Array(analyser.frequencyBinCount);
            }
            if (audioCtx.state === 'suspended') audioCtx.resume();
            if (source) { source.disconnect(); source = null; }
            // Reuse the existing capture when it is the same element and still live.
            // captureStream() is not guaranteed to hand back a usable stream once the
            // previous one's tracks were stopped, so never stop them speculatively.
            const reusable = currentStream
                && currentStreamEl === audioEl
                && currentStream.getTracks().some(t => t.readyState === 'live');
            if (!reusable) {
                if (currentStream) currentStream.getTracks().forEach(t => t.stop());
                currentStream = audioEl.captureStream();
                currentStreamEl = audioEl;
            }
            source = audioCtx.createMediaStreamSource(currentStream);
            source.connect(analyser);
            audioConnected = true;
            refreshVizAvailability();
            startVizLoop(); // (re)start now that audio is flowing
        } catch (e) { console.warn('media.connectToAudio:', e); }
    }

    // Pause: detach the analyser but keep the captured stream and the AudioContext,
    // so resuming the same element does not depend on a second captureStream().
    function pauseAudio() {
        audioConnected = false;
        if (source) { try { source.disconnect(); } catch (e) {} source = null; }
        stopVizLoop();
        refreshVizAvailability();
    }

    // Genuine teardown: release every audio resource.
    function disconnectAudio() {
        audioConnected = false;
        if (source) { try { source.disconnect(); } catch (e) {} source = null; }
        if (currentStream) {
            currentStream.getTracks().forEach(t => t.stop());
            currentStream = null;
            currentStreamEl = null;
        }
        if (audioCtx) {
            audioCtx.close().catch(() => {});
            audioCtx = null;
            analyser = null;
        }
        stopVizLoop();
        refreshVizAvailability();
    }

    // ── Viz mode cycling ─────────────────────────────────────────
    // Only the page's own <audio> element (radio) reaches the analyser; MPRIS /
    // bridge audio never passes through this document's audio graph, so the canvas
    // would stay black. Rather than advertise a capability we don't have, the
    // controls report themselves unavailable while no audio source is connected.
    const VIZ_UNAVAILABLE_LABEL = 'Visualizer — no audio source';

    function refreshVizAvailability() {
        const label = hasAudio() ? VIZ_MODES[vizMode] : VIZ_UNAVAILABLE_LABEL;
        ['home-viz-mode', 'fs-viz-mode'].forEach(id => {
            const el = document.getElementById(id);
            if (el) el.textContent = label;
        });
        const btn = document.getElementById('fs-viz-btn');
        if (btn) btn.disabled = !hasAudio();
    }

    function nextVizMode() {
        if (!hasAudio()) return; // nothing to visualize — don't pretend otherwise
        vizMode = (vizMode + 1) % VIZ_MODES.length;
        try { localStorage.setItem(VIZ_MODE_KEY, String(vizMode)); } catch (e) { /* not persisted */ }
        if (vizMode !== 9) waterfallReset();
        refreshVizAvailability();
    }

    // ── Visualizer loop ──────────────────────────────────────────
    let vizRafId = null;

    // Canvas colours come from the theme tokens (no colour redefined here), and
    // are re-read when the theme changes (app.js dispatches 'rascar-theme').
    const theme = { bg: '', fsBg: '', accent: '', accentFill: '', accentFaint: '' };
    function readThemeColors() {
        const css = getComputedStyle(document.documentElement);
        theme.bg = css.getPropertyValue('--bg-secondary').trim();
        theme.fsBg = css.getPropertyValue('--bg-primary').trim();
        theme.accent = css.getPropertyValue('--accent').trim();
        theme.accentFill = `color-mix(in srgb, ${theme.accent} 15%, transparent)`;
        theme.accentFaint = `color-mix(in srgb, ${theme.accent} 8%, transparent)`;
    }
    document.addEventListener('rascar-theme', readThemeColors);

    function startVizLoop() {
        if (vizRunning) return;
        if (!vizShouldRun()) return; // don't spin when nothing should be drawn
        vizRunning = true;

        readThemeColors();
        readPalette();

        function frame() {
            // Stop scheduling as soon as we shouldn't be running anymore.
            if (!vizRunning || !vizShouldRun()) { stopVizLoop(); return; }
            vizRafId = requestAnimationFrame(frame);

            lastBandFrame++;
            bandNow = performance.now() / 1000;
            // Look up canvases each frame (cheap) so page swaps stay correct.
            const homeCanvas = document.getElementById('visualizer-canvas');
            const fsActive = document.getElementById('media-fullscreen')?.classList.contains('active');
            const fsCanvas = document.getElementById('fs-viz-canvas');

            if (backendVizLive()) {
                const a = Math.min(1, (performance.now() - vizToAt) / vizPeriod);
                for (let i = 0; i < freqData.length; i++) freqData[i] = vizFrom.f[i] + (vizTo.f[i] - vizFrom.f[i]) * a;
                for (let i = 0; i < timeData.length; i++) timeData[i] = vizFrom.w[i] + (vizTo.w[i] - vizFrom.w[i]) * a;
            } else if (analyser) {
                // Get FFT data from the page's own analyser.
                analyser.getByteFrequencyData(rawFreq);
                analyser.getByteTimeDomainData(timeData);
                // Streamed radio is low-passed around 16 kHz, so the top third of
                // the bins is always empty and every mode left a dead band on the
                // right. Stretch the audible part over the full width instead.
                const nyquist = (audioCtx?.sampleRate || 48000) / 2;
                const used = Math.max(8, Math.floor(rawFreq.length * Math.min(1, AUDIBLE_TOP_HZ / nyquist)));
                for (let i = 0; i < freqData.length; i++) {
                    const pos = (i / (freqData.length - 1)) * (used - 1);
                    const lo = Math.floor(pos), hi = Math.min(used - 1, lo + 1), f = pos - lo;
                    freqData[i] = rawFreq[lo] * (1 - f) + rawFreq[hi] * f;
                }
            }

            // Draw home visualizer (small) — only when the media page is showing.
            if (homeCanvas && (typeof currentPage === 'undefined' || currentPage === 4)) {
                const homeCtx = homeCanvas.getContext('2d', { alpha: false });
                if (homeCtx) drawMode(homeCtx, homeCanvas.width, homeCanvas.height, theme.bg);
            }

            // Draw fullscreen visualizer (only when visible)
            if (fsCanvas && fsActive) {
                const fsCtx = fsCanvas.getContext('2d', { alpha: false });
                if (fsCtx) {
                    if (fsCanvas.width !== window.innerWidth || fsCanvas.height !== window.innerHeight) {
                        fsCanvas.width = window.innerWidth;
                        fsCanvas.height = window.innerHeight;
                    }
                    drawMode(fsCtx, fsCanvas.width, fsCanvas.height, theme.fsBg);
                }
            }
        }
        frame();
    }

    function stopVizLoop() {
        vizRunning = false;
        if (vizRafId !== null) { cancelAnimationFrame(vizRafId); vizRafId = null; }
    }

    // ── Mode router ──────────────────────────────────────────────
    function drawMode(ctx, W, H, bg) {
        switch (vizMode) {
            case 0:  drawBars(ctx, W, H, bg); break;
            case 1:  drawWave(ctx, W, H, bg); break;
            case 2:  drawCircles(ctx, W, H, bg); break;
            case 3:  drawMirrored(ctx, W, H, bg); break;
            case 4:  drawRadial(ctx, W, H, bg); break;
            case 5:  drawBarsWithPeaks(ctx, W, H, bg); break;
            case 6:  drawLedBars(ctx, W, H, bg); break;
            case 7:  drawFire(ctx, W, H, bg); break;
            case 8:  drawHalfCircle(ctx, W, H, bg); break;
            case 9:  drawWaterfall(ctx, W, H, bg); break;
            case 10: drawQuadMirror(ctx, W, H, bg); break;
            case 11: drawRadialWedges(ctx, W, H, bg); break;
            case 12: drawTunnel(ctx, W, H, bg); break;
            case 13: drawOrbs(ctx, W, H, bg); break;
            case 14: drawSpiral(ctx, W, H, bg); break;
            case 15: drawLissajous(ctx, W, H, bg); break;
            default: drawLedBars(ctx, W, H, bg);
        }
    }

    // ── Shared drawing engine ────────────────────────────────────
    // Bars average their whole slice of the (log-spaced) spectrum — one bin per
    // bar aliased and flickered — then a display pass rises fast and falls
    // quickly, like AndroidAuto2. One display state per bar count, so two
    // canvases (home + fullscreen) of the same mode stay in step.
    // The dynamics are in seconds, not frames: the kiosk's rAF runs at the
    // capture rate (25 fps), and the old per-frame linear fall (0.025/frame)
    // took 1.4 s to empty a bar, so the meter never dropped between beats.
    const BAR_RISE_S = 0.025;   // attack time constant
    const BAR_FALL_S = 0.06;    // release time constant (the backend smooths too)
    const PEAK_HOLD_S = 0.35;
    const PEAK_GRAVITY = 6;     // peak caps fall with gravity, in heights/s²
    const displayState = new Map();
    let lastBandFrame = 0;
    let bandNow = 0;            // performance.now() of the current frame, in s
    function bands(N) {
        let st = displayState.get(N);
        if (!st) { st = { v: new Float32Array(N), peak: new Float32Array(N), hold: new Float32Array(N), pvel: new Float32Array(N), frame: -1, t: 0 }; displayState.set(N, st); }
        if (st.frame === lastBandFrame) return st;   // already advanced this frame
        st.frame = lastBandFrame;
        // Clamped so a paused loop or a stalled frame resumes without a jump.
        const dt = st.t ? Math.min(0.1, Math.max(0, bandNow - st.t)) : 0.04;
        st.t = bandNow;
        const rise = Math.exp(-dt / BAR_RISE_S), fall = Math.exp(-dt / BAR_FALL_S);
        const L = freqData.length;
        for (let i = 0; i < N; i++) {
            const a = Math.floor(i * L / N), b = Math.max(a + 1, Math.floor((i + 1) * L / N));
            let sum = 0, max = 0;
            for (let k = a; k < b; k++) { sum += freqData[k]; if (freqData[k] > max) max = freqData[k]; }
            // Mostly the mean, a little of the peak: smooth but still punchy. The
            // curve keeps headroom: mastered music otherwise pins every bar high.
            const target = Math.pow((0.7 * sum / (b - a) + 0.3 * max) / 255, 1.5);
            const v = st.v[i];
            st.v[i] = target + (v - target) * (target > v ? rise : fall);
            // Peak cap: holds briefly, then falls with gravity onto the bar.
            if (st.v[i] >= st.peak[i]) { st.peak[i] = st.v[i]; st.hold[i] = PEAK_HOLD_S; st.pvel[i] = 0; }
            else if (st.hold[i] > 0) st.hold[i] -= dt;
            else { st.pvel[i] += PEAK_GRAVITY * dt; st.peak[i] = Math.max(st.v[i], st.peak[i] - st.pvel[i] * dt); }
        }
        return st;
    }

    // Palette resolved from the theme tokens (no colour is defined here):
    // accent → accent/danger blends → danger ≈ AndroidAuto2's cyan → blue →
    // purple → magenta; fire uses danger → warning → text.
    const palette = { stops: [], fire: [], bgRgb: [0, 0, 0] };
    const probe = document.createElement('canvas').getContext('2d', { willReadFrequently: true });
    function toRgb(color) {
        probe.clearRect(0, 0, 1, 1);
        probe.fillStyle = '#000';
        probe.fillStyle = color;
        probe.fillRect(0, 0, 1, 1);
        const d = probe.getImageData(0, 0, 1, 1).data;
        return [d[0], d[1], d[2]];
    }
    function readPalette() {
        const css = getComputedStyle(document.documentElement);
        const tok = n => css.getPropertyValue(n).trim();
        const accent = tok('--accent'), danger = tok('--danger'), warning = tok('--warning'), text = tok('--text-primary');
        // OKLCH: the blend travels round the hue wheel (cyan → blue → purple →
        // magenta) instead of through grey, as an sRGB mix did.
        palette.stops = [accent, `color-mix(in oklch, ${accent} 67%, ${danger})`, `color-mix(in oklch, ${accent} 33%, ${danger})`, danger].map(toRgb);
        palette.fire = [danger, warning, text].map(toRgb);
        palette.bgRgb = toRgb(tok('--bg-primary'));
    }
    document.addEventListener('rascar-theme', readPalette);
    const lerp = (a, b, t) => a + (b - a) * t;
    function mix(stops, t) {
        t = Math.min(1, Math.max(0, t)) * (stops.length - 1);
        const i = Math.min(stops.length - 2, Math.floor(t)), f = t - i;
        const a = stops[i], b = stops[i + 1];
        return [lerp(a[0], b[0], f), lerp(a[1], b[1], f), lerp(a[2], b[2], f)];
    }
    const rgba = (c, a) => `rgba(${c[0] | 0},${c[1] | 0},${c[2] | 0},${a})`;
    const col = (t, a = 1) => rgba(mix(palette.stops, t), a);

    function clear(ctx, W, H, bg) {
        ctx.shadowBlur = 0;
        ctx.globalAlpha = 1;
        ctx.fillStyle = bg;
        ctx.fillRect(0, 0, W, H);
    }
    // Trails fade towards the theme background (was always black).
    function fade(ctx, W, H, amount) {
        ctx.shadowBlur = 0;
        ctx.fillStyle = rgba(palette.bgRgb, amount);
        ctx.fillRect(0, 0, W, H);
    }
    function glow(ctx, color, blur) {
        ctx.shadowColor = color;
        ctx.shadowBlur = blur;
    }
    function bar(ctx, x, y, w, h, r) {
        ctx.beginPath();
        ctx.roundRect(x, y, w, h, Math.min(r, w / 2, h / 2));
        ctx.fill();
    }

    // ── 16 Visualizer Draw Functions ─────────────────────────────

    // 0: Bars — rounded, per-band colour, glow when loud
    function drawBars(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const N = 32, st = bands(N);
        const slot = W / N, bw = slot * 0.7;
        for (let i = 0; i < N; i++) {
            const v = st.v[i], h = Math.max(3, v * H * 0.92), x = i * slot + (slot - bw) / 2, t = i / (N - 1);
            const g = ctx.createLinearGradient(0, H - h, 0, H);
            g.addColorStop(0, col(t, 0.95));
            g.addColorStop(1, col(t, 0.35));
            ctx.fillStyle = g;
            glow(ctx, col(t, 0.6), v > 0.45 ? 18 * v : 0);
            bar(ctx, x, H - h, bw, h, bw / 2);
        }
    }

    // 1: Wave — smooth oscilloscope line, gradient across, soft fill
    function drawWave(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const n = timeData.length, pts = [];
        for (let i = 0; i < n; i++) pts.push([(i / (n - 1)) * W, H / 2 + ((timeData[i] - 128) / 128) * H * 0.42]);
        const trace = () => {
            ctx.beginPath();
            ctx.moveTo(pts[0][0], pts[0][1]);
            for (let i = 1; i < n - 1; i++) {
                const mx = (pts[i][0] + pts[i + 1][0]) / 2, my = (pts[i][1] + pts[i + 1][1]) / 2;
                ctx.quadraticCurveTo(pts[i][0], pts[i][1], mx, my);
            }
            ctx.lineTo(pts[n - 1][0], pts[n - 1][1]);
        };
        const g = ctx.createLinearGradient(0, 0, W, 0);
        [0, 0.33, 0.66, 1].forEach(s => g.addColorStop(s, col(s)));
        trace();
        ctx.lineTo(W, H); ctx.lineTo(0, H); ctx.closePath();
        const fill = ctx.createLinearGradient(0, H / 2, 0, H);
        fill.addColorStop(0, col(0.3, 0.18));
        fill.addColorStop(1, col(0.3, 0));
        ctx.fillStyle = fill;
        ctx.fill();
        trace();
        ctx.strokeStyle = g;
        ctx.lineWidth = 3;
        ctx.lineJoin = 'round';
        glow(ctx, col(0.2, 0.7), 14);
        ctx.stroke();
    }

    // 2: Circles — concentric rings breathing with their band
    function drawCircles(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const cx = W / 2, cy = H / 2, R = Math.min(W, H) * 0.46, st = bands(10);
        for (let i = 9; i >= 0; i--) {
            const v = st.v[i], t = i / 9, r = R * (0.12 + 0.09 * i) * (0.85 + 0.35 * v);
            ctx.beginPath();
            ctx.arc(cx, cy, r, 0, Math.PI * 2);
            ctx.strokeStyle = col(t, 0.35 + v * 0.6);
            ctx.lineWidth = 2 + v * 7;
            glow(ctx, col(t, 0.6), 10 * v);
            ctx.stroke();
        }
    }

    // 3: Mirrored — bars growing up and down from the centre line
    function drawMirrored(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const N = 32, st = bands(N), slot = W / N, bw = slot * 0.7, cy = H / 2;
        for (let i = 0; i < N; i++) {
            const v = st.v[i], h = Math.max(2, v * H * 0.46), x = i * slot + (slot - bw) / 2, t = i / (N - 1);
            const g = ctx.createLinearGradient(0, cy - h, 0, cy + h);
            g.addColorStop(0, col(t, 0.95));
            g.addColorStop(0.5, col(t, 0.55));
            g.addColorStop(1, col(t, 0.2));
            ctx.fillStyle = g;
            glow(ctx, col(t, 0.5), v > 0.45 ? 14 * v : 0);
            bar(ctx, x, cy - h, bw, h * 2, bw / 2);
        }
    }

    // 4: Radial — bars around a core that pulses with the bass
    function drawRadial(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const cx = W / 2, cy = H / 2, N = 64, st = bands(N / 2);
        const bass = (st.v[0] + st.v[1] + st.v[2]) / 3;
        const rMin = Math.min(W, H) * (0.16 + bass * 0.04), rMax = Math.min(W, H) * 0.47;
        const core = ctx.createRadialGradient(cx, cy, 0, cx, cy, rMin);
        core.addColorStop(0, col(0.1, 0.25 + bass * 0.35));
        core.addColorStop(1, col(0.1, 0));
        ctx.fillStyle = core;
        ctx.beginPath(); ctx.arc(cx, cy, rMin, 0, Math.PI * 2); ctx.fill();
        ctx.lineCap = 'round';
        ctx.lineWidth = Math.max(3, (2 * Math.PI * rMin / N) * 0.55);
        for (let i = 0; i < N; i++) {
            // Symmetric: low notes at the top, mirrored left/right.
            const k = i < N / 2 ? i : N - 1 - i, v = st.v[k], t = k / (N / 2 - 1);
            const a = (i / N) * Math.PI * 2 - Math.PI / 2, r = rMin + 6 + v * (rMax - rMin);
            ctx.beginPath();
            ctx.moveTo(cx + Math.cos(a) * (rMin + 4), cy + Math.sin(a) * (rMin + 4));
            ctx.lineTo(cx + Math.cos(a) * r, cy + Math.sin(a) * r);
            ctx.strokeStyle = col(t, 0.55 + v * 0.45);
            glow(ctx, col(t, 0.6), v > 0.5 ? 12 : 0);
            ctx.stroke();
        }
    }

    // 5: Bars + peaks — bars with falling peak caps
    function drawBarsWithPeaks(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const N = 32, st = bands(N), slot = W / N, bw = slot * 0.7, capH = Math.max(3, H * 0.012);
        for (let i = 0; i < N; i++) {
            const v = st.v[i], h = Math.max(3, v * H * 0.86), x = i * slot + (slot - bw) / 2, t = i / (N - 1);
            const g = ctx.createLinearGradient(0, H - h, 0, H);
            g.addColorStop(0, col(t, 0.85));
            g.addColorStop(1, col(t, 0.25));
            ctx.fillStyle = g;
            glow(ctx, col(t, 0.5), 0);
            bar(ctx, x, H - h, bw, h, 4);
            const py = H - st.peak[i] * H * 0.86 - capH * 2.5;
            ctx.fillStyle = col(t, 1);
            glow(ctx, col(t, 0.9), 10);
            bar(ctx, x, py, bw, capH, capH / 2);
        }
    }

    // One LED segment, glow included, drawn once and stamped with drawImage.
    // A shadow blur per segment (up to 640 a frame) held the fullscreen meter
    // at ~8 fps on the Pi with music playing (2026-10-01); every other mode ran at 25.
    const ledSprites = new Map();
    function ledSprite(rgb, alpha, blur, w, h) {
        const key = `${rgb.map(v => v | 0).join()}|${alpha}|${blur}|${w.toFixed(2)}x${h.toFixed(2)}`;
        let s = ledSprites.get(key);
        if (!s) {
            if (ledSprites.size > 256) ledSprites.clear(); // resized or re-themed: old sizes are dead
            const pad = blur * 2;
            const c = document.createElement('canvas');
            c.width = Math.ceil(w + pad * 2);
            c.height = Math.ceil(h + pad * 2);
            const g = c.getContext('2d');
            g.fillStyle = rgba(rgb, alpha);
            glow(g, rgba(rgb, 0.55), blur);
            bar(g, pad, pad, w, h, h * 0.3);
            s = { c, pad };
            ledSprites.set(key, s);
        }
        return s;
    }

    // 6: LED bars (default) — 32 columns × 20 segments, red top, peak LED
    function drawLedBars(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const N = 32, SEGS = 20, st = bands(N);
        const slot = W / N, bw = slot * 0.72, gap = Math.max(2, H * 0.008);
        const segH = (H - gap * (SEGS + 1)) / SEGS, redFrom = Math.floor(SEGS * 0.85);
        const red = palette.stops[palette.stops.length - 1];
        for (let i = 0; i < N; i++) {
            const t = i / (N - 1), x = i * slot + (slot - bw) / 2;
            const lit = Math.round(st.v[i] * SEGS), peakSeg = Math.min(SEGS - 1, Math.round(st.peak[i] * SEGS));
            const c = mix(palette.stops, t);
            // [unlit, lit, peak] for the column colour and for the red top.
            // Unlit segments stay faintly visible, like a real VU meter.
            const looks = [c, red].map(rgb => [ledSprite(rgb, 0.08, 0, bw, segH), ledSprite(rgb, 0.9, 8, bw, segH), ledSprite(rgb, 0.95, 8, bw, segH)]);
            for (let s = 0; s < SEGS; s++) {
                const y = H - (s + 1) * (segH + gap);
                const on = s < lit || (s === peakSeg && peakSeg > 0);
                const led = looks[s >= redFrom ? 1 : 0][!on ? 0 : s === peakSeg && s >= lit ? 2 : 1];
                ctx.drawImage(led.c, x - led.pad, y - led.pad);
            }
        }
    }

    // 7: Fire — flames in the theme's danger/warning colours, with trails
    function drawFire(ctx, W, H, bg) {
        fade(ctx, W, H, 0.28);
        const N = 40, st = bands(N), slot = W / N, now = performance.now() / 1000;
        for (let i = 0; i < N; i++) {
            const flick = 0.04 * Math.sin(now * 14 + i * 2.3);
            const v = Math.max(0, Math.min(1, st.v[i] + flick)), h = v * H * 0.95, x = i * slot;
            const g = ctx.createLinearGradient(0, H, 0, H - h);
            g.addColorStop(0, rgba(mix(palette.fire, 0), 0.95));
            g.addColorStop(0.55, rgba(mix(palette.fire, 0.5), 0.75));
            g.addColorStop(1, rgba(mix(palette.fire, 1), 0.1));
            ctx.fillStyle = g;
            glow(ctx, rgba(mix(palette.fire, 0.4), 0.7), 16 * v);
            bar(ctx, x + slot * 0.08, H - h, slot * 0.84, h, slot * 0.4);
        }
    }

    // 8: Half circle — a rising fan from the bottom
    function drawHalfCircle(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const cx = W / 2, cy = H * 0.96, N = 48, st = bands(N);
        const rMin = Math.min(W / 2, H) * 0.2, rMax = Math.min(W / 2, H) * 0.92;
        ctx.lineCap = 'round';
        ctx.lineWidth = Math.max(3, (Math.PI * rMin / N) * 0.6);
        for (let i = 0; i < N; i++) {
            const v = st.v[i], t = i / (N - 1), a = Math.PI + (i + 0.5) / N * Math.PI, r = rMin + 6 + v * (rMax - rMin);
            ctx.beginPath();
            ctx.moveTo(cx + Math.cos(a) * rMin, cy + Math.sin(a) * rMin);
            ctx.lineTo(cx + Math.cos(a) * r, cy + Math.sin(a) * r);
            ctx.strokeStyle = col(t, 0.55 + v * 0.45);
            glow(ctx, col(t, 0.6), v > 0.5 ? 12 : 0);
            ctx.stroke();
        }
    }

    // 9: Waterfall — spectrogram scrolling up; the canvas scrolls itself, so
    // only one new row is drawn per frame (the old version redrew all history).
    let waterfallCanvases = new WeakSet();
    function waterfallReset() { waterfallCanvases = new WeakSet(); }
    function drawWaterfall(ctx, W, H, bg) {
        const ROW = 3, N = 96, st = bands(N);
        ctx.shadowBlur = 0;
        ctx.globalAlpha = 1;
        // Scrolling reuses the canvas: start from a clean one, not the last mode.
        if (!waterfallCanvases.has(ctx.canvas)) { clear(ctx, W, H, bg); waterfallCanvases.add(ctx.canvas); }
        ctx.drawImage(ctx.canvas, 0, -ROW);
        ctx.fillStyle = bg;
        ctx.fillRect(0, H - ROW, W, ROW);
        const cw = W / N;
        for (let i = 0; i < N; i++) {
            const v = st.v[i];
            if (v < 0.08) continue;
            // Heat: quiet fades into the background, loud is bright and moves
            // along the palette.
            ctx.fillStyle = col(v, Math.min(1, Math.pow(v, 1.3) * 1.4));
            ctx.fillRect(i * cw, H - ROW, Math.ceil(cw), ROW);
        }
    }

    // 10: Quad mirror — four mirrored quadrants from the centre
    function drawQuadMirror(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const N = 20, st = bands(N), hw = W / 2, hh = H / 2, slot = hw / N, bw = slot * 0.7;
        for (let i = 0; i < N; i++) {
            const v = st.v[i], h = Math.max(2, v * hh * 0.9), t = i / (N - 1), off = i * slot + (slot - bw) / 2;
            ctx.fillStyle = col(t, 0.5 + v * 0.45);
            glow(ctx, col(t, 0.5), v > 0.5 ? 10 : 0);
            bar(ctx, hw + off, hh - h, bw, h * 2, bw / 2);
            bar(ctx, hw - off - bw, hh - h, bw, h * 2, bw / 2);
        }
    }

    // 11: Radial wedges — pie slices with a radial gradient
    function drawRadialWedges(ctx, W, H, bg) {
        clear(ctx, W, H, bg);
        const cx = W / 2, cy = H / 2, N = 32, st = bands(N / 2), rMax = Math.min(W, H) * 0.47;
        for (let i = 0; i < N; i++) {
            const k = i < N / 2 ? i : N - 1 - i, v = st.v[k], t = k / (N / 2 - 1);
            const a1 = (i / N) * Math.PI * 2 - Math.PI / 2 + 0.02, a2 = ((i + 1) / N) * Math.PI * 2 - Math.PI / 2 - 0.02;
            const r = rMax * (0.18 + 0.82 * v);
            const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, r);
            g.addColorStop(0, col(t, 0.1));
            g.addColorStop(1, col(t, 0.55 + v * 0.4));
            ctx.fillStyle = g;
            ctx.beginPath();
            ctx.moveTo(cx, cy);
            ctx.arc(cx, cy, r, a1, a2);
            ctx.closePath();
            ctx.fill();
        }
    }

    // 12: Tunnel — rings flying outwards, speed and size from the music
    let tunnelPhase = 0;
    function drawTunnel(ctx, W, H, bg) {
        fade(ctx, W, H, 0.3);
        const cx = W / 2, cy = H / 2, R = Math.hypot(W, H) / 2, st = bands(12);
        const energy = st.v.reduce((a, b) => a + b, 0) / 12;
        tunnelPhase = (tunnelPhase + 0.004 + energy * 0.02) % 1;
        for (let i = 0; i < 12; i++) {
            const p = (i / 12 + tunnelPhase) % 1, r = R * p * p, v = st.v[i];
            ctx.beginPath();
            ctx.arc(cx, cy, Math.max(2, r), 0, Math.PI * 2);
            ctx.strokeStyle = col(p, (0.15 + v * 0.7) * (1 - p * 0.5));
            ctx.lineWidth = 1 + p * 6 + v * 6;
            glow(ctx, col(p, 0.6), 10 * v);
            ctx.stroke();
        }
    }

    // 13: Orbs — glowing orbs orbiting, size from their band
    function drawOrbs(ctx, W, H, bg) {
        fade(ctx, W, H, 0.22);
        const N = 14, st = bands(N), now = performance.now() / 1000;
        ctx.shadowBlur = 0;
        for (let i = 0; i < N; i++) {
            const v = st.v[i], t = i / (N - 1);
            const x = W / 2 + Math.cos(now * (0.3 + t * 0.4) + i * 1.9) * W * (0.12 + 0.3 * v);
            const y = H / 2 + Math.sin(now * (0.4 + t * 0.3) + i * 1.3) * H * (0.1 + 0.3 * v);
            const r = 8 + v * Math.min(W, H) * 0.12;
            const g = ctx.createRadialGradient(x, y, 0, x, y, r);
            g.addColorStop(0, col(t, 0.85));
            g.addColorStop(0.4, col(t, 0.35 * (0.5 + v)));
            g.addColorStop(1, col(t, 0));
            ctx.fillStyle = g;
            ctx.beginPath();
            ctx.arc(x, y, r, 0, Math.PI * 2);
            ctx.fill();
        }
    }

    // 14: Spiral — the spectrum wound on a slowly turning spiral
    function drawSpiral(ctx, W, H, bg) {
        fade(ctx, W, H, 0.25);
        const cx = W / 2, cy = H / 2, st = bands(96), now = performance.now() / 4000, R = Math.min(W, H) * 0.46;
        ctx.lineWidth = 3;
        ctx.lineCap = 'round';
        for (let i = 1; i < 96; i++) {
            const seg = k => {
                const f = k / 96, a = now + f * Math.PI * 6, r = f * R + st.v[k] * R * 0.18;
                return [cx + Math.cos(a) * r, cy + Math.sin(a) * r];
            };
            const [x0, y0] = seg(i - 1), [x1, y1] = seg(i), t = i / 95;
            ctx.beginPath();
            ctx.moveTo(x0, y0);
            ctx.lineTo(x1, y1);
            ctx.strokeStyle = col(t, 0.5 + st.v[i] * 0.5);
            glow(ctx, col(t, 0.6), st.v[i] > 0.5 ? 10 : 0);
            ctx.stroke();
        }
    }

    // 15: Scope — the waveform wrapped around a ring that swells with the
    // music. (Was a Lissajous figure: with one mono channel it could only
    // ever be a tangle.)
    function drawLissajous(ctx, W, H, bg) {
        fade(ctx, W, H, 0.3);
        const n = timeData.length, cx = W / 2, cy = H / 2, st = bands(8);
        const energy = st.v.reduce((a, b) => a + b, 0) / 8;
        const R = Math.min(W, H) * (0.24 + energy * 0.1), amp = Math.min(W, H) * 0.16;
        const sm = k => ((timeData[(k + n - 1) % n] + 2 * timeData[k % n] + timeData[(k + 1) % n]) / 4 - 128) / 128;
        const pt = i => {
            const a = (i / n) * Math.PI * 2 - Math.PI / 2, r = R + sm(i) * amp;
            return [cx + Math.cos(a) * r, cy + Math.sin(a) * r];
        };
        const g = ctx.createLinearGradient(cx - R, cy - R, cx + R, cy + R);
        [0, 0.33, 0.66, 1].forEach(s => g.addColorStop(s, col(s)));
        ctx.beginPath();
        let [px, py] = pt(0);
        ctx.moveTo(px, py);
        for (let i = 1; i <= n; i++) {
            const [x, y] = pt(i % n);
            ctx.quadraticCurveTo(px, py, (px + x) / 2, (py + y) / 2);
            px = x; py = y;
        }
        ctx.closePath();
        ctx.strokeStyle = g;
        ctx.lineWidth = 3;
        ctx.lineJoin = 'round';
        glow(ctx, col(0.3, 0.8), 16);
        ctx.stroke();
    }

    // ── Fullscreen ───────────────────────────────────────────────
    function enterFullscreen() {
        const el = document.getElementById('media-fullscreen');
        if (!el) return;
        el.classList.add('active');
        syncFullscreenUI();
        refreshVizAvailability();
        resetFsAutoHide();
        startVizLoop(); // fullscreen just became visible — (re)start drawing if audio is live
        // Click anywhere to show/hide controls
        el.onclick = (e) => {
            if (e.target.closest('.fs-ctrl-btn, .fs-transport-btn, .fs-ctrl-grid, .fs-bottom button, .viz-mode-label')) return;
            toggleFsControls();
        };
    }

    // ── Visualizer-only fullscreen (tap the home vumeter) ────────
    // Same overlay and canvas as the media fullscreen, with every control hidden:
    // tap anywhere = next mode (its name flashes), x = close.
    let vizNameTimer = null;

    function flashVizName() {
        const lbl = document.getElementById('viz-only-label');
        if (!lbl) return;
        lbl.textContent = hasAudio()
            ? `${VIZ_MODES[vizMode]}  ·  ${vizMode + 1}/${VIZ_MODES.length}`
            : 'No audio playing';
        lbl.classList.add('visible');
        if (vizNameTimer) clearTimeout(vizNameTimer);
        // Without audio the hint stays up: a black screen must explain itself.
        if (hasAudio()) vizNameTimer = setTimeout(() => lbl.classList.remove('visible'), 1800);
    }

    function enterVizFullscreen() {
        const el = document.getElementById('media-fullscreen');
        if (!el) return;
        el.classList.add('active', 'viz-only');
        startVizLoop();
        flashVizName();
        el.onclick = (e) => {
            if (e.target.closest('.viz-close')) return;
            nextVizMode();
            flashVizName();
        };
    }

    function exitFullscreen() {
        const el = document.getElementById('media-fullscreen');
        if (el) el.classList.remove('active', 'viz-only');
        if (vizNameTimer) { clearTimeout(vizNameTimer); vizNameTimer = null; }
        document.getElementById('viz-only-label')?.classList.remove('visible');
        if (fsAutoHideTimer) clearTimeout(fsAutoHideTimer);
        karaokeActive = false;
        // Stop the lyric auto-scroll timer so it doesn't run forever after exit.
        if (lyricTimer) { clearInterval(lyricTimer); lyricTimer = null; }
        document.getElementById('karaoke-overlay')?.classList.remove('active');
        document.getElementById('playlist-overlay')?.classList.remove('active');
        document.getElementById('search-overlay')?.classList.remove('active');
        // Re-evaluate whether the loop should keep running (e.g. home page hidden).
        if (!vizShouldRun()) stopVizLoop();
    }

    function toggleFsControls() {
        const el = document.getElementById('media-fullscreen');
        if (!el) return;
        el.classList.toggle('fs-controls-hidden');
        fsControlsVisible = !el.classList.contains('fs-controls-hidden');
        if (fsControlsVisible) resetFsAutoHide();
    }

    function resetFsAutoHide() {
        if (fsAutoHideTimer) clearTimeout(fsAutoHideTimer);
        const el = document.getElementById('media-fullscreen');
        if (el) el.classList.remove('fs-controls-hidden');
        fsControlsVisible = true;
        fsAutoHideTimer = setTimeout(() => {
            if (document.getElementById('media-fullscreen')?.classList.contains('active')) {
                document.getElementById('media-fullscreen')?.classList.add('fs-controls-hidden');
                fsControlsVisible = false;
            }
        }, 5000);
    }

    function syncFullscreenUI() {
        const s = window.RasCar?.state || {};
        const m = s.media || {};
        const artEl = document.getElementById('fs-album-art');
        if (m.title) {
            setText('fs-title', m.title);
            setText('fs-artist', m.artist || '');
            if (artEl && m.art_url) {
                // Reuse the shared DOM-based art renderer — no URL interpolated into HTML.
                window.setArtImage(artEl, m.art_url);
            }
        } else {
            // The radio (the page's own <audio>) is not a player the backend
            // reports: it only fills the home card. Mirror that card rather than
            // claiming "No media playing" over a station that is playing.
            setText('fs-title', document.getElementById('media-title')?.textContent || 'No media playing');
            setText('fs-artist', document.getElementById('media-artist')?.textContent || '');
            const homeArt = document.getElementById('media-art');
            if (artEl && homeArt) artEl.replaceChildren(...[...homeArt.childNodes].map(n => n.cloneNode(true)));
        }
        // Intensify the glow when there is real artwork behind it. The gradient
        // itself lives in media.css — no styling is assigned from script.
        const glow = document.querySelector('.fs-album-glow');
        if (glow) glow.classList.toggle('has-art', !!artEl?.querySelector('img'));
        // Transport state — the button showed a hardcoded "pause" for every track.
        const playIcon = document.querySelector('#fs-play-btn i');
        if (playIcon) playIcon.className = m.is_playing ? 'fas fa-pause' : 'fas fa-play';
        // Shuffle reflects the player's real state, not a local flag.
        const shuffleBtn = document.getElementById('fs-shuffle-btn');
        if (shuffleBtn) shuffleBtn.classList.toggle('active', !!m.shuffle);
        const repeat = s.music?.ours ? s.music.repeat : 'off';
        const repeatBtn = document.getElementById('fs-repeat-btn');
        if (repeatBtn) {
            repeatBtn.classList.toggle('active', repeat === 'all' || repeat === 'one');
            repeatBtn.classList.toggle('repeat-one', repeat === 'one');
        }
        syncFullscreenProgress(m);
    }

    // Position/length come from the media payload; when either is missing the whole
    // progress row hides rather than showing an invented bar between 0:00 and 0:00.
    function syncFullscreenProgress(m) {
        const row = document.getElementById('fs-progress-row');
        const fill = document.getElementById('fs-progress-fill');
        const known = window.hasPlaybackPosition ? window.hasPlaybackPosition(m) : false;
        if (row) row.classList.toggle('hidden', !known);
        if (!known) {
            if (fill) fill.style.width = '0%';
            return;
        }
        if (fill) fill.style.width = `${window.playbackPct(m)}%`;
        setText('fs-time-current', window.fmtDuration(m.position_sec));
        setText('fs-time-total', window.fmtDuration(m.length_sec));
    }

    // ── Karaoke ──────────────────────────────────────────────────
    function toggleKaraoke() {
        karaokeActive = !karaokeActive;
        const overlay = document.getElementById('karaoke-overlay');
        const btn = document.getElementById('fs-karaoke-btn');
        if (overlay) overlay.classList.toggle('active', karaokeActive);
        if (btn) btn.classList.toggle('active', karaokeActive);

        if (karaokeActive) {
            fetchLyrics();
        } else {
            if (lyricTimer) { clearInterval(lyricTimer); lyricTimer = null; }
        }
    }

    // Playback clock for the karaoke highlight: the WS reports the position once
    // a second; between reports it is extrapolated while playing, so the
    // highlight follows the song instead of jumping once a second.
    let clockPos = null, clockAt = 0, clockPlaying = false;
    function noteClock(m) {
        if (typeof m?.position_sec === 'number' && Number.isFinite(m.position_sec)) {
            clockPos = m.position_sec;
            clockAt = performance.now();
        }
        clockPlaying = !!m?.is_playing;
    }
    const clockNow = () => clockPos === null ? null
        : clockPos + (clockPlaying ? (performance.now() - clockAt) / 1000 : 0);

    async function fetchLyrics() {
        const s = window.RasCar?.state || {};
        const m = s.media || {};
        if (lyricTimer) { clearInterval(lyricTimer); lyricTimer = null; }
        lyricLines = [];
        lyricsSynced = false;
        lyricIndex = -1;
        if (!m.title) {
            renderLyrics(['No song playing', '', 'Play a song to see lyrics']);
            return;
        }
        renderLyrics([m.title, m.artist || '', '', 'Looking for lyrics…']);
        try {
            const res = await fetch(`/api/media/lyrics?title=${encodeURIComponent(m.title)}&artist=${encodeURIComponent(m.artist || '')}`);
            const data = await res.json();
            const lines = normalizeLyrics(data && data.lines);
            if (lines.length > 0) {
                lyricLines = lines;
                // Only claim synchronisation when the source actually carried times.
                lyricsSynced = !!data.synced && lines.some(l => typeof l.time_sec === 'number');
                buildLyricsList();
                if (lyricsSynced) {
                    // ~7 updates a second: smooth, and trivial for the page.
                    lyricTimer = setInterval(followClock, 150);
                    followClock();
                }
            } else {
                renderLyrics([m.title, m.artist || '', '', 'Lyrics not available']);
            }
        } catch (e) {
            renderLyrics([m.title, m.artist || '', '', 'Lyrics not available']);
        }
    }

    // Accept both the timestamped contract ({time_sec, text}) and a plain string list.
    function normalizeLyrics(lines) {
        if (!Array.isArray(lines)) return [];
        return lines.map(l => (typeof l === 'string')
            ? { time_sec: null, text: l }
            : { time_sec: (typeof l?.time_sec === 'number' ? l.time_sec : null), text: l?.text ?? '' });
    }

    // Index of the last line whose timestamp has been reached (binary search).
    function indexForPosition(positionSec) {
        if (typeof positionSec !== 'number' || !Number.isFinite(positionSec)) return lyricIndex;
        let lo = 0, hi = lyricLines.length - 1, idx = -1;
        while (lo <= hi) {
            const mid = (lo + hi) >> 1;
            const t = lyricLines[mid].time_sec;
            if (typeof t === 'number' && t <= positionSec) { idx = mid; lo = mid + 1; } else { hi = mid - 1; }
        }
        return idx;
    }

    // Short status messages (no song, loading, not found).
    function renderLyrics(lines) {
        const el = document.getElementById('karaoke-lyrics');
        if (!el) return;
        el.textContent = '';
        lines.forEach((l, i) => {
            const d = document.createElement('div');
            d.className = `karaoke-line ${i === 0 ? 'current' : 'future'}`;
            d.textContent = l || ' ';
            el.appendChild(d);
        });
    }

    // The whole song once; the highlight then only moves classes and scrolls.
    function buildLyricsList() {
        const el = document.getElementById('karaoke-lyrics');
        if (!el) return;
        el.textContent = '';
        lyricLines.forEach(l => {
            const d = document.createElement('div');
            // Unsynced lyrics: readable text, never highlighted as if in time.
            d.className = `karaoke-line ${lyricsSynced ? 'future' : 'plain'}`;
            d.textContent = l.text || ' ';
            el.appendChild(d);
        });
        el.scrollTop = 0;
    }

    function highlightLine(idx) {
        const el = document.getElementById('karaoke-lyrics');
        if (!el || idx === lyricIndex) return;
        lyricIndex = idx;
        [...el.children].forEach((d, i) => {
            d.classList.toggle('past', i < idx);
            d.classList.toggle('current', i === idx);
            d.classList.toggle('future', i > idx);
        });
        const cur = el.children[Math.max(0, idx)];
        if (cur) el.scrollTo({ top: cur.offsetTop - el.clientHeight / 2 + cur.offsetHeight / 2, behavior: 'smooth' });
    }

    function followClock() {
        if (!karaokeActive || !lyricsSynced || !lyricLines.length) return;
        highlightLine(indexForPosition(clockNow()));
    }

    // ── YouTube Music helpers ────────────────────────────────────
    // A full-width message inside a .playlist-grid (XSS-safe via textContent).
    function gridMessage(grid, msg) {
        grid.textContent = '';
        const d = document.createElement('div');
        d.className = 'playlist-card-sub grid-message';
        d.textContent = msg;
        grid.appendChild(d);
    }

    // Shown when the backend reports YouTube Music is not connected.
    function connectPrompt(grid) {
        gridMessage(grid, 'Sign in on the YouTube Music screen to see this');
    }

    // The track list the playlist overlay currently shows (liked songs or a
    // playlist), for Play all / Shuffle all and for tap-to-play-from-here.
    let currentList = null;

    async function postMusic(path, body, okMsg) {
        try {
            const res = await fetch(path, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body || {})
            });
            const data = await res.json();
            if (!res.ok || !data.ok) { toast(data.error || 'Player unavailable'); return null; }
            if (okMsg) toast(okMsg);
            return data;
        } catch (e) {
            toast('Player unavailable');
            return null;
        }
    }

    // One song, then its radio (services/player.rs), like the YouTube Music app.
    async function playTrack(t) {
        if (!t || !t.videoId) { toast('Track cannot be played'); return; }
        if (await postMusic('/api/music/play', t, `Playing ${t.title || 'track'}`)) {
            closePlaylists();
            closeSearch();
        }
    }

    // A whole list, from `start`: the rest of the list follows, then radio.
    async function playList(tracks, start, shuffle) {
        const playable = (tracks || []).filter(t => t && t.videoId);
        if (!playable.length) { toast('Nothing to play'); return; }
        const msg = shuffle ? 'Shuffling' : `Playing ${playable[start]?.title || 'list'}`;
        if (await postMusic('/api/music/queue', { tracks: playable, start: start || 0, shuffle: !!shuffle }, msg)) {
            closePlaylists();
            closeSearch();
        }
    }

    function playAll(shuffle) {
        if (currentList && currentList.length) playList(currentList, 0, shuffle);
    }

    // Attach the play handler to a track card. `t` is captured by closure — the
    // videoId is never interpolated into markup or an onclick string. With a
    // list, the tap plays that list from this track; without, song + radio.
    function bindPlay(card, t, list, index) {
        if (!t || !t.videoId) return;
        card.classList.add('playable');
        card.addEventListener('click', () => list ? playList(list, index, false) : playTrack(t));
    }

    // Play all / Shuffle all / Radio belong to track lists only.
    function showListActions(on) {
        ['playlist-play-all', 'playlist-shuffle-all', 'playlist-radio']
            .forEach(id => document.getElementById(id)?.classList.toggle('hidden', !on));
    }

    // The radio of the list's first song ("Play <artist> radio" on an artist page).
    function listRadio() {
        const seed = currentList && currentList[0];
        if (seed) postMusic('/api/music/radio', { videoId: seed.videoId }, `Radio from ${seed.title || 'track'}`).then(ok => { if (ok) { closePlaylists(); closeSearch(); } });
    }

    // A card with its artwork (DOM only: no URL or text interpolated into markup).
    function makeCard(artUrl, titleText, subText) {
        const card = document.createElement('div');
        card.className = 'playlist-card playlist-card-row';
        const art = document.createElement('img');
        art.className = 'ytm-art';
        art.alt = '';
        art.loading = 'lazy';
        // YouTube's image hosts (yt3.ggpht.com) answer 429 to a cross-site
        // Referer: 20 of 107 artworks were broken. Send none.
        art.referrerPolicy = 'no-referrer';
        if (artUrl) art.src = artUrl;
        const text = document.createElement('span');
        text.className = 'ytm-track-text';
        const title = document.createElement('strong');
        title.textContent = titleText;
        const sub = document.createElement('small');
        sub.className = 'muted';
        sub.textContent = subText || '';
        text.append(title, sub);
        card.append(art, text);
        return card;
    }

    // "Load more" (AndroidAuto2 paged liked songs and playlists): the current
    // list is re-fetched with a larger limit.
    let listSource = null;   // { kind: 'liked'|'playlist', id, title, limit }
    const PAGE = 100;
    function appendLoadMore(grid) {
        if (!listSource || !currentList || currentList.length < listSource.limit) return;
        const card = makeCard('', 'Load more', `${currentList.length} shown`);
        card.classList.add('playlist-folder');
        card.addEventListener('click', () => {
            listSource.limit += PAGE;
            const keep = grid.scrollTop;
            (listSource.kind === 'liked' ? showLikedSongs(listSource.limit) : openPlaylist(listSource.id, listSource.title, listSource.limit))
                .then(() => { grid.scrollTop = keep; });
        });
        grid.appendChild(card);
    }

    // Render a list of normalized tracks {videoId,title,artist,album,art}.
    function renderTracks(grid, tracks) {
        grid.textContent = '';
        currentList = (tracks || []).filter(t => t && t.videoId);
        showListActions(currentList.length > 0);
        if (!tracks || !tracks.length) {
            gridMessage(grid, 'Nothing here yet');
            return;
        }
        tracks.forEach(t => {
            const card = makeCard(t.art, t.title || 'Unknown track', t.artist || t.album || '');
            bindPlay(card, t, currentList, currentList.indexOf(t));
            grid.appendChild(card);
        });
        appendLoadMore(grid);
    }

    // ── Playlists / Liked Songs ──────────────────────────────────
    async function showPlaylists() {
        const overlay = document.getElementById('playlist-overlay');
        if (!overlay) return;
        setText('playlist-title', 'Playlists');
        currentList = null;
        listSource = null;
        showListActions(false);
        overlay.classList.add('active');
        const grid = document.getElementById('playlist-grid');
        if (!grid) return;
        gridMessage(grid, 'Loading…');
        try {
            const res = await fetch('/api/music/playlists');
            const data = await res.json();
            if (data.error) { connectPrompt(grid); return; }
            const playlists = data.playlists || [];
            if (!playlists.length) { gridMessage(grid, 'No playlists found'); return; }
            grid.textContent = '';
            playlists.forEach(p => {
                const card = makeCard(p.art, p.title || 'Playlist', p.count ? `${p.count} tracks` : '');
                // Handler via closure — no string-built onclick, id never interpolated.
                if (p.id) {
                    card.classList.add('playlist-folder');
                    card.addEventListener('click', () => openPlaylist(p.id, p.title));
                }
                grid.appendChild(card);
            });
        } catch (e) {
            gridMessage(grid, 'Could not load playlists');
        }
    }

    async function showLikedSongs(limit) {
        const overlay = document.getElementById('playlist-overlay');
        if (!overlay) return;
        setText('playlist-title', 'Liked Songs');
        overlay.classList.add('active');
        const grid = document.getElementById('playlist-grid');
        if (!grid) return;
        gridMessage(grid, 'Loading…');
        try {
            listSource = { kind: 'liked', limit: limit || PAGE };
            const res = await fetch(`/api/music/liked?limit=${listSource.limit}`);
            const data = await res.json();
            if (data.error) { connectPrompt(grid); return; }
            renderTracks(grid, data.tracks);
        } catch (e) {
            gridMessage(grid, 'Could not load liked songs');
        }
    }

    async function openPlaylist(id, title, limit) {
        const grid = document.getElementById('playlist-grid');
        if (!grid) return;
        setText('playlist-title', title || 'Playlist');
        gridMessage(grid, 'Loading…');
        try {
            listSource = { kind: 'playlist', id, title, limit: limit || 2 * PAGE };
            const res = await fetch(`/api/music/playlist/${encodeURIComponent(id)}?limit=${listSource.limit}`);
            const data = await res.json();
            if (data.error) { connectPrompt(grid); return; }
            setText('playlist-title', data.title || title || 'Playlist');
            renderTracks(grid, data.tracks);
        } catch (e) {
            gridMessage(grid, 'Could not load playlist');
        }
    }

    // An artist's page: top songs, Play all / Shuffle all / Radio (artist radio).
    async function openArtist(id, name) {
        const overlay = document.getElementById('playlist-overlay');
        const grid = document.getElementById('playlist-grid');
        if (!overlay || !grid || !id) return;
        closeSearch();
        setText('playlist-title', name || 'Artist');
        overlay.classList.add('active');
        listSource = null;
        gridMessage(grid, 'Loading…');
        try {
            const res = await fetch(`/api/music/artist/${encodeURIComponent(id)}`);
            const data = await res.json();
            if (data.error) { gridMessage(grid, 'Could not load artist'); return; }
            setText('playlist-title', data.name || name || 'Artist');
            renderTracks(grid, data.tracks);
        } catch (e) {
            gridMessage(grid, 'Could not load artist');
        }
    }

    // Open one playlist (or the liked songs) in the fullscreen player's list
    // overlay, from outside it (the YouTube Music home screen).
    function showPlaylist(id, title) {
        enterFullscreen();
        currentList = null;
        showListActions(false);
        document.getElementById('playlist-overlay')?.classList.add('active');
        return id === 'LM' ? showLikedSongs() : openPlaylist(id, title);
    }

    function showAllPlaylists() {
        enterFullscreen();
        return showPlaylists();
    }

    function closePlaylists() {
        document.getElementById('playlist-overlay')?.classList.remove('active');
    }

    // ── Search ───────────────────────────────────────────────────
    function searchArtist() {
        const s = window.RasCar?.state || {};
        const artist = s.media?.artist || '';
        search(artist);
    }

    async function search(query) {
        const overlay = document.getElementById('search-overlay');
        if (!overlay) return;
        overlay.classList.add('active');
        setText('search-query', query ? `"${query}"` : '');
        const grid = document.getElementById('search-results-grid');
        if (!grid) return;
        if (!query || !query.trim()) { gridMessage(grid, 'Nothing playing to search'); return; }
        gridMessage(grid, 'Searching…');
        try {
            const res = await fetch(`/api/music/search?q=${encodeURIComponent(query)}`);
            const data = await res.json();
            if (data.error) { connectPrompt(grid); return; }
            // One row of artists: past the first few, YouTube's artist hits are noise.
            const artists = (data.artists || []).slice(0, 4);
            const tracks = data.tracks || [];
            if (!artists.length && !tracks.length) { gridMessage(grid, 'No results'); return; }
            grid.textContent = '';
            // Artists first, then songs.
            artists.forEach(a => {
                const card = makeCard(a.art, a.name || 'Artist', 'Artist');
                if (a.id) {
                    card.classList.add('playlist-folder');
                    card.addEventListener('click', () => openArtist(a.id, a.name));
                }
                grid.appendChild(card);
            });
            tracks.forEach(t => {
                const card = makeCard(t.art, t.title || 'Unknown track', t.artist || '');
                bindPlay(card, t);
                grid.appendChild(card);
            });
        } catch (e) {
            gridMessage(grid, 'Search failed');
        }
    }

    function closeSearch() {
        document.getElementById('search-overlay')?.classList.remove('active');
    }

    // ── Shuffle ──────────────────────────────────────────────────
    // Asks the player to toggle shuffle; the button's lit state is then driven by
    // the `shuffle` field of the media payload, never by a local flag.
    async function toggleShuffle() {
        try {
            // Our own player shuffles its queue; other players via MPRIS.
            const ours = !!window.RasCar?.state?.music?.ours;
            const res = await fetch(ours ? '/api/music/shuffle' : '/api/media/shuffle', { method: 'POST' });
            if (!res.ok) throw new Error(`HTTP ${res.status}`);
        } catch (e) {
            toast('Shuffle unavailable');
        }
    }

    // ── Repeat / seek (our own player only) ──────────────────────
    async function cycleRepeat() {
        if (!window.RasCar?.state?.music?.ours) { toast('Repeat works with YouTube Music'); return; }
        const data = await postMusic('/api/music/repeat', {});
        if (data) toast({ off: 'Repeat off', all: 'Repeat all', one: 'Repeat this song' }[data.repeat] || 'Repeat');
    }

    // Tap on the progress line: jump there.
    function seekTo(e) {
        const s = window.RasCar?.state || {};
        const m = s.media || {};
        if (!s.music?.ours || !(m.length_sec > 0)) return;
        const bar = document.getElementById('fs-progress-bar');
        const r = bar.getBoundingClientRect();
        const frac = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
        postMusic('/api/music/seek', { position: frac * m.length_sec });
    }

    // ── Like / Dislike ───────────────────────────────────────────
    async function like() { await rateTrack('like'); }
    async function dislike() { await rateTrack('dislike'); }

    async function rateTrack(kind) {
        try {
            const res = await fetch(`/api/media/${kind}`, { method: 'POST' });
            const data = await res.json();
            if (data && data.ok && data.rated) {
                reflectRating(kind);
            } else {
                toast((data && data.error) || 'Could not rate track');
            }
        } catch (e) {
            toast('Could not rate track');
        }
    }

    // Toggle the thumbs buttons so only the chosen rating shows as active.
    function reflectRating(kind) {
        // The fullscreen player's thumbs and the home card's.
        document.querySelectorAll('.fs-transport-btn.like, .media-rate.like')
            .forEach(b => b.classList.toggle('active', kind === 'like'));
        document.querySelectorAll('.fs-transport-btn.dislike, .media-rate.dislike')
            .forEach(b => b.classList.toggle('active', kind === 'dislike'));
    }

    // Minimal transient toast (created on demand, no HTML dependency).
    let toastTimer = null;
    function toast(msg) {
        let el = document.getElementById('media-toast');
        if (!el) {
            el = document.createElement('div');
            el.id = 'media-toast';
            el.className = 'media-toast';
            document.body.appendChild(el);
        }
        el.textContent = msg;
        el.style.opacity = '1';
        if (toastTimer) clearTimeout(toastTimer);
        toastTimer = setTimeout(() => { el.style.opacity = '0'; }, 2500);
    }

    // ── WS Update ────────────────────────────────────────────────
    let lastRatedTrack = '';
    function onWsUpdate(s) {
        // Clear the thumbs highlight when the track changes (rating is per-track).
        const m = (s && s.media) || {};
        const key = `${m.title || ''}|${m.artist || ''}`;
        if (key !== lastRatedTrack) {
            lastRatedTrack = key;
            reflectRating(null); // toggle both off
            // Lyrics belong to the previous track — refetch instead of scrolling on.
            if (karaokeActive) fetchLyrics();
        }
        // Sync fullscreen UI if open
        if (document.getElementById('media-fullscreen')?.classList.contains('active')) {
            syncFullscreenUI();
        }
        noteClock(m);
    }

    // ── Resume the last session after a reboot ───────────────────
    // Radio and TV play in this page, so the page asks the backend what to
    // resume (POST /api/resume/claim, which also counts the attempt). While
    // offline the backend answers "retry in N s": that is how the page follows
    // 4G / Wi-Fi coming up. YouTube Music (mpv) is resumed by the backend
    // itself. No page switch: the sound just comes back (a TV channel in its
    // picture-in-picture window, as before), with a toast.
    let resumeDone = false;     // the driver picked or stopped something
    let resumeTimer = null;

    function reportSession(event, source, station) {
        if (event === 'play' || event === 'stop') { resumeDone = true; clearTimeout(resumeTimer); }
        fetch('/api/resume/report', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ event, source, name: station?.name || '', url: station?.url || '' }),
        }).catch(() => { /* best effort */ });
    }

    const sleep = ms => new Promise(r => setTimeout(r, ms));
    async function waitUntil(test, ms) {
        const end = Date.now() + ms;
        while (!test()) {
            if (Date.now() > end) return false;
            await sleep(250);
        }
        return true;
    }

    function scheduleResume(sec) {
        clearTimeout(resumeTimer);
        if (!resumeDone) resumeTimer = setTimeout(resumeLastSession, sec * 1000);
    }

    /* Index of the saved station in a list: same stream URL, else same name. */
    function findEntry(list, r) {
        if (!Array.isArray(list)) return -1;
        const byUrl = list.findIndex(x => x.url === r.url);
        return byUrl >= 0 ? byUrl : list.findIndex(x => x.name === r.name);
    }

    /* Playing = the media element's clock advances for a while (an error page
       or a dead stream never gets there). */
    async function confirmPlaying(el) {
        if (!el) return false;
        // A dead stream fails fast (el.error): no need to sit out the 30 s.
        if (!await waitUntil(() => el.error || (!el.paused && el.currentTime > 0), 30000) || el.error) return false;
        const t0 = el.currentTime;
        await sleep(10000);
        return !el.paused && !el.error && el.currentTime > t0 + 5;
    }

    async function resumeLastSession() {
        if (resumeDone) return;
        // The station / channel lists come from the backend right after load.
        await waitUntil(() => (typeof radioStations !== 'undefined' && radioStations.length)
            || (typeof tvChannels !== 'undefined' && tvChannels.length), 20000);
        if (resumeDone) return;
        let c;
        try { c = await (await fetch('/api/resume/claim', { method: 'POST' })).json(); }
        catch (e) { scheduleResume(10); return; }
        if (!c.resume) {
            if (c.retry_in) scheduleResume(c.retry_in);
            return;
        }
        const r = c.resume;
        const radio = r.source === 'radio';
        const idx = findEntry(radio ? radioStations : tvChannels, r);
        if (idx < 0) {
            // Removed from the list since: nothing to retry.
            reportSession('fail', r.source, r);
            return;
        }
        toast(`Resuming ${r.name}`);
        if (radio) playStation(idx, { resumed: true });
        else playTvChannelByIdx(idx, { resumed: true });
        const ok = await confirmPlaying(document.getElementById(radio ? 'radio-player' : 'tv-player'));
        if (resumeDone) return;   // the driver took over meanwhile
        if (ok) {
            reportSession('ok', r.source, r);
        } else {
            reportSession('fail', r.source, r);
            scheduleResume(20);   // the backend caps the attempts
        }
    }

    // Keeps the saved session's age current while it plays (the TV is only
    // resumed after a short stop, so "last heard playing" is what counts).
    function sessionHeartbeat() {
        const audio = document.getElementById('radio-player');
        if (audio && !audio.paused) reportSession('alive', 'radio');
        const video = document.getElementById('tv-player');
        if (video && !video.paused && typeof tvIsPlaying === 'function' && tvIsPlaying()) reportSession('alive', 'tv');
    }

    document.addEventListener('DOMContentLoaded', () => {
        resumeLastSession();
        setInterval(sessionHeartbeat, 60000);
    });

    // ── Helpers ──────────────────────────────────────────────────
    function setText(id, text) {
        const el = document.getElementById(id);
        if (el) el.textContent = text;
    }

    // ── Export ────────────────────────────────────────────────────
    window.RasCar = window.RasCar || {};
    RasCar.register = RasCar.register || function(name, mod) { RasCar._modules = RasCar._modules || {}; RasCar._modules[name] = mod; RasCar[name] = mod; };

    RasCar.register('media', {
        init, onWsUpdate, onVizFrame, connectToAudio, pauseAudio, disconnectAudio,
        startVizLoop, stopVizLoop, toast,
        nextVizMode, getVizMode: () => vizMode, getVizModeName: () => VIZ_MODES[vizMode],
        enterFullscreen, exitFullscreen, enterVizFullscreen,
        toggleKaraoke, toggleShuffle, cycleRepeat, seekTo, playAll, playList, playTrack, listRadio, openArtist,
        showPlaylists, showLikedSongs, openPlaylist, closePlaylists, showPlaylist, showAllPlaylists, makeCard,
        searchArtist, search, closeSearch,
        like, dislike,
    });

    // Also expose connectToAudio globally for backward compat with radio
    window.connectVisualizerToAudio = connectToAudio;
    window.pauseVisualizerAudio = pauseAudio;
    window.disconnectVisualizerAudio = disconnectAudio;
    // Radio / TV report the driver's choices for the resume after a reboot.
    window.reportMediaSession = reportSession;
})();

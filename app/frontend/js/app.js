/**
 * RasCar Dashboard — Main JavaScript
 *
 * Handles:
 *  - Horizontal swipe navigation (11 pages, touch + mouse) + page indicator
 *  - WebSocket connection for real-time data
 *  - UI updates for all screens
 *  - Screensaver timeout
 *  - Gauge rendering (Canvas)
 *  - Chart rendering (Canvas)
 */

// --- State ---
let currentPage = 4; // Home (Media)
const TOTAL_PAGES = 14;
let ws = null;
let lastInteraction = Date.now();
let screensaverTimeout = 300; // seconds
let calendarDate = new Date();
let state = {};
// The media source the driver last played ('radio' | 'tv' | 'mpris'): where a
// car Play key goes when nothing is playing (see carKey).
let lastMediaSource = null;

// --- Security helper: escape untrusted strings before they touch innerHTML ---
// Defined on window so media.js (loaded first) can reuse the same implementation.
function escapeHtml(s) {
    if (s == null) return '';
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}
window.escapeHtml = escapeHtml;

// --- Security helper: safely render album art without string-built URLs/handlers ---
// Builds an <img> via the DOM, assigns .src (never interpolated into innerHTML),
// and attaches the music-note fallback via addEventListener.
function setArtImage(container, url) {
    if (!container) return;
    container.textContent = '';
    const img = document.createElement('img');
    img.addEventListener('error', () => {
        container.textContent = '';
        const icon = document.createElement('i');
        icon.className = 'fas fa-music';
        container.appendChild(icon);
    });
    img.referrerPolicy = 'no-referrer'; // yt3.ggpht.com answers 429 to a cross-site Referer
    img.src = artSrc(url); // assignment, not HTML interpolation — no markup can execute
    container.appendChild(img);
}
window.setArtImage = setArtImage;

// MPRIS reports mpris:artUrl as a file:// URI for most players, and an http-origin
// page cannot load file://. Serve it through the backend proxy instead. The original
// URI is carried as a query so the browser cache keys per track, not per route.
function artSrc(url) {
    if (!url) return url;
    return /^file:\/\//i.test(url)
        ? `/api/media/art?u=${encodeURIComponent(url)}`
        : url;
}
window.artSrc = artSrc;

// --- Unit conversion (metric base ⇄ imperial display) ---
// Backend always sends metric (km/h, °C, m). These format for the active unit system.
function isImperial() { return window.dashUnits === 'imperial'; }
function fmtSpeed(kmh) {
    const v = kmh ?? 0;
    return isImperial() ? `${Math.round(v * 0.621371)} mph` : `${Math.round(v)} km/h`;
}
function fmtTemp(c) {
    const v = c ?? 0;
    return isImperial() ? `${Math.round(v * 9 / 5 + 32)}°F` : `${Math.round(v)}°C`;
}
function fmtAlt(m) {
    const v = m ?? 0;
    return isImperial() ? `${Math.round(v * 3.28084)} ft` : `${Math.round(v)} m`;
}
function speedUnitLabel() { return isImperial() ? 'mph' : 'km/h'; }

// --- Init ---
document.addEventListener('DOMContentLoaded', () => {
    initSwipe();
    connectWebSocket();
    const savedPage = parseInt(sessionStorage.getItem('currentPage') ?? '4');
    goToPage(isNaN(savedPage) ? 4 : savedPage);
    initSwipeNavigation();
    initGlobalEscape();
    setInterval(updateClock, 1000);
    setInterval(checkScreensaver, 5000);
    setInterval(fetchCalendar, 300000);
    setInterval(fetchCalSources, 300000);
    fetchCrypto();
    setInterval(fetchCrypto, 60000);
    fetchWeatherPage();
    setInterval(fetchWeatherPage, 10 * 60000);
    setCalView(calView);
    fetchNextEvent();
    setInterval(fetchNextEvent, 60000);
    refreshHeadunit();
    setInterval(refreshHeadunit, 60000);
    loadSettings();
    setText('trip-date', tripDateLabel(tripDate));
    fetchTrips();
    fetchContacts();
    ytmRender();
    initKeyboards();
    refreshApps();
    fetchTvChannels();
    fetchRadioStations();
    startVisualizer();
    bindScreensaver();
    resetInteraction();
});

// --- Settings ---
// Last settings answered by the server; every control patches it and saves.
let currentSettings = {};
const THEMES = ['night', 'glass', 'amber', 'day'];

async function loadSettings() {
    try {
        const res = await fetch('/api/settings');
        currentSettings = await res.json();
        applySettings(currentSettings);
        renderSettings();
        // The calendar's hidden agendas come from settings: redraw with them.
        renderCalLegend();
        fetchCalendar();
    } catch (e) {
        console.warn('loadSettings failed:', e);
    }
}

function applyTheme(theme) {
    const id = THEMES.includes(theme) ? theme : 'night';
    document.documentElement.dataset.theme = id;
    try { localStorage.setItem('rascar-theme', id); } catch (e) { /* cache only */ }
    document.querySelectorAll('.theme-card')
        .forEach(c => c.classList.toggle('active', c.dataset.themeChoice === id));
    // Canvases read colours when they draw: tell the live ones to re-read.
    document.dispatchEvent(new Event('rascar-theme'));
    if (selectedTrip) renderTripChart(selectedTrip, selectedTripMetric);
}

function renderSettings() {
    const s = currentSettings;
    const alarm = s.speed_alarm_kmh ?? 0;
    setText('set-speed-alarm', alarm ? `${alarm} km/h` : 'Off');
    const choices = { units: s.units ?? 'metric', screensaver: String(s.screensaver_timeout ?? 300) };
    document.querySelectorAll('.setting-choice').forEach(group => {
        group.querySelectorAll('.media-tab').forEach(b =>
            b.classList.toggle('active', b.dataset.value === choices[group.dataset.group]));
    });
    setText('set-fuel-tank', `${Math.round(s.fuel_tank_l ?? 63)} L`);
    setText('set-fuel-price', `${(s.fuel_price_eur_l ?? 1.8).toFixed(2)} €/L`);
    setText('set-fuel-l100', `${(s.fuel_l100 ?? 9.5).toFixed(1)} L/100`);
    const n = s.calendar_sources ?? 0;
    setText('set-calendar-sources', n ? `${n} calendar${n > 1 ? 's' : ''} connected` : 'None');
    document.querySelectorAll('.theme-card')
        .forEach(c => c.classList.toggle('active', c.dataset.themeChoice === (s.theme || 'night')));
}

/* Partial save: /api/settings keeps every field the patch leaves out. */
async function saveSettingsPatch(patch) {
    try {
        const res = await fetch('/api/settings', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(patch),
        });
        // fetch() does not reject on 4xx/5xx — a rejected save must not look like success.
        if (!res.ok) { notify(`Setting not saved (server said ${res.status})`); return false; }
        currentSettings = { ...currentSettings, ...patch };
        applySettings(currentSettings);
        renderSettings();
        return true;
    } catch (e) {
        console.warn('settings save failed:', e);
        notify('Setting not saved — dashboard unreachable');
        return false;
    }
}

function chooseTheme(theme) {
    const previous = currentSettings.theme || 'night';
    applyTheme(theme);
    saveSettingsPatch({ theme }).then(ok => { if (!ok) applyTheme(previous); });
}

function setSettingChoice(group, value) {
    if (group === 'units') saveSettingsPatch({ units: value });
    if (group === 'screensaver') saveSettingsPatch({ screensaver_timeout: value });
}

let speedAlarmTimer = null;
function stepSpeedAlarm(delta) {
    const next = Math.min(250, Math.max(0, (currentSettings.speed_alarm_kmh ?? 0) + delta));
    currentSettings.speed_alarm_kmh = next;
    renderSettings();
    // Tapping +/+/+ quickly sends one save, not one per tap.
    clearTimeout(speedAlarmTimer);
    speedAlarmTimer = setTimeout(() => saveSettingsPatch({ speed_alarm_kmh: next }), 600);
}

/* Fuel settings: step, range and default per key (the server clamps to the same ranges). */
const FUEL_STEPS = {
    fuel_tank_l:      { step: 1,    min: 10,  max: 150, def: 63 },
    fuel_price_eur_l: { step: 0.05, min: 0.5, max: 5,   def: 1.8 },
    fuel_l100:        { step: 0.1,  min: 2,   max: 30,  def: 9.5 },
};
const fuelSettingTimers = {};
function stepFuelSetting(key, dir) {
    const f = FUEL_STEPS[key];
    const cur = currentSettings[key] ?? f.def;
    const next = Math.round(Math.min(f.max, Math.max(f.min, cur + dir * f.step)) / f.step) * f.step;
    currentSettings[key] = Number(next.toFixed(2));
    renderSettings();
    clearTimeout(fuelSettingTimers[key]);
    fuelSettingTimers[key] = setTimeout(() => saveSettingsPatch({ [key]: currentSettings[key] }), 600);
}

function settingsSection(name) {
    document.querySelectorAll('.settings-tab')
        .forEach(t => t.classList.toggle('active', t.dataset.section === name));
    document.querySelectorAll('.settings-section')
        .forEach(sec => sec.classList.toggle('hidden', sec.dataset.section !== name));
    if (name === 'services') { refreshYtmusicStatus(); refreshHeadunit(); }
    if (window.RasCar && RasCar.connections) RasCar.connections.onSection(name);
}


// Transient user-visible message. Reuses the media module's toast; falls back to
// the console when media.js has not registered yet.
function notify(msg) {
    if (window.RasCar && RasCar.media && RasCar.media.toast) RasCar.media.toast(msg);
    else console.warn(msg);
}

function applySettings(s) {
    screensaverTimeout = s.screensaver_timeout ?? 300;
    window.dashUnits = s.units ?? 'metric';
    window.speedAlarm = s.speed_alarm_kmh ?? 0;
    applyTheme(s.theme);
}

// --- WebSocket (single guarded reconnect with exponential backoff) ---
let wsReconnectTimer = null;
let wsBackoff = 2000;          // start at 2s
const WS_BACKOFF_MAX = 30000;  // cap at 30s

function setConnectionStatus(online) {
    const el = document.getElementById('sb-offline');
    if (el) el.classList.toggle('hidden', online);
}

function scheduleReconnect() {
    if (wsReconnectTimer) return; // already scheduled — never stack timers
    wsReconnectTimer = setTimeout(() => {
        wsReconnectTimer = null;
        connectWebSocket();
    }, wsBackoff);
    wsBackoff = Math.min(wsBackoff * 2, WS_BACKOFF_MAX);
}

function connectWebSocket() {
    // Tear down any previous socket so its stale handlers can't fire reconnects.
    if (ws) {
        ws.onclose = null;
        ws.onerror = null;
        ws.onmessage = null;
        try { ws.close(); } catch (e) {}
    }
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    ws = new WebSocket(`${proto}://${location.host}/ws`);
    ws.onopen = () => {
        wsBackoff = 2000;            // reset backoff on success
        if (wsReconnectTimer) { clearTimeout(wsReconnectTimer); wsReconnectTimer = null; }
        setConnectionStatus(true);
    };
    ws.onmessage = (e) => {
        const msg = JSON.parse(e.data);
        if (msg.type === 'viz') { if (window.RasCar && RasCar.media) RasCar.media.onVizFrame(msg); return; }
        if (msg.type === 'touch') { showTouch(msg); return; }
        if (msg.type === 'carkey') { carKey(msg.key); return; }
        if (msg.type === 'signin-done') { onSignInDone(msg); return; }
        if (msg.type === 'arrival') { if (window.RasCar && RasCar.home) RasCar.home.arrival(); return; }
        state = msg;
        updateUI(state);
    };
    ws.onclose = () => {
        setConnectionStatus(false);
        scheduleReconnect();
    };
    ws.onerror = () => {
        setConnectionStatus(false);
        try { ws.close(); } catch (e) {} // triggers onclose → scheduleReconnect
    };
}

// --- UI Update ---
function updateUI(s) {
    // Publish the latest WS state so media.js (fullscreen view, lyrics, search) can read it.
    if (window.RasCar) RasCar.state = s;

    // Status bar
    setText('sb-clock', s.time || '--:--');
    setText('sb-weather-temp', fmtTemp(s.weather?.temperature));

    // Speed in status bar (metric base, displayed per active units)
    const speedKmh = Math.round(s.gps?.speed ?? 0);
    const dispSpeed = isImperial() ? Math.round(speedKmh * 0.621371) : speedKmh;
    const speedEl = document.getElementById('sb-speed');
    if (speedEl) {
        // Build via DOM (textContent) — no untrusted data, but avoids HTML strings.
        speedEl.textContent = `${dispSpeed} `;
        const small = document.createElement('small');
        small.textContent = speedUnitLabel();
        speedEl.appendChild(small);
        // Threshold comes from Settings (metric base); 0 disables the alarm.
        speedEl.classList.toggle('alarm', window.speedAlarm > 0 && speedKmh > window.speedAlarm);
    }

    // Home glance card: same values as the status row.
    setText('home-speed', dispSpeed);
    setText('home-speed-unit', speedUnitLabel());
    setText('home-temp', fmtTemp(s.weather?.temperature));

    // GPS page
    setText('gps-speed', dispSpeed);
    setText('gps-alt', fmtAlt(s.gps?.alt));
    setText('gps-bearing', bearingToCompass(s.gps?.bearing ?? 0));
    setText('gps-coords', `${(s.gps?.lat ?? 0).toFixed(5)}, ${(s.gps?.lon ?? 0).toFixed(5)}`);
    setText('gps-speed-unit', speedUnitLabel());
    renderGpsSky(s.gps);

    // OBD page
    setText('obd-rpm', Math.round(s.obd?.rpm ?? 0));
    setText('obd-speed', fmtSpeed(s.obd?.speed));
    setText('obd-coolant', fmtTemp(s.obd?.coolant));
    setText('obd-battery', `${(s.obd?.battery ?? 0).toFixed(1)}V`);
    renderObdMetrics(s.obd?.metrics);
    renderObdHealth(s.obd?.health, s.obd?.metrics);

    const dtcEl = document.getElementById('obd-dtc');
    if (dtcEl) {
        const dtc = s.obd?.dtc ?? 0;
        dtcEl.classList.toggle('hidden', dtc === 0);
        setText('obd-dtc-count', `${dtc} DTC${dtc !== 1 ? 's' : ''}`);
    }

    setText('gps-accuracy', `${(s.gps?.accuracy ?? 0).toFixed(1)} m`);

    // Provenance of the position. The backend reports `mock` when the gpsd
    // supervisor has fallen back to synthetic data; showing it identically to a
    // real fix is what made fabricated trips indistinguishable before.
    const gpsSrc = document.getElementById('gps-source');
    if (gpsSrc) {
        const mock = s.gps?.mock === true;
        const noFix = !mock && s.gps?.fix === false;
        gpsSrc.classList.toggle('hidden', !(mock || noFix));
        gpsSrc.classList.toggle('no-fix', noFix);
        if (mock) gpsSrc.textContent = 'SIMULATED POSITION';
        else if (noFix) gpsSrc.textContent = 'NO GPS FIX';
    }
    const recBadge = document.getElementById('trip-recording-badge');
    if (recBadge) recBadge.classList.toggle('hidden', !s.trip_recording);

    const weatherIcon = document.getElementById('sb-weather-icon');
    if (weatherIcon) weatherIcon.className = `fas fa-${s.weather?.icon ?? 'cloud'}`;

    // Signal bars (dBm → 0-5 bars)
    const dbm = s.signal ?? -100;
    const bars = dbm >= -55 ? 5 : dbm >= -65 ? 4 : dbm >= -75 ? 3 : dbm >= -85 ? 2 : dbm >= -95 ? 1 : 0;
    document.querySelectorAll('#sb-signal .sig-bar').forEach((bar, i) => {
        bar.classList.toggle('active', i < bars);
    });

    updateCellIndicator(s.cell);
    document.getElementById('sb-car-native')?.classList.toggle('inactive', !s.carplay);

    // The status-bar Bluetooth icon reflects the only real Bluetooth link this
    // dashboard owns: the ELM327 OBD-II adapter (paired over RFCOMM).
    const btIcon = document.getElementById('sb-bt');
    if (btIcon) btIcon.classList.toggle('active', !!s.obd?.connected);

    const obdStatus = document.getElementById('obd-status');
    if (obdStatus) {
        obdStatus.className = `obd-status ${s.obd?.connected ? 'connected' : 'disconnected'}`;
        obdStatus.querySelector('span').textContent = s.obd?.connected ? 'Connected' : 'Disconnected';
    }

    // Gauges — treat missing/0 readings as "no data" (grey/alarm), never healthy-green.
    const obdConnected = !!s.obd?.connected;
    const rpm = s.obd?.rpm ?? 0;
    const obdSpeed = s.obd?.speed ?? 0;
    const coolant = s.obd?.coolant ?? 0;
    const battery = s.obd?.battery ?? 0;
    const NO_DATA = `color-mix(in srgb, ${cssToken('--text-secondary')} 45%, transparent)`;
    const OK = cssToken('--success'), HOT = cssToken('--danger'), WARM = cssToken('--warning');
    drawGauge('gauge-rpm', rpm, 8000, cssToken('--accent'));
    drawGauge('gauge-speed', obdSpeed, 240, OK);
    // Coolant: grey when no OBD/no reading; orange normal; red when too hot.
    drawGauge('gauge-coolant', coolant, 130,
        (!obdConnected || coolant <= 0) ? NO_DATA : (coolant > 105 ? HOT : WARM));
    // Battery: a disconnected OBD / 0V must NOT read healthy-green — show alarm/no-data.
    drawGauge('gauge-battery', battery, 16,
        (!obdConnected || battery <= 0) ? NO_DATA : (battery < 12 ? HOT : OK));

    // Media — only override if WS provides actual media metadata (not radio)
    const m = s.media || {};
    const radioAudio = document.getElementById('radio-player');
    const radioIsPlaying = radioAudio && !radioAudio.paused;
    // Chromium's own MPRIS entry mirrors the page's radio/TV, not a separate player.
    if (m.is_playing && !(m.player_name || '').startsWith('chromium')) lastMediaSource = 'mpris';
    if (m.title) {
        setText('media-title', m.title);
        setText('media-artist', m.artist || '');
        const playBtn = document.querySelector('#media-play-btn i');
        if (playBtn) playBtn.className = m.is_playing ? 'fas fa-pause' : 'fas fa-play';
        // Thumbs only for our YouTube Music player (mpv): other players have no rating.
        const ytm = (m.player_name || '').startsWith('mpv');
        document.querySelectorAll('.media-rate').forEach(b => b.classList.toggle('hidden', !ytm));
        const artEl = document.getElementById('media-art');
        if (artEl && m.art_url) {
            setArtImage(artEl, m.art_url);
        }
    } else if (!radioIsPlaying) {
        setText('media-title', 'No media playing');
        setText('media-artist', '');
        document.querySelectorAll('.media-rate').forEach(b => b.classList.add('hidden'));
    }

    // Home progress bar — driven by the real position, hidden when the player
    // does not report one (never a fabricated fill).
    updateHomeProgress(m);

    // Screensaver
    // ss-clock is driven every second by updateClock() (it shows seconds).
    setText('ss-media', m.title ? `${m.artist} — ${m.title}` : '');
    setText('ss-weather', fmtTemp(s.weather?.temperature));

    // Trip cost + fuel wallet strip, refill popup.
    if (RasCar.fuel) RasCar.fuel.onWs(s);
    // Modem phone call: incoming overlay, in-call screen, status pill (js/phone.js).
    RasCar.phone?.onWs(s);

    // Drive stats tab

    // Let the fullscreen media view live-update from the same WS payload.
    if (RasCar.media && RasCar.media.onWsUpdate) RasCar.media.onWsUpdate(s);
    // The offline map follows the car (it keeps the fix while off screen).
    RasCar.navmap?.onGps(s.gps);
}

// --- 4G stick indicator (status row) ---
/* One line of text for the current 4G state; also the tap-for-details toast. */
function cellSummary(c) {
    if (!c || !c.present) return 'No 4G stick plugged in';
    if (!c.at_ok) return '4G stick found, modem not answering yet';
    if (!c.registered) return '4G: no service (not registered)';
    const parts = [c.operator || 'Unknown operator', c.tech || '4G'];
    if (c.rsrp_dbm != null) parts.push(`RSRP ${c.rsrp_dbm} dBm`);
    else if (c.rssi_dbm != null) parts.push(`RSSI ${c.rssi_dbm} dBm`);
    parts.push(c.internet ? 'internet OK' : 'NO INTERNET');
    if (c.link && c.link !== 'connected') parts.push(`data link: ${c.link}`);
    return parts.join(' · ');
}

function updateCellIndicator(c) {
    const el = document.getElementById('sb-cell');
    if (!el) return;
    const present = !!c?.present;
    const registered = present && !!c.registered;
    // Label: the live technology, or a plain "No 4G" without a stick.
    // Switched off from the 4G panel: cell.css appends " OFF", so the label is
    // the plain "4G" (never "No 4G OFF").
    const switchedOff = document.getElementById('sb-cell')?.classList.contains('cell-off');
    setText('sb-cell-label', switchedOff ? '4G' : !present ? 'No 4G' : (registered ? (c.tech || '4G') : '4G'));
    const bars = registered ? (c.bars ?? 0) : 0;
    el.querySelectorAll('.sig-bar').forEach((bar, i) => bar.classList.toggle('active', i < bars));
    el.querySelector('.sb-signal')?.classList.toggle('hidden', !present);
    // Registered but the handshake through the stick failed: say so.
    document.getElementById('sb-cell-warn')?.classList.toggle('hidden', !(registered && !c.internet));
    el.setAttribute('aria-label', cellSummary(c));
}

function showCellInfo() { notify(cellSummary(state.cell)); }

// --- "Car" button: the Jaguar's native screen ---
/* The hint first: once the car shows its own UI this screen is gone, and the
   way back (the car's CarPlay icon) is not obvious. */
let carNativePending = null;
function carNativeScreen() {
    if (!state.carplay) { notify('No CarPlay session: the car already shows its own screen'); return; }
    if (carNativePending) return;
    notify('Tap the CarPlay icon on the car screen to come back');
    carNativePending = setTimeout(async () => {
        carNativePending = null;
        try {
            const r = await (await fetch('/api/carplay/native', { method: 'POST' })).json();
            if (!r.ok) notify('Could not switch to the car screen');
        } catch (e) {
            notify('Could not switch to the car screen');
        }
    }, 1800);
}

// --- Playback position helpers (shared with media.js) ---
// A position is usable only when both numbers are present and the track has a
// real length; anything else means "unknown" and the progress UI must hide.
function hasPlaybackPosition(m) {
    return m != null
        && typeof m.position_sec === 'number' && Number.isFinite(m.position_sec)
        && typeof m.length_sec === 'number' && Number.isFinite(m.length_sec)
        && m.length_sec > 0;
}
window.hasPlaybackPosition = hasPlaybackPosition;

function playbackPct(m) {
    return Math.max(0, Math.min(100, (m.position_sec / m.length_sec) * 100));
}
window.playbackPct = playbackPct;

// Seconds → m:ss (h:mm:ss past an hour).
function fmtDuration(sec) {
    const total = Math.max(0, Math.floor(sec || 0));
    const h = Math.floor(total / 3600);
    const mn = Math.floor((total % 3600) / 60);
    const s = total % 60;
    const pad = (n) => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${pad(mn)}:${pad(s)}` : `${mn}:${pad(s)}`;
}
window.fmtDuration = fmtDuration;

function updateHomeProgress(m) {
    const row = document.getElementById('media-progress');
    const fill = document.getElementById('media-progress-bar');
    if (!row || !fill) return;
    const known = hasPlaybackPosition(m);
    row.classList.toggle('hidden', !known);
    fill.style.width = known ? `${playbackPct(m)}%` : '0%';
}

// --- Swipe Navigation (disabled — use nav popup instead) ---
function initSwipe() {
    // Touch/mouse interactions only reset the screensaver timer; no swipe navigation.
    const container = document.getElementById('pages-container');
    container.addEventListener('touchstart', resetInteraction, { passive: true });
    container.addEventListener('mousedown', resetInteraction);
}

/* An optional feature's page (e.g. Home without a home server) carries
   .page-disabled: it keeps its slot in the track but is never shown. */
function pageDisabled(idx) {
    return !!document.querySelector(`.page[data-page="${idx}"]`)?.classList.contains('page-disabled');
}

function goToPage(idx) {
    idx = Math.max(0, Math.min(TOTAL_PAGES - 1, idx));
    if (pageDisabled(idx)) idx = 4;
    currentPage = idx;
    if (currentPage === 12) ytmLoadHome(false);
    sessionStorage.setItem('currentPage', currentPage);
    snapToPage();
    updatePageIndicator();
    // Let the visualizer re-evaluate whether it should be drawing on this page.
    // startVizLoop() no-ops unless vizShouldRun() (audio live + media page/fullscreen);
    // the running frame self-stops via vizShouldRun() when we leave the media page.
    if (RasCar.media) RasCar.media.startVizLoop();
}

function snapToPage(animate) {
    const track = document.getElementById('pages-track');
    track.style.transition = animate ? 'transform 0.22s ease-out' : 'none';
    // One page = the container width (800 px on the original display, 1200 on the
    // Jaguar); read it live so the track follows whatever size the car negotiated.
    const pageWidth = document.getElementById('pages-container').clientWidth;
    track.style.transform = `translateX(${-currentPage * pageWidth}px)`;
}

// Re-align the track if the window size changes (e.g. the kiosk is resized).
window.addEventListener('resize', () => snapToPage(false));

/* Build the dots once, then only toggle .active. Also marks the current
   destination in the nav popup, which previously showed no "you are here". */
function buildPageIndicator() {
    const el = document.getElementById('page-indicator');
    if (!el || el.childElementCount) return;
    for (let i = 0; i < TOTAL_PAGES; i++) {
        const d = document.createElement('div');
        d.className = 'dot';
        el.appendChild(d);
    }
}

let pageIndicatorTimer = null;
function updatePageIndicator() {
    const el = document.getElementById('page-indicator');
    if (el) {
        buildPageIndicator();
        [...el.children].forEach((d, i) => {
            d.classList.toggle('active', i === currentPage);
            d.classList.toggle('hidden', pageDisabled(i));
        });
        // Flash it on every move, then fade — see the CSS note on why it is not
        // permanently visible.
        el.classList.add('visible');
        if (pageIndicatorTimer) clearTimeout(pageIndicatorTimer);
        pageIndicatorTimer = setTimeout(() => el.classList.remove('visible'), 1600);
    }
    updateTvPip();
    if (currentPage === 11) fetchWeatherPage();
    if (currentPage === 6) refreshApps();
    if (currentPage === 3) renderNavMap();
    updateAppLayer();
    // The dock is the primary "where am I": light up the button of this page.
    document.querySelectorAll('.dock-btn[data-page]').forEach(btn => {
        btn.classList.toggle('active', Number(btn.dataset.page) === currentPage);
    });
    document.querySelectorAll('.nav-popup-tile').forEach(tile => {
        const m = (tile.getAttribute('onclick') || '').match(/navPopupGo\((\d+)\)/);
        tile.classList.toggle('current', !!m && Number(m[1]) === currentPage);
    });
}

/* Horizontal swipe. The file header advertised this for months but no handler
   existed: 11 pages were reachable only through one 44px button, three taps
   deep. Vertical drags are handed back to the scrollable lists untouched. */
const SWIPE_MIN_PX = 45;        // below this it is a tap, not a swipe
const SWIPE_MAX_OFF_AXIS = 0.8; // |dy| must stay under this fraction of |dx|
function initSwipeNavigation() {
    const container = document.getElementById('pages-container');
    if (!container) return;
    let startX = 0, startY = 0, tracking = false, decided = false, horizontal = false;

    // A drag on a map pans the map; it must not also flip the page (the offline
    // MapLibre map and the Leaflet maps live in this document, unlike the
    // Google/Waze frames that swallow their own gestures).
    const PAN_SURFACES = '.maplibregl-map, .leaflet-container, [data-no-swipe]';
    const begin = (x, y, target) => {
        if (target?.closest?.(PAN_SURFACES)) { tracking = false; return; }
        startX = x; startY = y; tracking = true; decided = false; horizontal = false;
    };
    const move = (x, y, ev) => {
        if (!tracking) return;
        const dx = x - startX, dy = y - startY;
        if (!decided && (Math.abs(dx) > 10 || Math.abs(dy) > 10)) {
            decided = true;
            horizontal = Math.abs(dx) > Math.abs(dy);
        }
        // Only claim the gesture once it is clearly horizontal, so vertical
        // scrolling inside .scrollable lists keeps working.
        if (decided && horizontal && ev.cancelable) ev.preventDefault();
    };
    const end = (x, y) => {
        if (!tracking) return;
        tracking = false;
        const dx = x - startX, dy = y - startY;
        if (!horizontal) return;
        if (Math.abs(dx) < SWIPE_MIN_PX) return;
        if (Math.abs(dy) > Math.abs(dx) * SWIPE_MAX_OFF_AXIS) return;
        const step = dx < 0 ? 1 : -1;
        let target = currentPage + step;
        while (target >= 0 && target < TOTAL_PAGES && pageDisabled(target)) target += step;
        if (target < 0 || target > TOTAL_PAGES - 1) return;
        currentPage = target;
        sessionStorage.setItem('currentPage', currentPage);
        snapToPage(true);
        updatePageIndicator();
        if (RasCar.media) RasCar.media.startVizLoop();
    };

    container.addEventListener('touchstart', e => {
        if (e.touches.length !== 1) return;
        begin(e.touches[0].clientX, e.touches[0].clientY, e.target);
    }, { passive: true });
    container.addEventListener('touchmove', e => {
        if (e.touches.length !== 1) return;
        move(e.touches[0].clientX, e.touches[0].clientY, e);
    }, { passive: false });
    container.addEventListener('touchend', e => {
        const t = e.changedTouches[0];
        if (t) end(t.clientX, t.clientY);
    }, { passive: true });

    // Mouse equivalent, for the bench and for a connected pointer.
    container.addEventListener('mousedown', e => begin(e.clientX, e.clientY, e.target));
    container.addEventListener('mousemove', e => move(e.clientX, e.clientY, e));
    container.addEventListener('mouseup', e => end(e.clientX, e.clientY));
    container.addEventListener('mouseleave', () => { tracking = false; });
}

// --- Gauge Drawing ---
function drawGauge(canvasId, value, max, color) {
    const canvas = document.getElementById(canvasId);
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    const w = canvas.width, h = canvas.height;
    const cx = w / 2, cy = h / 2, r = Math.min(w, h) / 2 - 10;

    ctx.clearRect(0, 0, w, h);

    // Background arc
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0.75 * Math.PI, 2.25 * Math.PI);
    ctx.strokeStyle = `color-mix(in srgb, ${cssToken('--text-primary')} 10%, transparent)`;
    ctx.lineWidth = 8;
    ctx.lineCap = 'round';
    ctx.stroke();

    // Value arc
    const pct = Math.min(value / max, 1);
    const endAngle = 0.75 * Math.PI + pct * 1.5 * Math.PI;
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0.75 * Math.PI, endAngle);
    ctx.strokeStyle = color;
    ctx.lineWidth = 8;
    ctx.lineCap = 'round';
    ctx.stroke();

    // Glow tip (no shadowBlur — too expensive on ARM GPU)
    ctx.globalAlpha = 0.4;
    ctx.beginPath();
    ctx.arc(cx, cy, r, endAngle - 0.05, endAngle);
    ctx.strokeStyle = color;
    ctx.lineWidth = 14;
    ctx.lineCap = 'round';
    ctx.stroke();
    ctx.globalAlpha = 1.0;
}

// --- Chart Drawing ---
function drawChart(canvasId, data, color) {
    const canvas = document.getElementById(canvasId);
    if (!canvas || !data.length) return;
    const ctx = canvas.getContext('2d');
    const w = canvas.width, h = canvas.height;
    const pad = 4;

    ctx.clearRect(0, 0, w, h);

    const values = data.map(d => d.v);
    const min = Math.min(...values);
    const max = Math.max(...values) || 1;
    const range = max - min || 1;

    // Line
    ctx.beginPath();
    data.forEach((d, i) => {
        const x = pad + (i / (data.length - 1)) * (w - 2 * pad);
        const y = h - pad - ((d.v - min) / range) * (h - 2 * pad);
        i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    });
    ctx.strokeStyle = color;
    ctx.lineWidth = 2;
    ctx.stroke();

    // Fill
    const lastX = pad + ((data.length - 1) / (data.length - 1)) * (w - 2 * pad);
    ctx.lineTo(lastX, h);
    ctx.lineTo(pad, h);
    ctx.closePath();
    const grad = ctx.createLinearGradient(0, 0, 0, h);
    grad.addColorStop(0, color + '40');
    grad.addColorStop(1, color + '00');
    ctx.fillStyle = grad;
    ctx.fill();
}

// Paint a muted "unavailable" message on a chart canvas when its data can't be fetched.
function chartUnavailable(canvasId, msg = 'Data unavailable') {
    const canvas = document.getElementById(canvasId);
    if (!canvas) return;
    // Same device-resolution sizing as drawSeriesChart, so the two never fight
    // over the canvas' backing size or transform.
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || canvas.width, h = canvas.clientHeight || canvas.height;
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
    const ctx = canvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = cssToken('--text-secondary');
    ctx.font = '600 14px Manrope, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(msg, w / 2, h / 2);
}

// Refresh GPS charts periodically
setInterval(async () => {
    try {
        const resp = await fetch('/api/gps/history');
        const data = await resp.json();
        drawChart('chart-speed', data.speed, cssToken('--accent'));
        drawChart('chart-altitude', data.altitude, '#40ff80');
    } catch (e) {
        chartUnavailable('chart-speed');
        chartUnavailable('chart-altitude');
    }
}, 2000);

// --- Calendar ---
/* Day / Week / Month views over /api/calendar/range (events bucketed per local
   day by the backend). Each agenda gets a palette slot (.cal-src-N, from theme
   tokens) and can be hidden; the hidden set lives in settings.calendar_hidden. */
let calView = 'day';
try { calView = sessionStorage.getItem('calView') || 'day'; } catch (e) { /* default */ }
let calSources = [];
const CAL_PALETTE = 5;   // .cal-src-0 … .cal-src-4 in dashboard.css

const calHidden = () => new Set(currentSettings.calendar_hidden || []);
const calSrcClass = id => {
    const src = calSources.find(x => x.id === id);
    return `cal-src-${src ? src.index % CAL_PALETTE : CAL_PALETTE - 1}`;
};

/* First and last local day shown by the current view. Weeks start on Monday. */
function calRange() {
    const d = new Date(calendarDate.getFullYear(), calendarDate.getMonth(), calendarDate.getDate());
    if (calView === 'week') {
        const from = new Date(d); from.setDate(d.getDate() - (d.getDay() + 6) % 7);
        const to = new Date(from); to.setDate(from.getDate() + 6);
        return [from, to];
    }
    if (calView === 'month') {
        const first = new Date(d.getFullYear(), d.getMonth(), 1);
        const from = new Date(first); from.setDate(1 - (first.getDay() + 6) % 7);
        const last = new Date(d.getFullYear(), d.getMonth() + 1, 0);
        const to = new Date(last); to.setDate(last.getDate() + (7 - last.getDay()) % 7);
        return [from, to];
    }
    return [d, d];
}

async function fetchCalendarRange(from, to) {
    const resp = await fetch(`/api/calendar/range?from=${formatTripDate(from)}&to=${formatTripDate(to)}`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    return resp.json();
}

async function fetchCalSources() {
    try {
        calSources = await (await fetch('/api/calendar/sources')).json();
    } catch (e) { calSources = []; }
    renderCalLegend();
}

async function fetchCalendar() {
    if (!calSources.length) await fetchCalSources();
    const [from, to] = calRange();
    const view = calView;
    try {
        const byDay = await fetchCalendarRange(from, to);
        if (view !== calView) return;   // switched while loading
        renderCalendar(byDay, from, to);
    } catch (e) {
        const container = document.getElementById('cal-events');
        if (container) container.innerHTML = '<div class="cal-event"><span class="muted">Calendar unavailable</span></div>';
    }
}

function renderCalLegend() {
    const el = document.getElementById('cal-legend');
    if (!el) return;
    el.replaceChildren();
    el.classList.toggle('hidden', calSources.length < 2);
    const hidden = calHidden();
    for (const src of calSources) {
        const b = document.createElement('button');
        b.className = `media-tab cal-chip cal-src-${src.index % CAL_PALETTE}`;
        b.classList.toggle('off', hidden.has(src.id));
        const dot = document.createElement('span');
        dot.className = 'cal-dot';
        const name = document.createElement('span');
        name.textContent = src.name;          // from the feed — text, never markup
        b.append(dot, name);
        b.setAttribute('aria-label', `${hidden.has(src.id) ? 'Show' : 'Hide'} ${src.name}`);
        b.addEventListener('click', () => toggleCalSource(src.id));
        el.appendChild(b);
    }
}

function toggleCalSource(id) {
    const hidden = calHidden();
    if (hidden.has(id)) hidden.delete(id); else hidden.add(id);
    currentSettings.calendar_hidden = [...hidden];
    renderCalLegend();
    fetchCalendar();
    saveSettingsPatch({ calendar_hidden: [...hidden] });
}

/* Visible events of one day, without the hidden agendas. */
function calVisible(list) {
    const hidden = calHidden();
    return (list || []).filter(e => !hidden.has(e.source));
}

const calTime = e => e.is_all_day ? 'All day'
    : new Date(e.start).toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });

function renderCalendar(byDay, from, to) {
    const container = document.getElementById('cal-events');
    if (!container) return;
    container.className = `cal-events scrollable cal-${calView}`;
    calendarEvents = [];
    if (calView === 'day') return renderCalDay(container, calVisible(byDay[formatTripDate(from)]));
    renderCalGrid(container, byDay, from, to);
}

function renderCalDay(container, events) {
    if (!events.length) {
        container.innerHTML = '<div class="page-empty"><i class="fas fa-calendar-check"></i><p>No events</p></div>';
        return;
    }
    calendarEvents = events;
    container.innerHTML = events.map((e, i) => {
        const ongoing = e.is_ongoing ? ' ongoing' : '';
        const nowBadge = e.is_ongoing ? '<span class="cal-event-now">NOW</span> ' : '';
        const loc = e.location ? `<div class="cal-event-location"><i class="fas fa-map-marker-alt"></i> ${escapeHtml(e.location)}</div>` : '';
        const cal = e.calendar ? `<div class="cal-event-location">${escapeHtml(e.calendar)}</div>` : '';
        return `<div class="cal-event cal-src ${calSrcClass(e.source)}${ongoing}" onclick="openCalDetail(${i})">
            <div class="cal-event-time">${escapeHtml(calTime(e))}</div>
            <div>
                <div class="cal-event-title">${nowBadge}${escapeHtml(e.title)}</div>
                ${loc}${cal}
            </div>
        </div>`;
    }).join('');
}

/* Week: seven columns of compact events. Month: a 7-wide grid of day cells
   with the first event and a "+N". Tapping a day header/cell opens that day. */
function renderCalGrid(container, byDay, from, to) {
    container.replaceChildren();
    const grid = document.createElement('div');
    grid.className = `cal-grid cal-grid-${calView}`;
    const todayStr = new Date().toDateString();
    const month = calendarDate.getMonth();
    for (let d = new Date(from); d <= to; d.setDate(d.getDate() + 1)) {
        const day = new Date(d);
        const events = calVisible(byDay[formatTripDate(day)]);
        const cell = document.createElement('div');
        cell.className = 'cal-day';
        cell.classList.toggle('today', day.toDateString() === todayStr);
        cell.classList.toggle('other-month', calView === 'month' && day.getMonth() !== month);
        const week = calView === 'week';
        // Week: the day header is the "open this day" button, events open their
        // details. Month: the whole cell is the (finger-sized) target.
        const head = document.createElement(week ? 'button' : 'span');
        head.className = 'cd-sub cal-day-head';
        head.textContent = week
            ? day.toLocaleDateString('fr-FR', { weekday: 'short', day: 'numeric' })
            : String(day.getDate());
        if (week) head.addEventListener('click', () => calOpenDay(day));
        else cell.addEventListener('click', () => calOpenDay(day));
        cell.appendChild(head);
        for (const e of week ? events : events.slice(0, 1)) {
            const item = document.createElement(week ? 'button' : 'span');
            item.className = `cal-item cal-src ${calSrcClass(e.source)}`;
            item.classList.toggle('ongoing', !!e.is_ongoing);
            if (week) {
                const idx = calendarEvents.push(e) - 1;
                const t = document.createElement('small');
                t.textContent = calTime(e);
                item.append(t);
                item.addEventListener('click', () => openCalDetail(idx));
            }
            const title = document.createElement('span');
            title.textContent = e.title || '(no title)';
            item.append(title);
            cell.appendChild(item);
        }
        if (!week && events.length > 1) {
            // On the number's line: a third line would push a 6-week month off screen.
            const more = document.createElement('small');
            more.className = 'cal-more';
            more.textContent = `+${events.length - 1}`;
            head.appendChild(more);
        }
        grid.appendChild(cell);
    }
    if (calView === 'month') {
        const names = document.createElement('div');
        names.className = 'cal-grid cal-grid-month cal-weekdays';
        for (const n of ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']) {
            const s = document.createElement('span'); s.className = 'cd-sub'; s.textContent = n; names.appendChild(s);
        }
        container.appendChild(names);
    }
    container.appendChild(grid);
}

function calOpenDay(day) {
    calendarDate = new Date(day);
    setCalView('day');
}

function setCalView(view) {
    calView = view;
    try { sessionStorage.setItem('calView', view); } catch (e) { /* session only */ }
    document.querySelectorAll('.cal-views .media-tab')
        .forEach(b => b.classList.toggle('active', b.dataset.view === view));
    updateCalendarHeader();
    fetchCalendar();
}

function openCalDatePicker() {
    openDatePicker({
        selected: calendarDate,
        noun: 'event',
        onPick: calOpenDay,     // a picked day opens in the Day view
        fetchCounts: async (y, m) => {
            const byDay = await fetchCalendarRange(new Date(y, m, 1), new Date(y, m + 1, 0));
            const counts = {};
            for (const [day, list] of Object.entries(byDay)) {
                const n = calVisible(list).length;
                if (n) counts[day] = n;
            }
            return counts;
        },
    });
}

let calendarEvents = [];
let calDetailEvent = null;

/* Google descriptions are HTML (tables, <br>, Meet boilerplate). Parsing into
   an inert document and reading textContent never runs scripts or loads images. */
function descriptionText(raw) {
    if (!raw) return '';
    const html = raw.replace(/<br\s*\/?>/gi, '\n').replace(/<\/(p|div|tr|li)>/gi, '\n');
    const text = new DOMParser().parseFromString(html, 'text/html').body.textContent || '';
    return text
        .split('\n')
        .map(l => l.trim())
        .filter(l => !/^-?::~[:~]*/.test(l))       // Google Meet separator lines
        .join('\n')
        .replace(/\n{3,}/g, '\n\n')
        .trim();
}

function fmtEventWhen(e) {
    const start = new Date(e.start), end = new Date(e.end);
    const day = d => d.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' });
    const time = d => d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
    if (e.is_all_day) {
        // DTEND is exclusive: the last covered day is the one before it.
        const last = new Date(end.getTime() - 1);
        return last.toDateString() === start.toDateString()
            ? `${day(start)} · All day`
            : `${day(start)} → ${day(last)}`;
    }
    const mins = Math.round((end - start) / 60000);
    const dur = mins >= 60 ? `${Math.floor(mins / 60)} h${mins % 60 ? ` ${mins % 60}` : ''}` : `${mins} min`;
    return start.toDateString() === end.toDateString()
        ? `${day(start)} · ${time(start)} – ${time(end)} (${dur})`
        : `${day(start)} ${time(start)} → ${day(end)} ${time(end)}`;
}

function openCalDetail(i) {
    const e = calendarEvents[i];
    if (!e) return;
    calDetailEvent = e;
    setText('cal-detail-title', e.title || '(no title)');
    setText('cal-detail-when', fmtEventWhen(e));
    const loc = (e.location || '').trim();
    setText('cal-detail-loc', loc);
    document.getElementById('cal-detail-loc-row').classList.toggle('hidden', !loc);
    document.getElementById('cal-detail-nav').classList.toggle('hidden', !loc);
    calDetailMeeting = findMeetingLink(e);
    document.getElementById('cal-detail-join').classList.toggle('hidden', !calDetailMeeting);
    if (calDetailMeeting) setText('cal-detail-join-label', calDetailMeeting.label);
    const desc = descriptionText(e.description);
    const descEl = document.getElementById('cal-detail-desc');
    descEl.textContent = desc;
    descEl.classList.toggle('hidden', !desc);
    document.getElementById('cal-detail-overlay').classList.remove('hidden');
}

let calDetailMeeting = null;

/* Meet / Teams link from the event (Google puts it in the description, often
   as an HTML <a>; Outlook in the description or location). The backend
   re-validates the link, so this only has to find it. */
const MEETING_PATTERNS = [
    { re: /https:\/\/meet\.google\.com\/[a-z]{3,4}-[a-z]{3,4}-[a-z]{3,4}/i, label: 'Join Google Meet' },
    { re: /https:\/\/teams\.(?:microsoft|live)\.com\/[^\s"'<>]+/i, label: 'Join Teams meeting' },
];
function findMeetingLink(e) {
    const hrefs = [...new DOMParser().parseFromString(e.description || '', 'text/html').querySelectorAll('a[href]')]
        .map(a => a.href).join(' ');
    const haystack = `${hrefs} ${descriptionText(e.description)} ${e.location || ''}`;
    for (const { re, label } of MEETING_PATTERNS) {
        const m = haystack.match(re);
        if (m) return { url: m[0], label };
    }
    return null;
}

async function calDetailJoin() {
    const meeting = calDetailMeeting;
    if (!meeting) return;
    try {
        const res = await fetch('/api/apps/meeting', {
            method: 'POST', headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ url: meeting.url }),
        });
        const data = await res.json();
        if (!res.ok || !data.ok) { notify('Could not open the meeting'); return; }
        closeCalDetail();
        embedApp(data.id, data.name, data.url);
    } catch (err) {
        notify('Could not open the meeting — dashboard unreachable');
    }
}

function closeCalDetail() {
    document.getElementById('cal-detail-overlay').classList.add('hidden');
    calDetailEvent = null;
}

function calDetailNavigate() {
    const loc = calDetailEvent?.location;
    closeCalDetail();
    if (loc) navigateTo(loc);
}

/* ‹ › step by the current view: a day, a week or a month. */
function calStep(delta) {
    if (calView === 'month') calendarDate = new Date(calendarDate.getFullYear(), calendarDate.getMonth() + delta, 1);
    else calendarDate.setDate(calendarDate.getDate() + delta * (calView === 'week' ? 7 : 1));
    updateCalendarHeader();
    fetchCalendar();
}
function calPrevDay() { calStep(-1); }
function calNextDay() { calStep(1); }
function calToday() { calendarDate = new Date(); updateCalendarHeader(); fetchCalendar(); }

async function fetchNextEvent() {
    try {
        const resp = await fetch('/api/calendar/next');
        const event = await resp.json();
        const miniEl = document.getElementById('next-event-mini');
        const navEl = document.getElementById('nav-next-event');
        if (event && event.title) {
            const start = new Date(event.start);
            const timeStr = event.is_all_day ? 'All day' : start.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
            if (miniEl) {
                miniEl.classList.remove('hidden');
                miniEl.innerHTML = `<i class="fas fa-calendar-check cal-mini-icon"></i><strong>${escapeHtml(event.title)}</strong> <span class="muted">${escapeHtml(timeStr)}</span>`;
            }
            nextEventLocation = (event.location || '').trim();
            if (navEl) {
                navEl.classList.toggle('hidden', !nextEventLocation);
                navEl.innerHTML = `<i class="fas fa-calendar-check cal-mini-icon"></i><span><strong>${escapeHtml(event.title)}</strong>`
                    + `<small class="muted">${escapeHtml(nextEventLocation)}</small></span><i class="fas fa-route"></i>`;
            }
        } else {
            // Home keeps its glance card: say so instead of leaving a hole.
            if (miniEl) miniEl.innerHTML = '<span class="muted">No events today</span>';
            nextEventLocation = '';
            navEl?.classList.add('hidden');
        }
    } catch (e) {
        // Backend genuinely failed (empty "no event" is handled above) — surface it.
        const miniEl = document.getElementById('next-event-mini');
        if (miniEl) {
            miniEl.classList.remove('hidden');
            miniEl.innerHTML = '<span class="muted">Next event unavailable</span>';
        }
        document.getElementById('nav-next-event')?.classList.add('hidden');
    }
}

function updateCalendarHeader() {
    const today = new Date();
    const [from, to] = calRange();
    const showsToday = calView === 'month'
        ? (calendarDate.getMonth() === today.getMonth() && calendarDate.getFullYear() === today.getFullYear())
        : (from <= today && today < new Date(to.getFullYear(), to.getMonth(), to.getDate() + 1));
    let label;
    if (calView === 'day') {
        label = calendarDate.toDateString() === today.toDateString() ? 'Today'
            : calendarDate.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' });
    } else if (calView === 'week') {
        const f = { day: 'numeric', month: 'short' };
        label = `${from.toLocaleDateString('fr-FR', f)} – ${to.toLocaleDateString('fr-FR', f)}`;
    } else {
        label = calendarDate.toLocaleDateString('fr-FR', { month: 'long', year: 'numeric' });
    }
    setText('cal-date', label);
    const todayBtn = document.getElementById('cal-today-btn');
    if (todayBtn) todayBtn.classList.toggle('hidden', showsToday);
}

// --- Weather page ---
async function fetchWeatherPage() {
    try {
        const resp = await fetch('/api/weather');
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        renderWeather(await resp.json());
    } catch (e) { console.warn('weather page fetch failed', e); }
}

function renderWeather(w) {
    const c = w.current || {};
    const icon = document.getElementById('wx-icon');
    if (icon) icon.className = `fas fa-${c.icon || 'cloud'} wx-icon`;
    setText('wx-temp', typeof c.temperature === 'number' ? fmtTemp(c.temperature) : '--');
    setText('wx-cond', c.condition || '--');
    setText('wx-feels', typeof c.feels_like === 'number' ? fmtTemp(c.feels_like) : '--');
    setText('wx-hum', typeof c.humidity === 'number' ? `${c.humidity}%` : '--');
    setText('wx-wind', typeof c.wind_speed_kmh === 'number' ? `${Math.round(c.wind_speed_kmh)} km/h` : '--');
    const today = (w.daily || [])[0];
    const hm = t => (t || '').slice(11, 16);
    setText('wx-sun', today ? `${hm(today.sunrise)} – ${hm(today.sunset)}` : '--');

    // Next 12 hours, built with DOM nodes (no HTML strings, no inline CSS).
    const hourly = document.getElementById('wx-hourly');
    if (hourly) {
        hourly.replaceChildren(...(w.hourly || []).slice(0, 12).map(h => {
            const col = document.createElement('div'); col.className = 'wx-hour';
            const t = document.createElement('span'); t.className = 'wx-hour-time'; t.textContent = hm(h.time);
            const i = document.createElement('i'); i.className = `fas fa-${h.icon || 'cloud'}`;
            const v = document.createElement('strong'); v.textContent = fmtTemp(h.temp);
            const r = document.createElement('span'); r.className = 'wx-rain';
            r.textContent = typeof h.precip_prob === 'number' && h.precip_prob > 0 ? `${h.precip_prob}%` : '';
            col.append(t, i, v, r);
            return col;
        }));
    }

    // 7 days; the min–max bar is a meter element scaled over the week's range.
    const daily = document.getElementById('wx-daily');
    if (daily) {
        const days = w.daily || [];
        const lo = Math.min(...days.map(d => d.min)), hi = Math.max(...days.map(d => d.max));
        daily.replaceChildren(...days.map((d, idx) => {
            const row = document.createElement('div'); row.className = 'wx-day';
            const name = document.createElement('span'); name.className = 'wx-day-name';
            name.textContent = idx === 0 ? 'Today'
                : new Date(d.date + 'T12:00').toLocaleDateString('fr-FR', { weekday: 'short' });
            const i = document.createElement('i'); i.className = `fas fa-${d.icon || 'cloud'}`;
            const rain = document.createElement('span'); rain.className = 'wx-rain';
            rain.textContent = typeof d.precip_prob === 'number' && d.precip_prob > 0 ? `${d.precip_prob}%` : '';
            const min = document.createElement('span'); min.className = 'wx-min'; min.textContent = fmtTemp(d.min);
            const bar = document.createElement('canvas'); bar.className = 'wx-range';
            requestAnimationFrame(() => drawTempRange(bar, d.min, d.max, lo, hi));
            const max = document.createElement('span'); max.className = 'wx-max'; max.textContent = fmtTemp(d.max);
            row.append(name, i, rain, min, bar, max);
            return row;
        }));
    }
}

/* One day's min–max as a rounded bar positioned on the week's range, cool to warm. */
function drawTempRange(canvas, min, max, lo, hi) {
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 160, h = canvas.clientHeight || 8;
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
    const ctx = canvas.getContext('2d'); ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const span = (hi - lo) || 1, r = h / 2;
    const x0 = ((min - lo) / span) * (w - h), x1 = ((max - lo) / span) * (w - h) + h;
    const css = getComputedStyle(document.documentElement);
    ctx.fillStyle = css.getPropertyValue('--border-strong').trim();
    ctx.beginPath(); ctx.roundRect(0, 0, w, h, r); ctx.fill();
    const g = ctx.createLinearGradient(0, 0, w, 0);
    g.addColorStop(0, css.getPropertyValue('--accent').trim());
    g.addColorStop(1, css.getPropertyValue('--warning').trim());
    ctx.fillStyle = g;
    ctx.beginPath(); ctx.roundRect(x0, 0, Math.max(h, x1 - x0), h, r); ctx.fill();
}

// --- Crypto ---
// Goes through the backend proxy (same as every other outbound call) rather than
// hitting CoinGecko/alternative.me from the kiosk page. A stale price is never
// left on screen: on failure the price itself is cleared, not just the change.
let cryptoCoins = {};          // key -> coin object from /api/crypto
let cryptoDetailKey = null;    // coin shown in the detail view, or null
let cryptoRangeDays = 7;
let cryptoSource = 'coingecko';  // 'kraken' when CoinGecko is blocked: no rank/cap/ATH/supply

async function fetchCrypto() {
    try {
        const resp = await fetch('/api/crypto');
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        const data = await resp.json();
        if (data.error && !data.coins) throw new Error(data.error);
        cryptoCoins = {};
        cryptoSource = data.source || 'coingecko';
        (data.coins || []).forEach(c => { cryptoCoins[c.key] = c; });
        ['btc', 'eth', 'zec', 'flux'].forEach(k => renderCryptoCard(k, cryptoCoins[k]));
        renderFearGreed(data.fng);
        if (cryptoDetailKey) renderCryptoDetailStats();
    } catch (e) {
        cryptoUnavailable();
    }
}

/* Prices span five orders of magnitude (BTC ~70k, FLUX ~0.06): whole euros above
   1000, cents above 1, four significant digits below. */
function fmtEur(v) {
    if (typeof v !== 'number' || !isFinite(v)) return '--';
    if (v >= 1000) return '€' + Math.round(v).toLocaleString('en-GB');
    if (v >= 1) return '€' + v.toLocaleString('en-GB', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    return '€' + v.toPrecision(4);
}
/* Money for fuel: always two decimals, euro sign after (as at the pump). */
function fmtMoney(v) {
    if (typeof v !== 'number' || !isFinite(v)) return '--';
    return `${v.toLocaleString('fr-FR', { minimumFractionDigits: 2, maximumFractionDigits: 2 })} €`;
}
function fmtEurCompact(v) {
    if (typeof v !== 'number' || !isFinite(v)) return '--';
    return '€' + new Intl.NumberFormat('en-GB', { notation: 'compact', maximumFractionDigits: 2 }).format(v);
}
function fmtPct(v) {
    return typeof v === 'number' && isFinite(v) ? `${v > 0 ? '+' : ''}${v.toFixed(1)}%` : '--';
}
/* Colour a change via classes only (no inline CSS). */
function setChange(id, v) {
    const el = document.getElementById(id);
    if (!el) return;
    el.textContent = fmtPct(v);
    el.classList.toggle('up', typeof v === 'number' && v > 0);
    el.classList.toggle('down', typeof v === 'number' && v < 0);
}

function renderCryptoCard(k, coin) {
    if (!coin || typeof coin.price !== 'number') {
        setText(`${k}-price`, '--');
        setText(`${k}-24h`, '--');
        return;
    }
    setText(`${k}-price`, fmtEur(coin.price));
    setChange(`${k}-24h`, coin.change_24h);
    drawSeriesChart(`${k}-spark`, (coin.sparkline_7d || []).map((p, i) => [i, p]), coin.change_7d, { spark: true });
}

function renderFearGreed(fng) {
    const el = document.getElementById('crypto-fng-value');
    if (!el) return;
    el.className = 'fng-value';
    if (!fng || fng.value == null) { el.textContent = 'unavailable'; return; }
    el.textContent = `${fng.value} — ${fng.classification ?? ''}`.trim();
    const v = Number(fng.value);
    el.classList.add(v < 25 ? 'fng-xfear' : v < 45 ? 'fng-fear' : v <= 55 ? 'fng-neutral' : v <= 75 ? 'fng-greed' : 'fng-xgreed');
}

function cryptoUnavailable() {
    ['btc', 'eth', 'zec', 'flux'].forEach(k => { setText(`${k}-price`, '--'); setText(`${k}-24h`, 'unavailable'); });
    setText('crypto-fng-value', 'unavailable');
}

// Detail view
function openCryptoDetail(k) {
    if (!cryptoCoins[k]) return;
    cryptoDetailKey = k;
    document.getElementById('crypto-list')?.classList.add('hidden');
    document.getElementById('crypto-detail')?.classList.remove('hidden');
    const badge = document.getElementById('cd-badge');
    const src = document.querySelector(`#crypto-card-${k} .coin-badge`);
    if (badge && src) { badge.className = src.className; badge.innerHTML = src.innerHTML; }
    renderCryptoDetailStats();
    setCryptoRange(cryptoRangeDays);
}

function closeCryptoDetail() {
    cryptoDetailKey = null;
    document.getElementById('crypto-detail')?.classList.add('hidden');
    document.getElementById('crypto-list')?.classList.remove('hidden');
}

/* Hide a stat row (or change chip) whose value the current source does not
   give (Kraken has no rank, market cap, ATH or supply) instead of showing "--". */
function showStat(id, present) {
    document.getElementById(id)?.closest('.metric-row, .cd-chip')?.classList.toggle('hidden', !present);
}
const known = v => typeof v === 'number' && isFinite(v);

function renderCryptoDetailStats() {
    const c = cryptoCoins[cryptoDetailKey];
    if (!c) return;
    setText('cd-name', `${c.name} · ${c.symbol}`);
    setText('cd-rank', c.rank ? `Rank #${c.rank}` : (cryptoSource === 'kraken' ? 'Prices from Kraken' : ''));
    showStat('cd-c1h', known(c.change_1h));
    showStat('cd-c24h', known(c.change_24h));
    showStat('cd-c7d', known(c.change_7d));
    showStat('cd-c30d', known(c.change_30d));
    showStat('cd-mcap', known(c.market_cap));
    showStat('cd-vol', known(c.volume_24h));
    showStat('cd-range', known(c.low_24h) && known(c.high_24h));
    showStat('cd-ath', known(c.ath));
    showStat('cd-athchg', known(c.ath_change));
    showStat('cd-supply', known(c.circulating_supply));
    setText('cd-price', fmtEur(c.price));
    setChange('cd-change', c.change_24h);
    setChange('cd-c1h', c.change_1h);
    setChange('cd-c24h', c.change_24h);
    setChange('cd-c7d', c.change_7d);
    setChange('cd-c30d', c.change_30d);
    setText('cd-mcap', fmtEurCompact(c.market_cap));
    setText('cd-vol', fmtEurCompact(c.volume_24h));
    setText('cd-range', `${fmtEur(c.low_24h)} – ${fmtEur(c.high_24h)}`);
    const athDate = c.ath_date ? new Date(c.ath_date).toLocaleDateString('fr-FR', { day: 'numeric', month: 'short', year: 'numeric' }) : '';
    setText('cd-ath', `${fmtEur(c.ath)}${athDate ? ' · ' + athDate : ''}`);
    setChange('cd-athchg', c.ath_change);
    const sup = typeof c.circulating_supply === 'number'
        ? new Intl.NumberFormat('en-GB', { notation: 'compact', maximumFractionDigits: 2 }).format(c.circulating_supply) : '--';
    const max = typeof c.max_supply === 'number'
        ? ' / ' + new Intl.NumberFormat('en-GB', { notation: 'compact', maximumFractionDigits: 2 }).format(c.max_supply) : '';
    setText('cd-supply', `${sup}${max} ${c.symbol}`);
}

async function setCryptoRange(days) {
    cryptoRangeDays = days;
    document.querySelectorAll('.cd-ranges .media-tab').forEach(b =>
        b.classList.toggle('active', Number(b.dataset.days) === days));
    const c = cryptoCoins[cryptoDetailKey];
    const msg = document.getElementById('cd-chart-msg');
    if (!c) return;
    msg?.classList.add('hidden');
    try {
        const resp = await fetch(`/api/crypto/history?id=${encodeURIComponent(c.id)}&days=${days}`);
        const data = await resp.json();
        if (!resp.ok || !data.prices) throw new Error(data.error || `HTTP ${resp.status}`);
        if (cryptoDetailKey !== c.key || cryptoRangeDays !== days) return; // user moved on
        const pts = data.prices;
        const first = pts[0]?.[1], last = pts[pts.length - 1]?.[1];
        const chg = first ? (last - first) / first * 100 : 0;
        drawSeriesChart('cd-chart', pts, chg, { days });
    } catch (e) {
        if (msg) { msg.textContent = 'History unavailable'; msg.classList.remove('hidden'); }
    }
}

/* Line + soft fill, green when the range went up, red when down. `pts` are
   [x, price] pairs; x is unix ms for history, an index for sparklines. */
function cssToken(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/* Line + glow + fade-fill chart of [x, y] points, sized to the canvas' CSS box at
   device resolution (a fixed-size canvas stretched by CSS draws a blurry line).
   opts: spark (no labels), color (default: --success/--danger by `change` sign),
   fmtValue (min/max labels), fmtTime (x labels; null hides them), days (price axis). */
function drawSeriesChart(canvasId, pts, change, opts = {}) {
    const canvas = document.getElementById(canvasId);
    if (!canvas || !pts.length) return;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || canvas.width, h = canvas.clientHeight || canvas.height;
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
    const ctx = canvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    const fmtTime = opts.fmtTime === undefined
        ? (d => opts.days === 1 ? d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' })
            : d.toLocaleDateString('fr-FR', { day: 'numeric', month: 'short' }))
        : opts.fmtTime;
    const ys = pts.map(p => p[1]);
    const min = Math.min(...ys), max = Math.max(...ys), span = (max - min) || 1;
    const padT = opts.spark ? 4 : 18, padB = opts.spark ? 4 : (fmtTime ? 22 : 8), padL = opts.spark ? 0 : 4, padR = opts.spark ? 0 : 4;
    const x = i => padL + (i / (pts.length - 1 || 1)) * (w - padL - padR);
    const y = v => padT + (1 - (v - min) / span) * (h - padT - padB);
    const stroke = opts.color || cssToken(typeof change === 'number' && change < 0 ? '--danger' : '--success');
    ctx.beginPath();
    pts.forEach((p, i) => (i ? ctx.lineTo(x(i), y(p[1])) : ctx.moveTo(x(i), y(p[1]))));
    ctx.strokeStyle = stroke; ctx.lineWidth = opts.spark ? 1.6 : 2.2; ctx.lineJoin = 'round';
    ctx.shadowColor = stroke; ctx.shadowBlur = opts.spark ? 4 : 10;
    ctx.stroke();
    ctx.shadowBlur = 0;
    ctx.lineTo(x(pts.length - 1), h - padB); ctx.lineTo(x(0), h - padB); ctx.closePath();
    const g = ctx.createLinearGradient(0, padT, 0, h - padB);
    g.addColorStop(0, stroke); g.addColorStop(1, 'transparent');
    ctx.globalAlpha = 0.22; ctx.fillStyle = g; ctx.fill(); ctx.globalAlpha = 1;
    if (opts.spark) return;
    // Max / min labels and the time span.
    const fmtValue = opts.fmtValue || fmtEur;
    ctx.font = '600 12px Manrope, sans-serif'; ctx.fillStyle = cssToken('--text-secondary');
    ctx.textAlign = 'right';
    ctx.fillText(fmtValue(max), w - 6, 13);
    ctx.fillText(fmtValue(min), w - 6, h - padB - 6);
    if (!fmtTime) return;
    ctx.textAlign = 'left'; ctx.fillText(fmtTime(new Date(pts[0][0])), 6, h - 5);
    ctx.textAlign = 'right'; ctx.fillText(fmtTime(new Date(pts[pts.length - 1][0])), w - 6, h - 5);
}

// --- Trip History ---
let tripDate = new Date();
let selectedTripId = null;
let selectedTripMetric = 'speed';
let tripMap = null;
let tripPolyline = null;

/* Local YYYY-MM-DD: the backend buckets trips by LOCAL day, and toISOString()
   is the UTC date, which is the previous day between 00:00 and 02:00 in CEST. */
function formatTripDate(d) {
    const pad = n => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function tripDateLabel(d) {
    const today = new Date(), yesterday = new Date();
    yesterday.setDate(today.getDate() - 1);
    if (d.toDateString() === today.toDateString()) return 'Today';
    if (d.toDateString() === yesterday.toDateString()) return 'Yesterday';
    return d.toLocaleDateString('fr-FR', { weekday: 'short', day: 'numeric', month: 'short' });
}

const fmtClock = ts => new Date(ts * 1000).toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });

// Trip length in minutes. Named apart from the playback fmtDuration (m:ss)
// above: two declarations with one name made this one win everywhere, and the
// player showed "1 min" instead of 1:02.
function fmtTripDuration(seconds) {
    const m = Math.max(1, Math.round(seconds / 60));
    return m >= 60 ? `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, '0')}` : `${m} min`;
}

function tripShiftDay(delta) {
    tripDate.setDate(tripDate.getDate() + delta);
    setText('trip-date', tripDateLabel(tripDate));
    selectedTripId = null;
    setTripDetailVisible(false);
    fetchTrips();
}
function tripGoToDate(d) {
    tripDate = new Date(d);
    tripShiftDay(0);
}

function openTripDatePicker() {
    openDatePicker({
        selected: tripDate,
        noun: 'trip',
        onPick: tripGoToDate,
        fetchCounts: async (y, m) => {
            const res = await fetch(`/api/trips/calendar?month=${y}-${String(m + 1).padStart(2, '0')}`);
            return (await res.json()).days || {};
        },
    });
}

function tripPrevDay() { tripShiftDay(-1); }
function tripNextDay() { tripShiftDay(1); }

async function fetchTrips() {
    try {
        const res = await fetch(`/api/trips?date=${formatTripDate(tripDate)}`);
        const trips = await res.json();
        const list = document.getElementById('trip-list');
        list.replaceChildren();
        if (!Array.isArray(trips) || trips.length === 0) {
            const empty = document.createElement('div');
            empty.className = 'page-empty';
            const icon = document.createElement('i'); icon.className = 'fas fa-route';
            const p = document.createElement('p'); p.textContent = 'No trips this day';
            empty.append(icon, p);
            list.append(empty);
            selectedTripId = null;
            setTripDetailVisible(false);
            return;
        }
        // Newest first: the drive that just ended is the one being looked for.
        const ordered = [...trips].sort((a, b) => b.start_ts - a.start_ts);
        for (const t of ordered) {
            const row = document.createElement('button');
            row.className = 'trip-item';
            row.dataset.id = t.id;
            row.classList.toggle('active', t.id === selectedTripId);
            const when = document.createElement('span');
            when.className = 'trip-when';
            const range = document.createElement('strong');
            range.textContent = `${fmtClock(t.start_ts)} → ${fmtClock(t.end_ts)}`;
            const dur = document.createElement('small');
            dur.className = 'muted';
            dur.textContent = fmtTripDuration(t.end_ts - t.start_ts);
            when.append(range, dur);
            const km = document.createElement('span');
            km.className = 'trip-km';
            km.textContent = `${t.distance_km.toFixed(1)} km`;
            // Estimated cost under the distance (trips recorded before 2026-10-09 have none).
            if (t.cost_eur > 0) {
                const cost = document.createElement('small');
                cost.textContent = fmtMoney(t.cost_eur);
                km.append(cost);
            }
            row.append(when, km);
            row.onclick = () => selectTrip(t.id);
            list.append(row);
        }
        // Open the latest drive rather than leaving half the screen empty.
        if (!ordered.some(t => t.id === selectedTripId)) selectTrip(ordered[0].id);
    } catch (e) { console.warn('fetchTrips failed', e); }
}

let selectedTrip = null;

async function selectTrip(id) {
    selectedTripId = id;
    document.querySelectorAll('.trip-item').forEach(el =>
        el.classList.toggle('active', Number(el.dataset.id) === id));
    try {
        const res = await fetch(`/api/trips/${id}`);
        const trip = await res.json();
        // The endpoint answers {"error": "..."} for an unknown id — that is not a trip.
        if (!isTrip(trip)) { tripDetailUnavailable(); return; }
        selectedTrip = trip;
        setTripDetailVisible(true);
        renderTripMap(trip);
        renderTripChart(trip, selectedTripMetric);
        renderTripStats(trip);
    } catch (e) {
        console.warn('selectTrip failed', e);
        tripDetailUnavailable();
    }
}

function isTrip(trip) {
    return !!trip && !trip.error && Array.isArray(trip.points);
}

function tripDetailUnavailable() {
    chartUnavailable('trip-metric-chart', 'Trip unavailable');
    const statsEl = document.getElementById('trip-stats');
    if (statsEl) statsEl.textContent = '';
}

function renderTripMap(trip) {
    if (!window.L || !isTrip(trip)) return;
    if (!tripMap) {
        tripMap = L.map('trip-map', { zoomControl: false, attributionControl: true });
        L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
            attribution: '© OSM',
        }).addTo(tripMap);
        // The panel can still change width after the one-frame re-measure below
        // (stats and chart render into it): Leaflet then kept the old width and
        // left a grey strip on the right. Re-measure whenever the box changes.
        if (window.ResizeObserver) {
            new ResizeObserver(() => tripMap.invalidateSize()).observe(document.getElementById('trip-map'));
        }
    }
    if (tripPolyline) tripMap.removeLayer(tripPolyline);
    const coords = trip.points.map(p => [p.lat, p.lon]).filter(([la, lo]) => la !== 0 || lo !== 0);
    if (coords.length > 1) {
        tripPolyline = L.featureGroup([
            L.polyline(coords, { color: cssToken('--accent'), weight: 5, opacity: 0.9 }),
            L.circleMarker(coords[0], { radius: 7, color: cssToken('--success'), fillOpacity: 1, weight: 3 }),
            L.circleMarker(coords[coords.length - 1], { radius: 7, color: cssToken('--danger'), fillOpacity: 1, weight: 3 }),
        ]).addTo(tripMap);
        // The panel was hidden until this call: wait one frame for layout, then let
        // Leaflet re-measure, or fitBounds uses a stale size and crops the route.
        const bounds = tripPolyline.getBounds();
        requestAnimationFrame(() => {
            tripMap.invalidateSize();
            tripMap.fitBounds(bounds, { padding: [28, 28] });
        });
    }
}

const TRIP_METRICS = {
    speed:   { unit: 'km/h', digits: 0 },
    alt:     { unit: 'm',    digits: 0 },
    rpm:     { unit: 'rpm',  digits: 0 },
    coolant: { unit: '°C',   digits: 0 },
    load:    { unit: '%',    digits: 0 },
};

function renderTripChart(trip, metric) {
    if (!isTrip(trip)) return;
    const m = TRIP_METRICS[metric] || { unit: '', digits: 1 };
    const pts = trip.points.map(p => [p.ts * 1000, p[metric] ?? 0]);
    // No OBD adapter (or no GPS altitude) records zeros: say so instead of a flat line.
    if (pts.every(p => !p[1])) { chartUnavailable('trip-metric-chart', `No ${metric} data for this trip`); return; }
    drawSeriesChart('trip-metric-chart', pts, 0, {
        color: cssToken('--accent'),
        fmtValue: v => `${v.toFixed(m.digits)} ${m.unit}`,
        fmtTime: d => d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' }),
    });
}

function renderTripStats(trip) {
    if (!isTrip(trip)) return;
    const speeds = trip.points.map(p => p.speed);
    const maxSpeed = Math.max(...speeds, 0).toFixed(0);
    const hours = (trip.end_ts - trip.start_ts) / 3600;
    // Distance over time, not the mean of samples: stops would otherwise be under-weighted.
    const avgSpeed = hours > 0 ? (trip.distance_km / hours).toFixed(0) : '0';
    const l100 = trip.fuel_l > 0 && trip.distance_km >= 1 ? trip.fuel_l / trip.distance_km * 100 : null;
    const cards = [
        ['Distance', `${trip.distance_km.toFixed(1)} km`],
        ['Duration', fmtTripDuration(trip.end_ts - trip.start_ts)],
        ['Max speed', `${maxSpeed} km/h`],
        ['Avg speed', `${avgSpeed} km/h`],
        ['Fuel', trip.fuel_l > 0 ? `${trip.fuel_l.toFixed(1)} L` : '--', l100 != null ? `${l100.toFixed(1)} L/100` : ''],
        ['Cost', trip.cost_eur > 0 ? fmtMoney(trip.cost_eur) : '--'],
    ];
    const statsEl = document.getElementById('trip-stats');
    statsEl.replaceChildren(...cards.map(([label, value, sub]) => {
        const card = document.createElement('div');
        card.className = 'trip-stat-card';
        const l = document.createElement('span'); l.textContent = label;
        const v = document.createElement('strong'); v.textContent = value;
        card.append(l, v);
        if (sub) { const x = document.createElement('small'); x.className = 'muted'; x.textContent = sub; card.append(x); }
        return card;
    }));
}

/* The metric buttons, chart and stats are meaningless with no trip selected —
   they used to sit in the middle of an empty screen looking like a toolbar. */
function setTripDetailVisible(visible) {
    const body = document.getElementById('trip-detail-body');
    const empty = document.getElementById('trip-detail-empty');
    if (body) body.classList.toggle('hidden', !visible);
    if (empty) empty.classList.toggle('hidden', visible);
}

function selectTripMetric(btn, metric) {
    selectedTripMetric = metric;
    document.querySelectorAll('.trip-metric-btn').forEach(b => b.classList.toggle('active', b === btn));
    // The selected trip is already loaded: no need to refetch it per metric.
    if (selectedTrip && selectedTrip.id === selectedTripId) renderTripChart(selectedTrip, metric);
}

// --- Month date picker (shared by Trips and Calendar) ---
/* A popup month grid. The caller supplies the selected day, a `fetchCounts(year,
   monthIndex)` returning {"YYYY-MM-DD": n} for that month, and `onPick(date)`. */
const datePicker = { month: null, selected: null, fetchCounts: null, onPick: null, noun: '', counts: {} };

function openDatePicker({ selected, fetchCounts, onPick, noun }) {
    const sel = new Date(selected);
    Object.assign(datePicker, {
        selected: sel, fetchCounts, onPick, noun: noun || 'item',
        month: new Date(sel.getFullYear(), sel.getMonth(), 1), counts: {},
    });
    document.getElementById('date-picker-overlay').classList.remove('hidden');
    renderDatePicker();
    loadDatePickerCounts();
    resetInteraction();
}

function closeDatePicker() {
    document.getElementById('date-picker-overlay').classList.add('hidden');
}

function datePickerShift(delta) {
    const m = datePicker.month;
    datePicker.month = new Date(m.getFullYear(), m.getMonth() + delta, 1);
    datePicker.counts = {};
    renderDatePicker();
    loadDatePickerCounts();
}

async function loadDatePickerCounts() {
    const month = datePicker.month;
    try {
        const counts = await datePicker.fetchCounts(month.getFullYear(), month.getMonth());
        if (datePicker.month !== month) return;      // the user moved on meanwhile
        datePicker.counts = counts || {};
        renderDatePicker();
    } catch (e) {
        console.warn('date picker counts failed', e);
    }
}

function renderDatePicker() {
    const { month, selected, counts, noun } = datePicker;
    setText('dp-title', month.toLocaleDateString('fr-FR', { month: 'long', year: 'numeric' }));
    const grid = document.getElementById('dp-days');
    grid.replaceChildren();
    // Monday-first: getDay() is 0 for Sunday.
    const lead = (month.getDay() + 6) % 7;
    for (let i = 0; i < lead; i++) grid.appendChild(document.createElement('span'));
    const days = new Date(month.getFullYear(), month.getMonth() + 1, 0).getDate();
    const today = new Date().toDateString();
    for (let d = 1; d <= days; d++) {
        const date = new Date(month.getFullYear(), month.getMonth(), d);
        const n = counts[formatTripDate(date)] || 0;
        const b = document.createElement('button');
        b.className = 'media-tab dp-day';
        b.classList.toggle('active', date.toDateString() === selected.toDateString());
        b.classList.toggle('today', date.toDateString() === today);
        b.classList.toggle('has-items', n > 0);
        const num = document.createElement('strong');
        num.textContent = d;
        const cnt = document.createElement('small');
        cnt.className = 'dp-count';
        cnt.textContent = n ? String(n) : '';
        b.append(num, cnt);
        b.setAttribute('aria-label', `${date.toLocaleDateString('fr-FR')}: ${n} ${noun}${n === 1 ? '' : 's'}`);
        b.addEventListener('click', () => { closeDatePicker(); datePicker.onPick(date); });
        grid.appendChild(b);
    }
}

// --- Media Controls ---
function mediaCmd(action) {
    // If radio is the active source, control it directly
    const radioAudio = document.getElementById('radio-player');
    // Only hijack play-pause for the radio when the radio is ACTUALLY playing
    // (mirror the next/previous guards); otherwise it's real bridge media.
    if (action === 'play-pause' && radioAudio && !radioAudio.paused) {
        radioPlayPause();
        resetInteraction();
        return;
    }
    if (action === 'next' && radioStations.length > 0 && radioAudio && !radioAudio.paused) {
        radioNext();
        resetInteraction();
        return;
    }
    if (action === 'previous' && radioStations.length > 0 && radioAudio && !radioAudio.paused) {
        radioPrev();
        resetInteraction();
        return;
    }
    if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'media', action }));
    }
    resetInteraction();
}

// --- Car media keys (steering wheel / head unit, relayed by rascar-carplay) ---
// The car sends Play, Pause, Next and Previous as separate keys. They act on the
// source that is playing; with nothing playing, on the last one used, so Play
// after a pause resumes what was paused.
function activeMediaSource() {
    const radio = document.getElementById('radio-player');
    if (radio && !radio.paused) return 'radio';
    const video = document.getElementById('tv-player');
    if (tvIsPlaying() && video && !video.paused) return 'tv';
    if (state?.media?.is_playing && !(state.media.player_name || '').startsWith('chromium')) return 'mpris';
    return lastMediaSource || 'mpris';
}

/* Step within the visible (filtered) channels, like radioStep. */
function tvStep(delta) {
    const visible = tvVisibleIndices();
    if (!visible.length) return;
    const at = visible.indexOf(tvIdx);
    const next = at === -1 ? 0 : (at + delta + visible.length) % visible.length;
    playTvChannelByIdx(visible[next]);
}

function carKey(key) {
    // The wheel's phone keys: Hook (answer / hang up), Drop (decline / hang up),
    // and "hangup" from an older rascar-carplay that merged the two. A modem
    // call is the backend's: it acts on the same key and the page follows the
    // WS state (js/phone.js). Without one, both keys end an open meeting.
    if (key === 'hook' || key === 'drop' || key === 'hangup') {
        if (RasCar.phone?.hasCall()) {
            console.log(`car key ${key} → phone call (handled by the backend)`);
        } else if (embedded && MEETING_APPS.includes(embedded.id)) {
            console.log(`car key hangup → closing ${embedded.id}`);
            closeEmbeddedApp();
        } else console.log(`car key ${key} → no call or meeting, ignored`);
        return;
    }
    const source = activeMediaSource();
    console.log(`car key ${key} → ${source}`);
    if (source === 'radio') {
        const playing = !document.getElementById('radio-player')?.paused;
        if (key === 'next') radioNext();
        else if (key === 'previous') radioPrev();
        else if (key === 'play-pause' || (key === 'play' && !playing) || ((key === 'pause' || key === 'stop') && playing)) radioPlayPause();
        return;
    }
    if (source === 'tv') {
        const video = document.getElementById('tv-player');
        if (key === 'next') tvStep(1);
        else if (key === 'previous') tvStep(-1);
        else if (video && (key === 'play' || (key === 'play-pause' && video.paused))) video.play().catch(() => {});
        else if (video) video.pause();   // pause, stop, play-pause while playing
        return;
    }
    if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: 'media', action: key }));
    }
}

// --- Navigation: full Google Maps / Waze in a frame ---
// The frame-unlock extension in the car browser lifts their X-Frame-Options/CSP;
// the backend's dashboard session gives them the car's GPS position (the Pi has
// no location provider) and types the on-screen keyboard into their search box.
let navSource = 'google';
try { const s = localStorage.getItem('rascar-nav-source'); if (['google', 'waze', 'offline'].includes(s)) navSource = s; } catch (e) { /* storage unavailable */ }
let navDest = '';
let nextEventLocation = '';

function navMapUrl() {
    const g = window.RasCar?.state?.gps;
    const here = g && g.fix !== false && !g.mock && (g.lat || g.lon) ? g : null;
    if (navSource === 'waze') {
        return navDest
            ? `https://www.waze.com/ul?q=${encodeURIComponent(navDest)}&navigate=yes`
            : 'https://www.waze.com/live-map';
    }
    if (navDest) {
        return `https://www.google.com/maps/dir/?api=1&destination=${encodeURIComponent(navDest)}&travelmode=driving&hl=fr`;
    }
    return here ? `https://www.google.com/maps/@${here.lat.toFixed(5)},${here.lon.toFixed(5)},15z?hl=fr`
                : 'https://www.google.com/maps?hl=fr';
}

/* Loads the map once, then only when the source or destination changes, so
   coming back to the page keeps whatever was being looked at. */
let navLoadedUrl = '';
function renderNavMap(force) {
    const frame = document.getElementById('nav-map-frame');
    if (!frame) return;
    const offline = navSource === 'offline';
    frame.classList.toggle('hidden', offline);
    document.querySelectorAll('.nav-web-only').forEach(b => b.classList.toggle('hidden', offline));
    document.querySelectorAll('.nav-offline-only').forEach(b => b.classList.toggle('hidden', !offline));
    document.querySelectorAll('.nav-source .media-tab')
        .forEach(b => b.classList.toggle('active', b.dataset.source === navSource));
    if (offline) { RasCar.navmap?.show(); return; }
    RasCar.navmap?.hide();
    if (navLoadedUrl && !force) return;
    navLoadedUrl = navMapUrl();
    frame.src = navLoadedUrl;
}

function setNavSource(source) {
    navSource = source;
    try { localStorage.setItem('rascar-nav-source', source); } catch (e) { /* not remembered */ }
    renderNavMap(true);
}

// Route to a destination (calendar event, next-event card) inside the map.
function navigateTo(dest) {
    if (typeof dest !== 'string' || !dest.trim()) return;
    navDest = dest.trim();
    goToPage(3);
    renderNavMap(true);
}

function routeToNextEvent() {
    if (nextEventLocation) navigateTo(nextEventLocation);
}

function toggleNavKeyboard() {
    document.getElementById('nav-osk')?.classList.toggle('hidden');
}

// --- On-screen keyboard (Chromium on Linux has none) ---
const OSK_ROWS = ['1234567890', 'azertyuiop', 'qsdfghjklm', "wxcvbn,.'-"];
const OSK_SYMBOLS = '!@#$%&*?_/';

/* Builds an AZERTY keyboard into `el`. With `keepFocus`, pressing a key never
   takes focus away from the field being typed into (e.g. inside the Maps
   frame): the default action of pointerdown/mousedown is what moves focus. */
function buildKeyboard(el, { onKey, onBackspace, onGo, goLabel, goIcon, keepFocus, password }) {
    if (!el) return;
    let upper = false;
    const key = (label, cls, handler) => {
        const b = document.createElement('button');
        if (cls) b.className = cls;
        if (label.startsWith('fa-')) { const i = document.createElement('i'); i.className = `fas ${label}`; b.append(i); }
        else b.textContent = label;
        b.addEventListener('click', handler);
        return b;
    };
    // With `password`, a shift key and a symbols row: Wi-Fi keys need both.
    const letters = [];
    const typed = c => { onKey(upper ? c.toUpperCase() : c); };
    const rowsDef = password ? [...OSK_ROWS, OSK_SYMBOLS] : OSK_ROWS;
    const rows = rowsDef.map((chars, r) => {
        const row = document.createElement('div');
        row.className = 'osk-row';
        for (const c of chars) {
            const b = key(c, '', () => typed(c));
            if (c.toUpperCase() !== c) letters.push(b);
            row.append(b);
        }
        if (r === OSK_ROWS.length - 1) {
            const back = key('fa-delete-left', 'osk-wide', onBackspace);
            back.setAttribute('aria-label', 'Delete');
            row.append(back);
        }
        return row;
    });
    const last = document.createElement('div');
    last.className = 'osk-row';
    if (password) {
        const shift = key('fa-arrow-up', 'osk-wide', () => {
            upper = !upper;
            shift.classList.toggle('active', upper);
            letters.forEach(b => { b.textContent = upper ? b.textContent.toUpperCase() : b.textContent.toLowerCase(); });
        });
        shift.setAttribute('aria-label', 'Shift');
        last.append(shift);
    }
    last.append(key('space', 'osk-space', () => onKey(' ')));
    const go = key(goIcon || 'fa-magnifying-glass', 'osk-go', onGo);
    go.append(` ${goLabel || 'Search'}`);
    last.append(go);
    el.replaceChildren(...rows, last);
    if (keepFocus) {
        for (const type of ['pointerdown', 'mousedown']) el.addEventListener(type, e => e.preventDefault());
    }
}

function sendInput(body) {
    fetch('/api/input', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
        .then(r => { if (!r.ok) notify('Keyboard unavailable — car browser not reachable'); })
        .catch(() => notify('Keyboard unavailable'));
}

function initKeyboards() {
    buildKeyboard(document.getElementById('ytm-osk'), {
        onKey: ytmKey, onBackspace: ytmBackspace, onGo: ytmSearch, goLabel: 'Search',
    });
    // Types into whatever is focused in the map frame (tap its search box first).
    buildKeyboard(document.getElementById('nav-osk'), {
        onKey: c => sendInput({ text: c }),
        onBackspace: () => sendInput({ key: 'Backspace' }),
        onGo: () => sendInput({ key: 'Enter' }),
        goLabel: 'Go', goIcon: 'fa-arrow-right', keepFocus: true,
    });
    buildKeyboard(document.getElementById('app-osk'), {
        onKey: c => sendInput({ text: c }),
        onBackspace: () => sendInput({ key: 'Backspace' }),
        onGo: () => sendInput({ key: 'Enter' }),
        goLabel: 'Go', goIcon: 'fa-arrow-right', keepFocus: true,
    });
    // Opening the keyboard must not blur the field that was just tapped either.
    document.querySelectorAll('.nav-kbd-btn').forEach(btn => {
        for (const type of ['pointerdown', 'mousedown']) btn.addEventListener(type, e => e.preventDefault());
    });
}

// --- Settings ---
// --- Nav Popup ---
// --- Media Tabs ---

// --- Visualizer (delegated to media.js module) ---
// connectVisualizerToAudio is now provided by media.js as window.connectVisualizerToAudio
function startVisualizer() {
    // media.js handles all visualization via RasCar.media.init()
    if (RasCar.media) RasCar.media.init();
}

// --- Destinations (Events) Tab ---

// --- Favourites (radio stations / TV channels) ---
// Stored by name in settings.json (radio_favorites / tv_favorites). The filter
// chip "★ Favourites" is the FAV value of the country filter; the lists open on
// it when there are favourites.
const FAV = '\u2605';
const favs = kind => new Set((currentSettings && currentSettings[`${kind}_favorites`]) || []);

function toggleFavorite(kind, name) {
    const set = favs(kind);
    if (set.has(name)) set.delete(name); else set.add(name);
    const list = [...set];
    currentSettings = { ...currentSettings, [`${kind}_favorites`]: list };
    saveSettingsPatch({ [`${kind}_favorites`]: list });
    if (kind === 'tv') { renderCountryTabs('tv-country-tabs', tvChannels, tvCountry, setTvCountry, 'tv'); renderTvChannels(); }
    else { renderRadioCountryTabs(); renderRadioStations(); }
}

// A star toggle for a list row; the tap must not also play the row.
function favButton(kind, name) {
    const b = document.createElement('button');
    const on = favs(kind).has(name);
    b.className = 'ytm-clear fav-btn' + (on ? ' on' : '');
    b.setAttribute('aria-label', on ? 'Remove from favourites' : 'Add to favourites');
    const i = document.createElement('i');
    i.className = on ? 'fas fa-star' : 'far fa-star';
    b.appendChild(i);
    b.addEventListener('click', e => { e.stopPropagation(); toggleFavorite(kind, name); });
    return b;
}

// --- TV Streaming ---
let hlsInstance = null;
let tvChannels = [];

let tvCountry = null;   // null = All

function tvVisibleIndices() {
    if (!Array.isArray(tvChannels)) return [];
    return tvChannels
        .map((c, i) => [c, i])
        .filter(([c]) => !tvCountry || (tvCountry === FAV ? favs('tv').has(c.name) : (c.country || '') === tvCountry))
        .map(([, i]) => i);
}

function setTvCountry(country) {
    tvCountry = country;
    renderCountryTabs('tv-country-tabs', tvChannels, tvCountry, setTvCountry, 'tv');
    renderTvChannels();
}

/* 55 channels across 7 countries is too long to scan as a flat list. When no
   country filter is active the rows are grouped under sticky country headings;
   with a filter the heading would be noise, so it is dropped. */
function renderTvChannels() {
    const list = document.getElementById('tv-channel-list');
    if (!list) return;
    if (!Array.isArray(tvChannels) || tvChannels.length === 0) {
        list.innerHTML = '<p class="list-empty list-empty-left">No channels configured</p>';
        return;
    }
    const visible = tvVisibleIndices();
    list.textContent = '';
    if (visible.length === 0) {
        list.innerHTML = tvCountry === FAV
            ? '<p class="list-empty list-empty-left">No favourites yet — tap ☆ on a channel</p>'
            : '<p class="list-empty list-empty-left">No channels for this country</p>';
        return;
    }
    const grouped = !tvCountry;   // favourites and a country filter: flat
    let lastCountry = null;
    for (const i of visible) {
        const c = tvChannels[i];
        const country = c.country || 'Other';
        if (grouped && country !== lastCountry) {
            const h = document.createElement('div');
            h.className = 'list-group-header';
            h.textContent = country;      // from a config file — text, never markup
            list.appendChild(h);
            lastCountry = country;
        }
        const item = document.createElement('div');
        item.className = 'tv-channel-item fav-row' + (i === tvIdx ? ' active' : '');
        const label = document.createElement('span');
        label.textContent = c.name;       // untrusted name as text, never markup
        item.append(label, favButton('tv', c.name));
        item.dataset.idx = i;             // ORIGINAL index — filtering must not renumber
        item.addEventListener('click', () => playTvChannelByIdx(i));
        list.appendChild(item);
    }
}

async function fetchTvChannels() {
    try {
        const res = await fetch('/api/tv/channels');
        const channels = await res.json();
        if (!Array.isArray(channels) || channels.length === 0) {
            const list = document.getElementById('tv-channel-list');
            if (list) list.innerHTML = '<p class="list-empty list-empty-left">No channels configured</p>';
            return;
        }
        // Group the flat config by country so the sticky headings appear once
        // each, without requiring the JSON to be pre-sorted.
        tvChannels = [...channels].sort((a, b) => {
            const ca = a.country || 'Other', cb = b.country || 'Other';
            return ca === cb ? 0 : ca.localeCompare(cb);
        });
        if (tvCountry === null && favs('tv').size) tvCountry = FAV;
        renderCountryTabs('tv-country-tabs', tvChannels, tvCountry, setTvCountry, 'tv');
        renderTvChannels();
        // No autoplay: a channel starts only when tapped. Autoplaying the first one
        // decoded a live stream in the background from boot (metered uplink, CPU on
        // car power) and, with picture-in-picture, floated TV over every screen.
        if (tvIdx < 0) setText('tv-now-playing', 'Select a channel');
    } catch (e) { console.warn('fetchTvChannels failed', e); }
}

let tvIdx = -1;

function playTvChannelByIdx(idx, opts) {
    const c = tvChannels[idx];
    if (!c) return;
    tvIdx = idx;
    lastMediaSource = 'tv';
    if (!opts?.resumed) window.reportMediaSession?.('play', 'tv', c);
    document.getElementById('tv-now-playing').textContent = c.name;
    document.getElementById('tv-stop')?.classList.remove('hidden');
    // dataset.idx is the ORIGINAL index, so this stays correct under filtering.
    document.querySelectorAll('.tv-channel-item').forEach(el =>
        el.classList.toggle('active', parseInt(el.dataset.idx, 10) === idx)
    );
    // Through the backend HLS proxy: the TV CDNs send no CORS headers, so the
    // page cannot fetch them directly (see api_tv_hls).
    const url = '/api/tv/hls?url=' + encodeURIComponent(c.url);
    const video = document.getElementById('tv-player');
    if (!video) return;
    if (hlsInstance) { hlsInstance.destroy(); hlsInstance = null; }
    if (window.Hls && Hls.isSupported()) {
        hlsInstance = new Hls();
        hlsInstance.loadSource(url);
        hlsInstance.attachMedia(video);
        hlsInstance.on(Hls.Events.MANIFEST_PARSED, () => video.play());
    } else if (video.canPlayType('application/vnd.apple.mpegurl')) {
        video.src = url;
        video.play();
    }
}

// --- TV picture-in-picture ---
const TV_PAGE = 7;

function tvIsPlaying() {
    const video = document.getElementById('tv-player');
    return tvIdx >= 0 && !!video && !!(video.currentSrc || video.src);
}

/* Move the <video> into the floating window when TV plays off its page, and back
   when the TV page is shown again. Moving a node within the same task does not
   pause a playing media element, so the stream just continues. */
function updateTvPip() {
    const pip = document.getElementById('tv-pip');
    const slot = document.getElementById('tv-pip-slot');
    const video = document.getElementById('tv-player');
    const area = document.getElementById('tv-player-area');
    if (!pip || !slot || !video || !area) return;
    const wantPip = tvIsPlaying() && currentPage !== TV_PAGE;
    if (wantPip) {
        if (video.parentElement !== slot) slot.appendChild(video);
        setText('tv-pip-name', tvChannels[tvIdx]?.name || '');
        pip.classList.remove('hidden');
    } else {
        if (video.parentElement !== area) area.insertBefore(video, area.firstChild);
        pip.classList.add('hidden');
    }
    if (!video.paused || wantPip) video.play().catch(() => {});
}

function tvStop() {
    if (tvIdx >= 0) window.reportMediaSession?.('stop', 'tv');
    if (hlsInstance) { hlsInstance.destroy(); hlsInstance = null; }
    const video = document.getElementById('tv-player');
    if (video) { video.pause(); video.removeAttribute('src'); video.load(); }
    tvIdx = -1;
    document.querySelectorAll('.tv-channel-item.active').forEach(el => el.classList.remove('active'));
    setText('tv-now-playing', 'Select a channel');
    document.getElementById('tv-stop')?.classList.add('hidden');
}

function tvPipClose(ev) {
    if (ev) ev.stopPropagation();
    tvStop();
    updateTvPip();
}

/* Tap = back to the TV page; drag = snap to the nearest screen corner (classes
   only, no inline CSS). A move under 12 px counts as a tap. */
const TV_PIP_CORNERS = ['pip-tl', 'pip-tr', 'pip-bl', 'pip-br'];
/* A PiP window: a tap opens its page; a drag moves it with the finger and, on
   release, glides it into the nearest corner. The stylesheet does the moving
   (.pip-dragging / .pip-glide); JS only supplies the offset as --pip-dx/--pip-dy. */
function makePipDraggable(pip, onTap, ignoreSelector) {
    if (!pip) return;
    let startX = 0, startY = 0, moved = false, active = false;
    const setOffset = (dx, dy) => {
        pip.style.setProperty('--pip-dx', `${dx}px`);
        pip.style.setProperty('--pip-dy', `${dy}px`);
    };
    pip.addEventListener('pointerdown', e => {
        if (!pip.classList.contains('pip') && pip.id === 'app-layer') return;   // full size: not a PiP
        if (e.target.closest(ignoreSelector)) return;
        active = true; moved = false;
        startX = e.clientX; startY = e.clientY;
        pip.classList.remove('pip-glide');
        pip.setPointerCapture(e.pointerId);
    });
    pip.addEventListener('pointermove', e => {
        if (!active) return;
        const dx = e.clientX - startX, dy = e.clientY - startY;
        if (!moved && Math.hypot(dx, dy) < 12) return;   // still a tap
        moved = true;
        pip.classList.add('pip-dragging');
        setOffset(dx, dy);
    });
    const finish = e => {
        if (!active) return;
        active = false;
        if (!moved) { onTap(); return; }
        // FLIP: note where the window was dropped, switch corner, then animate
        // from the drop point to the new corner.
        const from = pip.getBoundingClientRect();
        const right = from.left + from.width / 2 > window.innerWidth / 2;
        const bottom = from.top + from.height / 2 > window.innerHeight / 2;
        pip.classList.remove(...TV_PIP_CORNERS);
        pip.classList.add(`pip-${bottom ? 'b' : 't'}${right ? 'r' : 'l'}`);
        setOffset(0, 0);
        const to = pip.getBoundingClientRect();
        setOffset(from.left - to.left, from.top - to.top);
        void pip.offsetWidth;                            // commit the start position
        pip.classList.add('pip-glide');
        setOffset(0, 0);
        pip.addEventListener('transitionend', () => {
            pip.classList.remove('pip-dragging', 'pip-glide');
            pip.style.removeProperty('--pip-dx');
            pip.style.removeProperty('--pip-dy');
        }, { once: true });
    };
    pip.addEventListener('pointerup', finish);
    pip.addEventListener('pointercancel', finish);
}

function initTvPip() {
    makePipDraggable(document.getElementById('tv-pip'), () => goToPage(TV_PAGE), '.tv-pip-close');
    makePipDraggable(document.getElementById('app-layer'), () => goToPage(APP_PAGE), '.app-pip-close');
}
document.addEventListener('DOMContentLoaded', initTvPip);

// --- Radio ---
let radioStations = [];
let radioIdx = -1;
let radioCountry = null;   // null = All

/* Indices into `radioStations` that survive the current country filter.
   playStation()/radioIdx always use the ORIGINAL index so a filter change can
   never silently start a different station; prev/next cycle within this view. */
function radioVisibleIndices() {
    if (!Array.isArray(radioStations)) return [];
    return radioStations
        .map((s, i) => [s, i])
        .filter(([s]) => !radioCountry || (radioCountry === FAV ? favs('radio').has(s.name) : (s.country || '') === radioCountry))
        .map(([, i]) => i);
}

/* Shared country-filter chips for the radio and TV lists. Derived from the data
   so a new country in the JSON needs no code change, and ordered by how many
   entries it has so the countries actually in use come first. */
function renderCountryTabs(tabsId, items, selected, onPick, favKind) {
    const tabs = document.getElementById(tabsId);
    if (!tabs) return;
    const counts = new Map();
    for (const it of items) {
        const c = it.country || 'Other';
        counts.set(c, (counts.get(c) || 0) + 1);
    }
    const countries = [...counts.entries()]
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
    tabs.textContent = '';
    const mk = (label, value, count) => {
        const b = document.createElement('button');
        b.className = 'media-tab' + (selected === value ? ' active' : '');
        b.textContent = `${label} ${count}`;   // textContent: country names come from a config file
        b.addEventListener('click', () => onPick(value));
        return b;
    };
    const favCount = favKind ? items.filter(it => favs(favKind).has(it.name)).length : 0;
    if (favKind) tabs.appendChild(mk(`${FAV} Favourites`, FAV, favCount));
    tabs.appendChild(mk('All', null, items.length));
    for (const [c, n] of countries) tabs.appendChild(mk(c, c, n));
}

function renderRadioCountryTabs() {
    renderCountryTabs('radio-country-tabs', radioStations, radioCountry, setRadioCountry, 'radio');
}

function setRadioCountry(country) {
    radioCountry = country;
    renderRadioCountryTabs();
    renderRadioStations();
}

function renderRadioStations() {
    const list = document.getElementById('radio-station-list');
    if (!list) return;
    if (!Array.isArray(radioStations) || radioStations.length === 0) {
        list.innerHTML = '<p class="list-empty list-empty-left">No stations configured</p>';
        return;
    }
    const visible = radioVisibleIndices();
    if (visible.length === 0) {
        list.innerHTML = radioCountry === FAV
            ? '<p class="list-empty list-empty-left">No favourites yet — tap ☆ on a station</p>'
            : '<p class="list-empty list-empty-left">No stations for this country</p>';
        return;
    }
    // DOM, not markup: the star needs its own handler. Original index kept —
    // filtering must not renumber the stations.
    list.textContent = '';
    for (const i of visible) {
        const s = radioStations[i];
        const row = document.createElement('div');
        row.className = 'radio-station-item fav-row' + (i === radioIdx ? ' active' : '');
        const text = document.createElement('span');
        text.className = 'fav-row-text';
        text.textContent = s.name;
        const small = document.createElement('small');
        small.textContent = s.genre || '';
        text.appendChild(small);
        row.append(text, favButton('radio', s.name));
        row.addEventListener('click', () => playStation(i));
        list.appendChild(row);
    }
}

async function fetchRadioStations() {
    try {
        const res = await fetch('/api/radio/stations');
        radioStations = await res.json();
        if (!Array.isArray(radioStations)) radioStations = [];
        if (radioCountry === null && favs('radio').size) radioCountry = FAV;
        renderRadioCountryTabs();
        renderRadioStations();
        // Preselect the first station but never auto-play: without a user
        // gesture the browser blocks it anyway.
        const first = radioVisibleIndices()[0];
        if (first !== undefined) {
            radioIdx = first;
            document.getElementById('radio-station-name').textContent = radioStations[first].name;
            renderRadioStations();
        }
    } catch (e) { console.warn('fetchRadioStations failed', e); }
}

function playStation(idx, opts) {
    if (!radioStations[idx]) return;
    radioIdx = idx;
    lastMediaSource = 'radio';
    const station = radioStations[idx];
    // Remembered for the resume after a reboot (not when it IS the resume).
    if (!opts?.resumed) window.reportMediaSession?.('play', 'radio', station);
    document.getElementById('radio-station-name').textContent = station.name;
    // The rendered rows are the FILTERED view, so their DOM position is not the
    // station index. Re-render instead of matching by position, which would mark
    // the wrong row (or none) whenever a country filter is active.
    renderRadioStations();
    const audio = document.getElementById('radio-player');
    const btn = document.getElementById('radio-play-btn');

    // Update home page media section immediately
    setText('media-title', station.name);
    setText('media-artist', station.genre || 'Radio');
    const artEl = document.getElementById('media-art');
    if (artEl) artEl.innerHTML = '<i class="fas fa-broadcast-tower"></i>';

    audio.onerror = null;
    audio.src = '/api/radio/stream?url=' + encodeURIComponent(station.url);
    audio.onerror = () => {
        if (btn) btn.querySelector('i').className = 'fas fa-play';
        document.getElementById('radio-station-name').textContent = station.name + ' (offline)';
        setText('media-title', station.name + ' (offline)');
        setText('media-artist', '');
    };
    audio.play().then(() => {
        connectVisualizerToAudio(audio);
        if (btn) btn.querySelector('i').className = 'fas fa-pause';
        const playBtnHome = document.querySelector('#media-play-btn i');
        if (playBtnHome) playBtnHome.className = 'fas fa-pause';
    }).catch(() => {
        if (btn) btn.querySelector('i').className = 'fas fa-play';
    });
    if (btn) btn.querySelector('i').className = 'fas fa-pause';
}

/* Step within the filtered view. Cycling the full list would jump to a country
   the driver has filtered out, which reads as the filter being broken. */
function radioStep(delta) {
    const visible = radioVisibleIndices();
    if (!visible.length) return;
    const at = visible.indexOf(radioIdx);
    const next = at === -1 ? 0 : (at + delta + visible.length) % visible.length;
    playStation(visible[next]);
}

function radioPrev() { radioStep(-1); }
function radioNext() { radioStep(1); }

function radioPlayPause() {
    const audio = document.getElementById('radio-player');
    const btn = document.getElementById('radio-play-btn');
    if (!audio) return;
    if (audio.paused) {
        // If no src loaded yet, start from selected station
        if (!audio.src || audio.readyState === 0) {
            playStation(radioIdx >= 0 ? radioIdx : 0);
            return;
        }
        audio.play().then(() => connectVisualizerToAudio(audio)).catch(() => {});
        if (btn) btn.querySelector('i').className = 'fas fa-pause';
        const station = radioStations[radioIdx];
        if (station) window.reportMediaSession?.('play', 'radio', station);
        if (station) {
            setText('media-title', station.name);
            setText('media-artist', station.genre || 'Radio');
        }
    } else {
        audio.pause();
        // Paused by the driver: not resumed after a reboot.
        window.reportMediaSession?.('stop', 'radio');
        if (btn) btn.querySelector('i').className = 'fas fa-play';
        setText('media-title', 'No media playing');
        setText('media-artist', '');
        // Pause only: detach the analyser but keep the captured stream alive, so
        // resuming does not depend on captureStream() handing back a live stream.
        if (window.pauseVisualizerAudio) window.pauseVisualizerAudio();
    }
}

// --- Embedded apps (YouTube, Netflix, Disney+, Free TV, Meet, Teams) ---
// Framed in the dashboard like Maps (the car browser's frame-unlock extension
// lifts their anti-framing headers), so the dock, status bar and keyboard stay
// around them. One app at a time: two players would talk over each other and
// the Pi has no CPU to spare. Off its page the app shrinks to a PiP window.
// Everything an app frame needs except escaping it (see embedApp).
const APP_SANDBOX = 'allow-scripts allow-same-origin allow-forms allow-modals allow-presentation '
    + 'allow-downloads allow-pointer-lock allow-orientation-lock allow-storage-access-by-user-activation';
const APP_PAGE = 13;
const APP_ALLOW = 'encrypted-media; autoplay; fullscreen; picture-in-picture; clipboard-write';
const MEETING_ALLOW = `${APP_ALLOW}; microphone; camera; display-capture`;
const MEETING_APPS = ['meet', 'teams'];
let streamingApps = { apps: [] };
let embedded = null;        // { id, name, iframe }
let appPipOn = false;

async function refreshApps() {
    try { streamingApps = await (await fetch('/api/apps')).json(); }
    catch (e) { console.warn('refreshApps failed', e); }
    document.querySelectorAll('.app-tile[data-app]')
        .forEach(t => t.classList.toggle('running', t.dataset.app === embedded?.id));
}

async function openApp(id) {
    if (!streamingApps.apps.length) await refreshApps();
    const app = streamingApps.apps.find(a => a.id === id);
    if (!app) { notify('Unknown app'); return; }
    embedApp(app.id, app.name, embedded?.id === id ? null : app.url);
}

/* Show an app full size, loading it (or `url`, for a meeting) unless it is the
   one already open — reopening must not restart a film. */
function embedApp(id, name, url) {
    if (embedded && (embedded.id !== id || url)) closeEmbeddedApp();
    if (!embedded) {
        if (tvIsPlaying()) tvStop();
        const iframe = document.createElement('iframe');
        iframe.className = 'app-frame';
        iframe.title = name;
        iframe.allow = MEETING_APPS.includes(id) ? MEETING_ALLOW : APP_ALLOW;
        // No allow-top-navigation / allow-popups: Disney+ replaced the whole
        // dashboard with itself (frame-busting at login), and a popup would open
        // a tab with no way back. Sign-in happens in its own tab instead (the
        // Sign in button).
        iframe.sandbox = APP_SANDBOX;
        iframe.src = url || streamingApps.apps.find(a => a.id === id)?.url || 'about:blank';
        document.getElementById('app-frames').replaceChildren(iframe);
        embedded = { id, name, iframe };
        setText('app-pip-name', name);
        if (MEETING_APPS.includes(id)) startMeetingHeartbeat();
    }
    refreshApps();
    goToPage(APP_PAGE);
    updateAppLayer();
}

/* While a meeting frame is open the car hands us its microphone (CarPlay
   telephony stream, see /api/call). The heartbeat keeps the call alive; if the
   page dies it stops and the call ends by itself after a few seconds. */
let meetingHeartbeat = null;
function postCall(active) {
    fetch('/api/call', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ active }) })
        .catch(() => {});
}
function startMeetingHeartbeat() {
    clearInterval(meetingHeartbeat);
    postCall(true);
    meetingHeartbeat = setInterval(() => postCall(true), 3000);
}
function stopMeetingHeartbeat() {
    if (!meetingHeartbeat) return;
    clearInterval(meetingHeartbeat);
    meetingHeartbeat = null;
    postCall(false);
}

function closeEmbeddedApp(e) {
    e?.stopPropagation();
    stopMeetingHeartbeat();
    document.getElementById('app-frames')?.replaceChildren();   // unloads it: playback stops
    document.getElementById('app-osk')?.classList.add('hidden');
    embedded = null;
    appPipOn = false;
    refreshApps();
    updateAppLayer();
    if (currentPage === APP_PAGE) goToPage(6);
}

/* Full size over page 13, PiP anywhere else, hidden without an app. Called on
   every page change. In PiP the backend pins the site's video over its frame
   (meetings keep their own layout: several videos, all useful). */
function updateAppLayer() {
    const layer = document.getElementById('app-layer');
    if (!layer) return;
    const pip = !!embedded && currentPage !== APP_PAGE;
    layer.classList.toggle('hidden', !embedded);
    layer.classList.toggle('pip', pip);
    if (!pip) layer.classList.remove(...TV_PIP_CORNERS.filter(c => c !== 'pip-bl'));
    if (pip && !TV_PIP_CORNERS.some(c => layer.classList.contains(c))) layer.classList.add('pip-bl');
    if (pip) document.getElementById('app-osk')?.classList.add('hidden');
    if (embedded && pip !== appPipOn && !MEETING_APPS.includes(embedded.id)) {
        fetch(`/api/apps/${encodeURIComponent(embedded.id)}/pip`, {
            method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ on: pip }),
        }).catch(() => {});
    }
    appPipOn = pip;
}

function toggleAppKeyboard() {
    document.getElementById('app-osk')?.classList.toggle('hidden');
}

/* Sign in to the open app. Some sites (Google) refuse to sign in inside a
   frame, so the backend opens their sign-in page as its own tab, with a
   keyboard and a Done button; the dashboard is hidden meanwhile. When that tab
   closes the backend sends {"type":"signin-done"} and the app is reloaded so
   it picks up the new session. */
let signInPending = null;
async function signInEmbeddedApp() {
    if (!embedded) return;
    const { id, name } = embedded;
    try {
        const r = await fetch(`/api/apps/${encodeURIComponent(id)}/signin`, { method: 'POST' });
        if (!r.ok) { notify(`${name}: sign-in unavailable`); return; }
        signInPending = id;
    } catch (e) {
        notify(`${name}: sign-in unavailable`);
    }
}

/* Same for the navigation map: Google refuses its sign-in inside the frame,
   so it happens in the sign-in tab, then the map reloads with the session. */
async function signInNavMap() {
    const id = navSource === 'waze' ? 'waze' : 'maps';
    const name = navSource === 'waze' ? 'Waze' : 'Google Maps';
    try {
        const r = await fetch(`/api/apps/${id}/signin`, { method: 'POST' });
        if (!r.ok) { notify(`${name}: sign-in unavailable`); return; }
        signInPending = id;
    } catch (e) {
        notify(`${name}: sign-in unavailable`);
    }
}

function onSignInDone(msg) {
    if (!signInPending || msg.id !== signInPending) return;
    signInPending = null;
    if (msg.id === 'maps' || msg.id === 'waze') { renderNavMap(true); return; }
    if (msg.id === 'ytmusic') { ytmOnSignInDone(msg); return; }
    const app = streamingApps.apps.find(a => a.id === msg.id);
    if (app) embedApp(app.id, app.name, app.url);
}

// --- YouTube Music (built-in player) ---
/* Home (default): your playlists + recently played when signed in, the YouTube
   Music home feed always (it is public). Search mode: the on-screen keyboard
   and the results list, opened by tapping the search field. */
let ytmText = '';
let ytmSearching = false;
let ytmHomeLoadedAt = 0;

function ytmRender() {
    const el = document.getElementById('ytm-query');
    if (!el) return;
    el.textContent = ytmText || 'Search songs, artists…';
    el.classList.toggle('muted', !ytmText);
}

function ytmKey(c) {
    if (ytmText.length >= 80) return;
    ytmText += c;
    ytmRender();
}

function ytmBackspace() { ytmText = ytmText.slice(0, -1); ytmRender(); }
function ytmClear() { ytmText = ''; ytmRender(); document.getElementById('ytm-results')?.replaceChildren(); }

function ytmSetSearchMode(on) {
    ytmSearching = on;
    document.getElementById('ytm-osk')?.classList.toggle('hidden', !on);
    document.getElementById('ytm-results')?.classList.toggle('hidden', !on);
    document.getElementById('ytm-home')?.classList.toggle('hidden', on);
    document.getElementById('ytm-close-search')?.classList.toggle('hidden', !on);
}
function ytmOpenSearch() { ytmSetSearchMode(true); }
function ytmCloseSearch() { ytmClear(); ytmSetSearchMode(false); }

function ytmMessage(list, text) {
    const empty = document.createElement('div');
    empty.className = 'page-empty';
    const icon = document.createElement('i'); icon.className = 'fas fa-headphones';
    const p = document.createElement('p'); p.textContent = text;
    empty.append(icon, p);
    list.replaceChildren(empty);
}

async function ytmSearch() {
    const q = ytmText.trim();
    const list = document.getElementById('ytm-results');
    if (!list || !q) return;
    ytmMessage(list, 'Searching…');
    try {
        const res = await fetch(`/api/music/search?q=${encodeURIComponent(q)}`);
        const data = await res.json();
        if (data.error) { ytmMessage(list, data.error === 'unavailable' ? 'YouTube Music is unavailable on this box' : 'Search failed'); return; }
        const tracks = (data.tracks || []).filter(t => t.videoId);
        if (!tracks.length) { ytmMessage(list, 'No results'); return; }
        list.replaceChildren(...tracks.map(t => {
            const row = document.createElement('button');
            row.className = 'ytm-track';
            const art = document.createElement('img');
            art.className = 'ytm-art';
            art.alt = '';
            art.loading = 'lazy';
            art.referrerPolicy = 'no-referrer'; // yt3.ggpht.com answers 429 to a cross-site Referer
            if (t.art) art.src = t.art;
            const text = document.createElement('span');
            text.className = 'ytm-track-text';
            const title = document.createElement('strong'); title.textContent = t.title || 'Untitled';
            const sub = document.createElement('small'); sub.className = 'muted';
            sub.textContent = [t.artist, t.album].filter(Boolean).join(' · ');
            text.append(title, sub);
            const play = document.createElement('i'); play.className = 'fas fa-play';
            row.append(art, text, play);
            row.onclick = () => ytmPlay(t);
            return row;
        }));
    } catch (e) {
        ytmMessage(list, 'Search failed — dashboard unreachable');
    }
}

/* One track (then its radio), or a list from `index` when `list` is given. */
async function ytmPlay(t, list, index) {
    try {
        const tracks = (list || []).filter(x => x && x.videoId);
        const [path, body] = tracks.length
            ? ['/api/music/queue', { tracks, start: Math.max(0, tracks.indexOf(t)), shuffle: false }]
            // The whole track: the queue shows its title/art; its radio follows.
            : ['/api/music/play', t];
        const res = await fetch(path, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
        });
        const data = await res.json().catch(() => ({}));
        if (!res.ok || !data.ok) { notify(data.error || 'Could not play track'); return; }
        notify(`Playing ${t.title || 'track'}`);
        // The full player: karaoke, queue actions, visualizer (not just Home).
        goToPage(4);
        if (window.RasCar && RasCar.media) RasCar.media.enterFullscreen();
    } catch (e) {
        notify('Could not play track');
    }
}

// --- YouTube Music home ---
function ytmSection(home, title) {
    const h = document.createElement('div');
    h.className = 'list-group-header';
    h.textContent = title;          // server data — text, never markup
    const grid = document.createElement('div');
    grid.className = 'playlist-grid ytm-home-grid';
    home.append(h, grid);
    return grid;
}

function ytmCard(art, title, sub, onTap) {
    const card = RasCar.media.makeCard(art, title, sub);
    card.addEventListener('click', onTap);
    return card;
}

/* What the page says about the account, from /api/music/status `session`. */
const YTM_SESSION_TEXT = {
    ok: 'Signed in to YouTube Music.',
    none: 'Sign in to YouTube Music to see your playlists',
    refused: 'YouTube Music refused the session — sign in again',
    unavailable: 'YouTube Music is unreachable right now',
};

function ytmSignInCard(home) {
    const card = document.createElement('div');
    card.className = 'card ytm-signin-card';
    const text = document.createElement('div');
    const strong = document.createElement('strong');
    strong.textContent = YTM_SESSION_TEXT[ytmSession] || YTM_SESSION_TEXT.none;
    const small = document.createElement('small');
    small.className = 'muted';
    small.textContent = 'Opens Google’s sign-in page with an on-screen keyboard; tap Done when finished.';
    text.append(strong, small);
    const b = document.createElement('button');
    b.className = 'btn-primary';
    b.innerHTML = '<i class="fab fa-google"></i> Sign in';
    b.addEventListener('click', ytmStartSignIn);
    card.append(text, b);
    home.appendChild(card);
}

async function ytmFetch(path) {
    try { return await (await fetch(path)).json(); } catch (e) { return { error: 'unreachable' }; }
}

async function ytmLoadHome(force) {
    const home = document.getElementById('ytm-home');
    if (!home || !window.RasCar || !RasCar.media) return;
    // The helper spawns python and hits the network: not on every page visit.
    if (!force && Date.now() - ytmHomeLoadedAt < 5 * 60000 && home.childElementCount) return;
    ytmHomeLoadedAt = Date.now();
    if (!home.childElementCount) ytmMessage(home, 'Loading YouTube Music…');
    const [playlists, history, feed] = await Promise.all([
        ytmFetch('/api/music/playlists'), ytmFetch('/api/music/history?limit=12'), ytmFetch('/api/music/home'),
    ]);
    // "Signed in" = an authenticated call worked, never just "a file exists".
    const signedIn = !playlists.error;
    ytmSession = signedIn ? 'ok' : ((await ytmFetch('/api/music/status')).session || 'unavailable');
    ytmUpdateAccountButton();
    home.replaceChildren();
    if (!signedIn) ytmSignInCard(home);
    if (signedIn) {
        const grid = ytmSection(home, 'Your playlists');
        grid.appendChild(ytmCard('', 'Liked songs', 'Your thumbs-up', () => RasCar.media.showPlaylist('LM', 'Liked songs')));
        for (const p of (playlists.playlists || []).slice(0, 10)) {
            if (!p.id) continue;
            grid.appendChild(ytmCard(p.art, p.title || 'Playlist', p.count ? `${p.count} tracks` : '',
                () => RasCar.media.showPlaylist(p.id, p.title)));
        }
        if ((playlists.playlists || []).length > 10) {
            grid.appendChild(ytmCard('', 'All playlists', `${playlists.playlists.length} playlists`, () => RasCar.media.showAllPlaylists()));
        }
        const played = (history.tracks || []).filter(t => t.videoId);
        if (played.length) {
            const g = ytmSection(home, 'Recently played');
            for (const t of played) g.appendChild(ytmCard(t.art, t.title || 'Track', t.artist || t.album || '', () => ytmPlay(t, played)));
        }
    }
    for (const shelf of (feed.shelves || [])) {
        const g = ytmSection(home, shelf.title || 'From YouTube Music');
        const tracks = shelf.items.filter(i => i.kind === 'track');
        for (const it of shelf.items) {
            const sub = it.subtitle || (it.kind === 'playlist' ? 'Playlist' : '');
            g.appendChild(ytmCard(it.art, it.title || '—', sub, it.kind === 'track'
                ? () => ytmPlay(it, tracks)
                : () => RasCar.media.showPlaylist(it.id, it.title)));
        }
    }
    if (!home.childElementCount) ytmMessage(home, 'YouTube Music is unavailable right now');
}

// --- YouTube Music account (sign-in in a car-browser tab, see signin.rs) ---
let ytmSession = 'none';

function ytmUpdateAccountButton() {
    const b = document.getElementById('ytm-account');
    if (!b) return;
    b.classList.toggle('active', ytmSession === 'ok');
    b.setAttribute('aria-label', ytmSession === 'ok' ? 'YouTube Music: signed in' : 'Sign in to YouTube Music');
}

function ytmRenderAccount() {
    setText('ytm-signin-status', YTM_SESSION_TEXT[ytmSession] || YTM_SESSION_TEXT.none);
    document.getElementById('ytm-signin-go').classList.toggle('hidden', ytmSession === 'ok');
    document.getElementById('ytm-signout').classList.toggle('hidden', ytmSession !== 'ok');
}

async function ytmAccount() {
    document.getElementById('ytm-signin-overlay').classList.remove('hidden');
    ytmRenderAccount();
    const st = await ytmFetch('/api/music/status');
    if (st.session) { ytmSession = st.session; ytmRenderAccount(); ytmUpdateAccountButton(); }
}

function ytmCloseSignIn() {
    document.getElementById('ytm-signin-overlay').classList.add('hidden');
}

async function ytmStartSignIn() {
    ytmCloseSignIn();
    try {
        const r = await fetch('/api/apps/ytmusic/signin', { method: 'POST' });
        if (!r.ok) { notify('YouTube Music: sign-in unavailable'); return; }
        signInPending = 'ytmusic';
    } catch (e) {
        notify('YouTube Music: sign-in unavailable');
    }
}

/* The sign-in tab closed; the backend has already tried to take the session over. */
function ytmOnSignInDone(msg) {
    ytmSession = msg.session === 'signed_out' ? 'none' : (msg.session || 'unavailable');
    notify(ytmSession === 'ok' ? 'Signed in to YouTube Music' : (YTM_SESSION_TEXT[ytmSession] || 'Sign-in not completed'));
    ytmUpdateAccountButton();
    ytmLoadHome(true);
}

async function ytmSignOut() {
    try { await fetch('/api/music/signout', { method: 'POST' }); } catch (e) { /* the reload below tells */ }
    ytmCloseSignIn();
    ytmLoadHome(true);
}

// --- Touch glow ---
// Touches arrive from the CarPlay injector over the WebSocket (x, y in page
// pixels): a ring on press, a glowing dot that follows the finger, fading out
// on release. Positions go through --tx/--ty; the stylesheet does the drawing.
let touchDot = null;

function placeTouch(el, m) {
    el.style.setProperty('--tx', `${m.x}px`);
    el.style.setProperty('--ty', `${m.y}px`);
}

function showTouch(m) {
    const layer = document.getElementById('touch-layer');
    if (!layer) return;
    if (m.phase === 'press') {
        const ring = document.createElement('span');
        ring.className = 'touch-ring';
        placeTouch(ring, m);
        ring.addEventListener('animationend', () => ring.remove(), { once: true });
        layer.append(ring);
        touchDot?.remove();
        touchDot = document.createElement('span');
        touchDot.className = 'touch-dot';
        placeTouch(touchDot, m);
        layer.append(touchDot);
    } else if (m.phase === 'drag') {
        if (touchDot) placeTouch(touchDot, m);
    } else if (touchDot) {
        const dot = touchDot;
        touchDot = null;
        dot.classList.add('touch-out');
        dot.addEventListener('animationend', () => dot.remove(), { once: true });
    }
}

// --- Phone dialer ---
let contacts = [];
let callHistory = [];
let dialerNumber = '';
let dialerLabel = '';   // contact name shown above the number, if picked from a list
let dialerView = 'contacts';

async function fetchContacts() {
    try {
        const res = await fetch('/api/contacts');
        const list = await res.json();
        contacts = Array.isArray(list) ? list.filter(c => c && c.phone) : [];
    } catch (e) { console.warn('fetchContacts failed', e); }
}

async function fetchCallHistory() {
    try {
        const res = await fetch('/api/calls');
        const list = await res.json();
        callHistory = Array.isArray(list) ? list : [];
    } catch (e) { console.warn('fetchCallHistory failed', e); }
}

function openDialer() {
    dialerSet('', '');
    document.getElementById('dialer-overlay').classList.remove('hidden');
    Promise.all([fetchContacts(), fetchCallHistory()]).then(renderDialerList);
    renderDialerList();
}

function closeDialer() {
    document.getElementById('dialer-overlay').classList.add('hidden');
}

function dialerSet(number, label) {
    dialerNumber = number;
    dialerLabel = label;
    setText('dialer-number', number);
    setText('dialer-name', label);
    document.getElementById('dialer-call').disabled = !/[0-9]/.test(number);
}

function dialerKey(k) {
    if (dialerNumber.length >= 32) return;
    // Typing after picking a contact edits a raw number, so drop the name.
    dialerSet(dialerNumber + k, '');
}

function dialerBackspace() {
    dialerSet(dialerNumber.slice(0, -1), '');
}

function dialerTab(view) {
    dialerView = view;
    document.querySelectorAll('#dialer-overlay .media-tab')
        .forEach(b => b.classList.toggle('active', b.dataset.tab === view));
    renderDialerList();
}

function contactNameFor(number) {
    const digits = n => String(n).replace(/[^0-9+]/g, '');
    return contacts.find(c => digits(c.phone) === digits(number))?.name || '';
}

function fmtCallTime(iso) {
    const d = new Date(iso);
    if (isNaN(d)) return '';
    const time = d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
    const today = new Date();
    const yesterday = new Date(); yesterday.setDate(today.getDate() - 1);
    if (d.toDateString() === today.toDateString()) return time;
    if (d.toDateString() === yesterday.toDateString()) return `Yesterday ${time}`;
    return `${d.toLocaleDateString('fr-FR', { day: 'numeric', month: 'short' })} ${time}`;
}

function renderDialerList() {
    const list = document.getElementById('dialer-list');
    if (!list) return;
    list.replaceChildren();
    const rows = dialerView === 'contacts'
        ? contacts.map(c => ({ name: c.name || c.phone, number: c.phone, meta: c.phone }))
        : callHistory.map(h => {
            const name = h.name || contactNameFor(h.number);
            return { name: name || h.number, number: h.number, meta: name ? `${h.number} · ${fmtCallTime(h.at)}` : fmtCallTime(h.at) };
        });
    if (!rows.length) {
        const empty = document.createElement('div');
        empty.className = 'page-empty';
        const icon = document.createElement('i');
        icon.className = dialerView === 'contacts' ? 'fas fa-address-book' : 'fas fa-clock-rotate-left';
        const p = document.createElement('p');
        p.textContent = dialerView === 'contacts' ? 'No contacts configured' : 'No recent calls';
        empty.append(icon, p);
        list.append(empty);
        return;
    }
    for (const r of rows) {
        const row = document.createElement('button');
        row.className = 'dialer-row';
        const avatar = document.createElement('span');
        avatar.className = 'dialer-avatar';
        avatar.textContent = (r.name.trim()[0] || '#').toUpperCase();
        const text = document.createElement('span');
        text.className = 'dialer-row-text';
        const name = document.createElement('strong');
        name.textContent = r.name;
        const meta = document.createElement('small');
        meta.className = 'muted';
        meta.textContent = r.meta;
        text.append(name, meta);
        row.append(avatar, text);
        // Picking fills the keypad display; the Call button is the only thing
        // that dials, so a mis-tap in a moving car never places a call.
        row.onclick = () => dialerSet(r.number, r.name === r.number ? '' : r.name);
        list.append(row);
    }
}

async function dialerCall() {
    const number = dialerNumber.trim();
    if (!/[0-9]/.test(number)) return;
    const name = dialerLabel || contactNameFor(number);
    // Through the 4G modem when it can call (js/phone.js shows the call);
    // otherwise, or if the modem refuses, the tel: hand-off as before. Never a
    // filter in front: 112 dials like any other number.
    const modem = !!RasCar.phone?.available();
    const viaModem = modem && await RasCar.phone.dial(number);
    if (!viaModem) dialTel(number);
    // The modem refused (its error is on screen): keep the dialer and the
    // typed number so a retry is one tap.
    if (modem && !viaModem) return;
    // A modem call is logged by the backend (services/phone, with direction and
    // missed flag); logging it here too would list it twice in recents.
    if (viaModem) { closeDialer(); return; }
    try {
        const res = await fetch('/api/calls', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ number, name }),
        });
        if (res.ok) callHistory = await res.json();
    } catch (e) { console.warn('call history not saved', e); }
    // The modem path shows its own call screen; only tel: needs the toast.
    if (!viaModem) notify(`Calling ${name || number}…`);
    closeDialer();
}

// A tel: URI on the top-level document replaces the dashboard with an error page
// when no handler is registered — and kiosk Chromium offers no way back. Hand the
// URI to a throwaway iframe so this document is never navigated away from.
function dialTel(number) {
    const frame = document.createElement('iframe');
    frame.className = 'hidden';
    document.body.appendChild(frame);
    try { frame.src = `tel:${encodeURIComponent(number)}`; } catch (e) {}
    setTimeout(() => frame.remove(), 2000);
}

// --- OBD metric list ---
// One row per value the car supports (backend obd.metrics: k key, l label,
// u unit, d decimals, v value), each opening its history chart. Until the car
// has answered, the values this page always showed are listed as "—".
const OBD_DEFAULT_ROWS = [
    { k: 'load', l: 'Load', u: '%' }, { k: 'throttle', l: 'Throttle', u: '%' },
    { k: 'boost', l: 'Manifold pressure', u: 'kPa' }, { k: 'fuel', l: 'Fuel level', u: '%' },
    { k: 'intake', l: 'Intake air', u: '°C' }, { k: 'oil', l: 'Oil', u: '°C' },
];
const obdLabels = { rpm: 'RPM', speed: 'Speed', coolant: 'Coolant', battery: 'Battery' };
let obdMetricKeys = '';

function fmtObdValue(m) {
    // Enumerated values (fuel system status, boost control) carry their text.
    if (m.t) return m.t;
    if (m.v === undefined || m.v === null || !Number.isFinite(m.v)) return '—';
    if (m.u === '°C') return fmtTemp(m.v);
    if (m.u === 'km/h') return fmtSpeed(m.v);
    if (m.u === 'on/off') return m.v ? 'On' : 'Off';
    const n = m.v.toFixed(m.d ?? 0);
    if (!m.u) return n;
    return (m.u === '%' || m.u === '°') ? `${n}${m.u}` : `${n} ${m.u}`;
}

function renderObdMetrics(metrics) {
    const box = document.getElementById('obd-metrics');
    if (!box) return;
    const rows = Array.isArray(metrics) && metrics.length ? metrics : OBD_DEFAULT_ROWS;
    const keys = rows.map(m => m.k).join(',');
    if (keys !== obdMetricKeys) {
        obdMetricKeys = keys;
        box.innerHTML = rows.map(m => `
            <div class="metric-row" data-key="${escapeHtml(m.k)}">
                <span>${escapeHtml(m.l)}</span><span></span>
            </div>`).join('');
        box.querySelectorAll('.metric-row').forEach(row =>
            row.addEventListener('click', () => openObdChart(row.dataset.key)));
    }
    rows.forEach(m => {
        obdLabels[m.k] = m.l;
        const cell = box.querySelector(`.metric-row[data-key="${CSS.escape(m.k)}"] span:last-child`);
        if (cell) cell.textContent = fmtObdValue(m);
    });
}

// --- OBD health ---
// The Gauges / Health switch swaps the four gauges for the health panel.
function setObdView(view) {
    const health = view === 'health';
    document.querySelector('.obd-dashboard .gauge-grid')?.classList.toggle('hidden', health);
    document.getElementById('obd-health')?.classList.toggle('hidden', !health);
    document.querySelectorAll('.obd-view-tabs .media-tab').forEach(b =>
        b.classList.toggle('active', b.dataset.view === view));
}

// Lowest in-use performance ratio the EU accepts for most monitors (catalyst,
// O2, EVAP...): below it, short trips are keeping the monitor from running.
const IPT_MIN_RATIO = 0.336;
const TYRES = [['tyre_fl', 'FL'], ['tyre_fr', 'FR'], ['tyre_rl', 'RL'], ['tyre_rr', 'RR']];

// obd.health (monitors, misfire, out_of_limits, ipt) plus the health values of
// obd.metrics (fuel_sys1, boost_cmd / boost_act / boost_ctl, tyre_*).
function renderObdHealth(h, metrics) {
    if (!document.getElementById('obd-health')) return;
    const mv = {};
    (Array.isArray(metrics) ? metrics : []).forEach(m => { mv[m.k] = m; });
    h = h || {};

    // Monitor tests outside their limits: the one thing worth shouting about.
    const bad = Array.isArray(h.out_of_limits) ? h.out_of_limits : [];
    const alert = document.getElementById('obd-health-alert');
    alert.classList.toggle('hidden', bad.length === 0);
    if (bad.length) {
        alert.innerHTML = `<i class="fas fa-triangle-exclamation"></i> ${bad.length} monitor test${bad.length > 1 ? 's' : ''} outside limits: `
            + bad.map(t => escapeHtml(`${t.monitor} (${t.tid})`)).join(', ');
    }

    // Engine: fuel loop, boost commanded vs actual, misfires.
    setText('hl-fuel-loop', mv.fuel_sys1 ? fmtObdValue(mv.fuel_sys1) : '—');
    setText('hl-boost-ctl', mv.boost_ctl ? fmtObdValue(mv.boost_ctl) : '');
    const cmd = mv.boost_cmd?.v, act = mv.boost_act?.v;
    const scale = Math.max(250, cmd ?? 0, act ?? 0);
    const bar = (label, v, cls = '') => `
        <div class="health-bar ${cls}"><span>${escapeHtml(label)}</span>
            <div class="hb-track"><div class="hb-fill" style="width:${Number.isFinite(v) ? Math.min(100, v / scale * 100) : 0}%"></div></div>
            <b>${Number.isFinite(v) ? `${Math.round(v)} kPa` : '—'}</b></div>`;
    document.getElementById('hl-boost').innerHTML = (cmd === undefined && act === undefined)
        ? '<div class="health-empty">Not reported</div>'
        : bar('Commanded', cmd) + bar('Actual', act, 'alt');

    const mis = (Array.isArray(h.misfire) ? h.misfire : []).filter(c => c.cyl > 0);
    setText('hl-misfire-at', h.mode06_at ? `this drive · avg 10 · ${h.mode06_at}` : '');
    const n = v => (Number.isFinite(v) ? Math.round(v) : '—');
    document.getElementById('hl-misfire').innerHTML = mis.length
        ? mis.map(c => `<div class="mf-cyl ${(c.this_drive > 0 || c.avg10 > 0) ? 'warn' : ''}">
                <span>Cyl ${c.cyl}</span><b>${n(c.this_drive)}</b><small>${n(c.avg10)}</small></div>`).join('')
        : `<div class="health-empty">${h.mode06_at ? 'Not reported' : 'Read after 2 min of driving'}</div>`;

    // Monitors this drive (01 41), only the ones the car supports (01 01).
    const mons = Array.isArray(h.monitors) ? h.monitors : [];
    const enabled = mons.filter(m => m.enabled);
    setText('hl-mon-count', enabled.length ? `${enabled.filter(m => m.complete).length}/${enabled.length} complete` : '');
    document.getElementById('hl-monitors').innerHTML = mons.length
        ? mons.map(m => {
            const st = m.enabled === false || m.enabled == null ? 'off' : (m.complete ? 'ok' : 'todo');
            const icon = st === 'ok' ? 'fa-check' : (st === 'todo' ? 'fa-hourglass-half' : 'fa-minus');
            return `<span class="health-chip ${st}"><i class="fas ${icon}"></i>${escapeHtml(m.name)}</span>`;
        }).join('')
        : '<div class="health-empty">Waiting for the engine ECU</div>';

    // In-use performance (09 08): completions / drives with the conditions.
    const ipt = h.ipt;
    setText('hl-ipt-ign', ipt ? `${ipt.ignition_cycles} ignitions` : '');
    const rows = (ipt?.monitors ?? []).filter(m => m.cond > 0);
    document.getElementById('hl-ipt').innerHTML = rows.length
        ? rows.map(m => {
            const r = m.ratio ?? 0;
            return `<div class="health-bar ${r < IPT_MIN_RATIO ? 'low' : ''}"><span>${escapeHtml(m.name)}</span>
                <div class="hb-track"><div class="hb-fill" style="width:${Math.min(100, r * 100)}%"></div>
                    <i class="hb-mark" style="left:${IPT_MIN_RATIO * 100}%"></i></div>
                <b>${r.toFixed(2)}</b></div>`;
        }).join('')
        : `<div class="health-empty">${ipt ? 'No monitor conditions yet' : 'Read after 2 min of driving'}</div>`;

    // Tyres: only when the TPMS reads answered.
    const tyres = TYRES.filter(([k]) => mv[k]);
    document.getElementById('hl-tyres-card').classList.toggle('hidden', tyres.length === 0);
    document.querySelector('.obd-health-grid').classList.toggle('with-tyres', tyres.length > 0);
    document.getElementById('hl-tyres').innerHTML = TYRES.map(([k, label]) =>
        `<div class="tyre"><small>${label}</small><b>${mv[k] ? mv[k].v.toFixed(2) : '—'}</b></div>`).join('');
}

// --- OBD Chart Overlay ---
let obdChartInterval = null;

async function openObdChart(metric) {
    clearInterval(obdChartInterval);
    document.getElementById('obd-chart-overlay').classList.remove('hidden');
    document.getElementById('obd-chart-title').textContent = `${obdLabels[metric] || metric} history`;
    ['min', 'avg', 'max'].forEach(k => setText(`obd-chart-${k}`, '—'));
    await refreshObdChart(metric);
    obdChartInterval = setInterval(() => refreshObdChart(metric), 2000);
}

async function refreshObdChart(metric) {
    try {
        const res = await fetch(`/api/obd/history/${encodeURIComponent(metric)}`);
        const { data, label, unit, decimals } = await res.json();
        if (label) setText('obd-chart-title', `${label} history`);
        if (!data || data.length === 0) return;
        const fmt = v => fmtObdValue({ v, u: unit, d: decimals ?? 1 });
        setText('obd-chart-min', fmt(Math.min(...data)));
        setText('obd-chart-avg', fmt(data.reduce((a, b) => a + b, 0) / data.length));
        setText('obd-chart-max', fmt(Math.max(...data)));
        drawSeriesChart('obd-chart-canvas', data.map((v, i) => [i, v]), 0,
            { color: cssToken('--accent'), fmtValue: v => v.toFixed(decimals ?? 1), fmtTime: null });
    } catch (e) { console.warn('obd chart fetch failed', e); }
}

function closeObdChart() {
    clearInterval(obdChartInterval);
    obdChartInterval = null;
    document.getElementById('obd-chart-overlay').classList.add('hidden');
}

// --- DTC Modal ---
async function openDtcModal() {
    document.getElementById('dtc-overlay').classList.remove('hidden');
    try {
        const res = await fetch('/api/obd/dtc');
        const dtcs = await res.json();
        const list = document.getElementById('dtc-list');
        if (!Array.isArray(dtcs) || dtcs.length === 0) {
            list.innerHTML = '<p class="list-empty">No fault codes</p>';
            return;
        }
        // kind: stored (mode 03), pending (07, not yet confirmed), permanent
        // (0A, only the ECU can erase it). An unknown code keeps its raw code
        // and a description of what its numbering tells.
        const KIND = { stored: 'Stored', pending: 'Pending', permanent: 'Permanent' };
        list.innerHTML = dtcs.map(d => `
            <div class="dtc-row">
                <span class="dtc-code">${escapeHtml(d.code)}</span>
                <span class="dtc-desc">${escapeHtml(d.description)}
                    <small class="muted"> · ${escapeHtml(KIND[d.kind] || d.kind || '')}${d.known === false ? ' · not in the code table' : ''}</small></span>
            </div>`).join('');
    } catch (e) {
        console.warn('dtc fetch failed', e);
        const list = document.getElementById('dtc-list');
        if (list) list.innerHTML = '<p class="list-empty">Fault codes unavailable</p>';
    }
}

function closeDtcModal() {
    document.getElementById('dtc-overlay').classList.add('hidden');
}

async function clearDtcs() {
    await fetch('/api/obd/dtc/clear', { method: 'POST' });
    closeDtcModal();
}

function toggleNavPopup() {
    document.getElementById('nav-popup').classList.toggle('hidden');
    resetInteraction();
}
function closeNavPopup(e) {
    // close on backdrop click (not on tile click)
    if (e.target === document.getElementById('nav-popup')) {
        document.getElementById('nav-popup').classList.add('hidden');
    }
}
function navPopupGo(idx) {
    document.getElementById('nav-popup').classList.add('hidden');
    goToPage(idx);
}

/* Escape / backdrop dismissal. Neither existed: the settings modal could only
   be left through one 34px Close button, and a mis-tap left the driver stuck. */
function initGlobalEscape() {
    const open = el => el && !el.classList.contains('hidden');
    document.addEventListener('keydown', e => {
        if (e.key !== 'Escape') return;
        // Innermost first, so Escape peels one layer at a time.
        if (open(document.getElementById('settings-overlay'))) { toggleSettings(); return; }
        if (open(document.getElementById('date-picker-overlay'))) { closeDatePicker(); return; }
        if (open(document.getElementById('ytm-signin-overlay'))) { ytmCloseSignIn(); return; }
        if (open(document.getElementById('dialer-overlay'))) { closeDialer(); return; }
        if (open(document.getElementById('cal-detail-overlay'))) { closeCalDetail(); return; }
        if (open(document.getElementById('dtc-overlay'))) { closeDtcModal(); return; }
        if (open(document.getElementById('obd-chart-overlay'))) { closeObdChart(); return; }
        const nav = document.getElementById('nav-popup');
        if (open(nav)) nav.classList.add('hidden');
    });
    // Backdrop dismissal for every overlay. `.overlay` is the backdrop and
    // `.overlay-content` the panel, so an exact-target test is enough to avoid
    // dismissing on a click inside the panel.
    const backdrops = [
        ['settings-overlay', toggleSettings],
        ['dialer-overlay', closeDialer],
        ['date-picker-overlay', closeDatePicker],
        ['ytm-signin-overlay', ytmCloseSignIn],
        ['cal-detail-overlay', closeCalDetail],
        ['dtc-overlay', () => closeDtcModal()],
        ['obd-chart-overlay', () => closeObdChart()],
    ];
    for (const [id, close] of backdrops) {
        const el = document.getElementById(id);
        if (el) el.addEventListener('click', e => { if (e.target === el) close(); });
    }
}

function toggleSettings() {
    const overlay = document.getElementById('settings-overlay');
    if (overlay.classList.contains('hidden')) { openSettings('appearance'); return; }
    overlay.classList.add('hidden');
    resetInteraction();
}

/* Open Settings straight on one section (the status-row Bluetooth icon opens
   the OBD adapter section). */
function openSettings(section) {
    document.getElementById('settings-overlay').classList.remove('hidden');
    renderSettings();
    settingsSection(section);
    resetInteraction();
}

// Query the backend for YouTube Music connection status and reflect it in the
// settings panel. Degrades to "Not connected" whenever anything is unavailable.
async function refreshYtmusicStatus() {
    const el = document.getElementById('ytmusic-status');
    if (!el) return;
    el.textContent = 'Checking…';
    el.className = 'ytmusic-status checking';
    try {
        const res = await fetch('/api/music/status');
        const data = await res.json();
        if (data && data.connected) {
            el.textContent = 'Connected';
            el.className = 'ytmusic-status connected';
        } else {
            el.textContent = 'Not connected';
            el.className = 'ytmusic-status disconnected';
        }
    } catch (e) {
        el.textContent = 'Not connected';
        el.className = 'ytmusic-status disconnected';
    }
}

/* Satellites in view from the car's $GPGSV (gps.sats: prn, el, az, snr). One
   bar per satellite, height = SNR on a 0-50 dB-Hz scale; >= 30 is a solid
   signal, 1-29 weak, empty = in view but not tracked. Hidden without data. */
function renderGpsSky(g) {
    const box = document.getElementById('gps-sky');
    if (!box) return;
    const sats = Array.isArray(g?.sats) ? g.sats : [];
    box.classList.toggle('hidden', sats.length === 0);
    if (!sats.length) return;
    const used = g?.sats_used;
    // GSV lists GPS satellites only; the car's GGA count covers every system its
    // fix uses (Galileo, GLONASS...), so "used" can exceed "in view" — say which is which.
    setText('gps-sats', `${g?.sats_view ?? sats.length} GPS in view` + (used != null ? ` · ${used} in the car's fix` : ''));
    const host = document.getElementById('gps-snr');
    // Strongest first, so the bars read as a ranking at a glance.
    const sorted = [...sats].sort((a, b) => (b.snr ?? -1) - (a.snr ?? -1)).slice(0, 24);
    host.replaceChildren(...sorted.map(sat => {
        const snr = sat.snr ?? 0;
        const col = document.createElement('div');
        col.className = 'snr-col';
        col.title = `PRN ${sat.prn}: ${sat.snr ?? '--'} dB-Hz, elevation ${sat.el ?? '--'}°`;
        const bar = document.createElement('div');
        bar.className = 'snr-bar' + (snr >= 30 ? ' ok' : snr > 0 ? ' weak' : '');
        bar.style.height = `${Math.min(100, Math.round(snr / 50 * 100))}%`;
        const prn = document.createElement('span');
        prn.className = 'snr-prn';
        prn.textContent = sat.prn;
        col.append(bar, prn);
        return col;
    }));
}

/* The car's head unit as it identified itself over CarPlay (/api/headunit,
   written by rascar-carplay). Shown on the Vehicle page and in Settings >
   Services. Bounded: a request that hangs is aborted after 3 s. */
async function refreshHeadunit() {
    let hu = null;
    try {
        const ctl = new AbortController();
        const timer = setTimeout(() => ctl.abort(), 3000);
        const res = await fetch('/api/headunit', { signal: ctl.signal });
        clearTimeout(timer);
        if (res.ok) hu = await res.json();
    } catch (e) { /* keep the placeholders */ }
    if (!hu || !hu.known) {
        setText('obd-car-name', '');
        return;
    }
    const name = hu.name || hu.model || '';
    setText('obd-car-name', name);
    setText('hu-name', name || '--');
    // One row (the section is sized to fit without scrolling): the name as the
    // value, the head unit's firmware / hardware / serial underneath.
    setText('hu-detail', [
        hu.firmware && `Head unit firmware ${hu.firmware}`,
        hu.hardware && `hardware ${hu.hardware}`,
        hu.serial && `serial ${hu.serial}`,
    ].filter(Boolean).join(' · '));
    const ma = hu.power?.available_current_ma;
    setText('hu-power', ma != null ? `${ma} mA` : '--');
    const charge = hu.power?.should_charge;
    setText('hu-power-detail', charge == null ? 'Offered to the Pi on the CarPlay port'
        : charge ? 'Offered to the Pi · charging allowed' : 'Offered to the Pi · charging not requested');
}

// --- Screensaver ---
// Note: this only refreshes the idle timer. Dismissal happens exclusively in
// onScreensaverTap() so the waking gesture is always swallowed.
function resetInteraction() {
    lastInteraction = Date.now();
}

// The overlay leaves the hit-test tree the moment it is hidden, so a tap that is
// only handled on touchend/click lands on whatever is underneath (Clear All Codes,
// launchNav, …). Consume the gesture here instead.
function onScreensaverTap(e) {
    e.preventDefault();
    e.stopPropagation();
    lastInteraction = Date.now();
    dismissScreensaver();
}

function bindScreensaver() {
    const ss = document.getElementById('screensaver');
    if (!ss) return;
    const opts = { capture: true, passive: false };
    ss.addEventListener('touchstart', onScreensaverTap, opts);
    ss.addEventListener('mousedown', onScreensaverTap, opts);
    // preventDefault on mousedown does not suppress the click — stop that too.
    ss.addEventListener('click', (e) => { e.preventDefault(); e.stopPropagation(); }, opts);
}

function checkScreensaver() {
    if (screensaverTimeout <= 0) return;
    // Never over the map or an embedded app shown full size (a film, a
    // meeting): they are watched, not touched, and taps inside their
    // cross-origin frames don't reach this page to count as interaction anyway.
    if (currentPage === 3 || (currentPage === APP_PAGE && embedded)) { lastInteraction = Date.now(); return; }
    const elapsed = (Date.now() - lastInteraction) / 1000;
    if (elapsed > screensaverTimeout) {
        document.getElementById('screensaver').classList.remove('hidden');
        updateClock();
    }
}

function dismissScreensaver() {
    document.getElementById('screensaver')?.classList.add('hidden');
}

document.addEventListener('touchstart', resetInteraction);
document.addEventListener('mousedown', resetInteraction);

// --- Helpers ---
function setText(id, text) {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
}

function updateClock() {
    const now = new Date();
    const time = now.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
    setText('sb-clock', time);
    // Screensaver and fullscreen media: HH:MM with smaller seconds.
    const pad = n => String(n).padStart(2, '0');
    const withSec = `${pad(now.getHours())}:${pad(now.getMinutes())}<span class="ss-sec">:${pad(now.getSeconds())}</span>`;
    const fs = document.getElementById('fs-clock');
    if (fs) fs.innerHTML = withSec;
    const ss = document.getElementById('ss-clock');
    if (ss && !document.getElementById('screensaver')?.classList.contains('hidden')) ss.innerHTML = withSec;
    setText('ss-date', now.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' }));
}

function bearingToCompass(deg) {
    const dirs = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
    const idx = Math.round(deg / 45) % 8;
    return `${dirs[idx]} ${Math.round(deg)}°`;
}

/* Phone calls through the 4G modem: incoming-call overlay, in-call screen,
 * minimised status pill, and the dialer's modem path.
 *
 * Backend: app/src/services/phone/ (rascar-app). The WS state carries
 *   phone: {available, modem: "absent"|"ready"|"busy"|"error", voice_domain,
 *           call: null | {id, state: "dialing"|"alerting"|"incoming"|"active"|"held"|"ended",
 *                         direction: "in"|"out", number, name, started_at, duration_s, muted}}
 * ("ended" lingers 3 s, then call goes back to null).
 *   POST /api/phone/dial {number} · /answer · /hangup · /dtmf {digits} · /mute {on}  → {ok, error}
 *
 * The page only reflects the call and forwards taps: the backend owns the
 * call, the car audio (it tells rascar-carplay to ring / switch to telephony)
 * and the steering-wheel keys (Hook = answer or hang up, Drop = decline or
 * hang up), so a wheel press and a tap can never disagree about the state.
 */
(function () {
    const $ = id => document.getElementById(id);
    const STATE_LABEL = {
        incoming: 'Incoming call', dialing: 'Calling…', alerting: 'Ringing…',
        active: '', held: 'On hold', ended: 'Call ended',
    };

    let phone = null;          // last WS `phone` block
    let call = null;           // phone.call
    let minimised = false;     // user tucked the in-call screen into the pill
    let keypadOpen = false;
    let dtmfSent = '';         // digits typed on the in-call keypad (shown, per call)
    let lastId = null;         // call id the view state above belongs to
    let base = { s: 0, at: 0 };  // duration_s at its receipt, for the local timer tick
    let tick = null;
    let busy = false;

    function nameFor(c) {
        if (c.name) return c.name;
        // The dialer's contacts (app.js) may know the number.
        try { if (typeof contactNameFor === 'function') return contactNameFor(c.number) || ''; } catch (e) { /* app.js not ready */ }
        return '';
    }

    function fmtDuration(s) {
        s = Math.max(0, Math.floor(s));
        const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, sec = s % 60;
        const mmss = `${String(m).padStart(h ? 2 : 1, '0')}:${String(sec).padStart(2, '0')}`;
        return h ? `${h}:${mmss}` : mmss;
    }

    function elapsed() {
        if (!call) return 0;
        // Only an established call counts up; "ended" freezes on its last value.
        if (call.state !== 'active' && call.state !== 'held') return base.s;
        return base.s + (performance.now() - base.at) / 1000;
    }

    // ── WS ──────────────────────────────────────────────────────────────────
    function onWs(s) {
        phone = s && s.phone ? s.phone : null;
        const next = phone && phone.call ? phone.call : null;
        const prevState = call ? call.state : null;
        if (next && next.id !== lastId) {
            // A new call: fresh view state. Incoming always takes the screen.
            lastId = next.id;
            minimised = false;
            keypadOpen = false;
            dtmfSent = '';
        }
        if (next && next.state === 'incoming' && prevState !== 'incoming') minimised = false;
        call = next;
        if (call) base = { s: Number(call.duration_s) || 0, at: performance.now() };
        render();
    }

    // ── Rendering ───────────────────────────────────────────────────────────
    function render() {
        const overlay = $('phone-overlay'), pill = $('phone-pill');
        if (!overlay || !pill) return;
        if (!call) {
            overlay.classList.add('hidden');
            pill.classList.add('hidden');
            stopTick();
            return;
        }
        const incoming = call.state === 'incoming';
        const ended = call.state === 'ended';
        const showOverlay = incoming || !minimised;
        overlay.classList.toggle('hidden', !showOverlay);
        pill.classList.toggle('hidden', showOverlay);
        overlay.dataset.state = call.state;
        pill.dataset.state = call.state;

        const name = nameFor(call);
        const number = call.number || 'Unknown number';
        $('phone-name').textContent = name || number;
        $('phone-number').textContent = name ? number : '';
        const initial = (name || '').trim()[0];
        const av = $('phone-avatar');
        av.replaceChildren();
        if (initial) av.textContent = initial.toUpperCase();
        else { const i = document.createElement('i'); i.className = 'fas fa-user'; av.append(i); }

        $('phone-incoming-actions').classList.toggle('hidden', !incoming);
        $('phone-call-actions').classList.toggle('hidden', incoming);
        $('phone-min').classList.toggle('hidden', incoming || ended);
        const kp = keypadOpen && !incoming && !ended;
        $('phone-keypad').classList.toggle('hidden', !kp);
        overlay.classList.toggle('keypad-open', kp);
        $('phone-dtmf').textContent = dtmfSent;
        $('phone-keypad-btn').classList.toggle('on', kp);

        const mute = $('phone-mute');
        mute.classList.toggle('on', !!call.muted);
        mute.querySelector('i').className = call.muted ? 'fas fa-microphone-slash' : 'fas fa-microphone';
        mute.querySelector('span').textContent = call.muted ? 'Muted' : 'Mute';
        for (const b of overlay.querySelectorAll('#phone-call-actions button')) b.disabled = ended;
        // Mute and DTMF only mean something once the other side picked up.
        mute.disabled = ended || (call.state !== 'active' && call.state !== 'held');
        $('phone-keypad-btn').disabled = mute.disabled;

        renderTime();
        if (call.state === 'active' || call.state === 'held') startTick(); else stopTick();
    }

    function renderTime() {
        if (!call) return;
        const label = STATE_LABEL[call.state] ?? '';
        const timed = call.state === 'active' || call.state === 'held' || (call.state === 'ended' && base.s > 0);
        const t = timed ? fmtDuration(elapsed()) : '';
        $('phone-state').textContent = label;
        $('phone-timer').textContent = t;
        // Pill: the timer while talking, else the state.
        const pillText = call.state === 'active' ? t : [label, t].filter(Boolean).join(' · ');
        $('phone-pill-text').textContent = pillText || 'Call';
        $('phone-pill-name').textContent = nameFor(call) || call.number || '';
    }

    function startTick() { if (!tick) tick = setInterval(renderTime, 500); }
    function stopTick() { if (tick) { clearInterval(tick); tick = null; } }

    // ── Actions (all through the backend) ───────────────────────────────────
    async function post(path, body) {
        try {
            const r = await fetch(`/api/phone/${path}`, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body || {}),
            });
            const j = await r.json().catch(() => ({}));
            if (!r.ok || j.ok === false) {
                notify(j.error ? `Phone: ${j.error}` : `Phone: ${path} failed (HTTP ${r.status})`);
                return false;
            }
            return true;
        } catch (e) {
            notify('Phone: the dashboard is unreachable');
            return false;
        }
    }

    // One action at a time: a double tap on Answer must not answer and then
    // hang up a call that moved on underneath.
    async function act(path, body) {
        if (busy) return false;
        busy = true;
        try { return await post(path, body); } finally { busy = false; }
    }

    function answer() { act('answer'); }
    function hangup() { act('hangup'); }
    function toggleMute() { if (call) act('mute', { on: !call.muted }); }
    function toggleKeypad() { keypadOpen = !keypadOpen; render(); }
    function dtmf(d) {
        if (!call || (call.state !== 'active' && call.state !== 'held')) return;
        dtmfSent = (dtmfSent + d).slice(-24);
        $('phone-dtmf').textContent = dtmfSent;
        // Not serialised with `act`: tones must follow fast typing.
        post('dtmf', { digits: d });
    }
    function minimise() { minimised = true; keypadOpen = false; render(); }
    function restore() { minimised = false; render(); }

    /* The dialer's Call button. Returns true when the modem took the call;
       false = no modem path (the caller keeps its tel: fallback). Emergency
       numbers go through exactly the same path: nothing here filters or asks
       for confirmation, and a failure is reported, never swallowed. */
    async function dial(number) {
        if (!available()) return false;
        return post('dial', { number });
    }

    function available() { return !!(phone && phone.available); }
    function hasCall() { return !!call && call.state !== 'ended'; }

    window.RasCar = window.RasCar || {};
    RasCar.phone = { onWs, answer, hangup, toggleMute, toggleKeypad, dtmf, minimise, restore, dial, available, hasCall };
})();

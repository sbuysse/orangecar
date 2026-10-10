/* Fuel wallet: trip cost on the home strip, the refill popup, the Trips fuel card.
 *
 * The backend (app/src/services/fuel.rs, trip.rs) publishes on every WS tick:
 *   s.trip = {distance_km, fuel_l, cost_eur, ...} | null   the open trip
 *   s.fuel = {level_pct, litres_in_tank, balance_eur, price_eur_l,
 *             price_from_refill, pending: {id, level_before, level_after, litres_est} | null}
 * A pending refill (fuel level rose) opens the popup once the car is stopped;
 * the amount paid is POSTed to /api/fuel/refill and credits the wallet.
 */
(function () {
    const SNOOZE_MS = 10 * 60 * 1000;   // "Later" asks again after this
    const MAX_ASK_SPEED_KMH = 5;        // never pop a form up while driving

    const $ = id => document.getElementById(id);
    const money = v => (typeof fmtMoney === 'function' ? fmtMoney(v) : `${v.toFixed(2)} €`);
    const num = v => typeof v === 'number' && isFinite(v);

    let open = null;          // {id|null, amount: '', litres: '', litresEst}
    let field = 'amount';
    let snoozed = {};         // pending id → time it may be asked again
    let lastFuel = null;

    function onWs(s) {
        const f = s.fuel || null;
        lastFuel = f;
        renderHome(s.trip || null, f);
        renderTripsCard(f);
        const p = f && f.pending;
        const stopped = (s.gps?.speed ?? 0) < MAX_ASK_SPEED_KMH;
        if (p && !open && stopped && Date.now() >= (snoozed[p.id] || 0)) openPending(p);
        // Answered elsewhere (another screen, or dismissed): close a stale popup.
        if (open && open.id != null && (!p || p.id !== open.id)) close();
    }

    function renderHome(trip, f) {
        if (trip) {
            setText('hf-trip-cost', money(trip.cost_eur));
            // The route icon says "trip"; whole km from 100 so a long drive still fits.
            const km = trip.distance_km.toFixed(trip.distance_km < 100 ? 1 : 0);
            setText('hf-trip-detail', `${km} km · ${trip.fuel_l.toFixed(1)} L`);
        } else {
            setText('hf-trip-cost', '--');
            setText('hf-trip-detail', 'No trip');
        }
        // Range, not money: what the driver needs at a glance is how far the
        // tank goes. The € balance lives on the Trips card.
        const bal = $('hf-balance');
        if (bal) bal.classList.remove('negative');
        if (f && num(f.range_km)) {
            setText('hf-balance', `${Math.round(f.range_km)} km`);
            if (bal) bal.classList.toggle('negative', f.range_km < 60);
            const lvl = num(f.level_pct) ? `${f.level_pct}%` : '';
            // The pump icon + "km" already say range; the subline must fit ~150 px.
            setText('hf-level', [lvl, `${f.avg_l100.toFixed(1)} L/100`].filter(Boolean).join(' · '));
        } else {
            setText('hf-balance', f && num(f.level_pct) ? `${f.level_pct}%` : '--');
            setText('hf-level', 'Range');
        }
    }

    function renderTripsCard(f) {
        if (!f) return;
        setText('tf-balance', num(f.balance_eur) ? `${money(f.balance_eur)} of fuel left` : 'No fuel entered');
        const level = num(f.level_pct) ? ` · tank ${f.level_pct}%` : '';
        const src = f.price_from_refill ? 'last refill' : 'Settings';
        setText('tf-price', `${f.price_eur_l.toFixed(3)} €/L (${src})${level} · tap to add fuel`);
    }

    function openPending(p) {
        open = { id: p.id, amount: '', litres: num(p.litres_est) && p.litres_est > 0 ? trimNum(p.litres_est) : '', litresEst: p.litres_est, litresTyped: false };
        const range = num(p.level_before) && num(p.level_after)
            ? `Fuel gauge ${Math.round(p.level_before)}% → ${Math.round(p.level_after)}%` : '';
        const est = num(p.litres_est) ? ` · about ${p.litres_est.toFixed(0)} L` : '';
        show('Fuel added — how much did you pay?', range + est, true);
    }

    function openManual() {
        const p = lastFuel && lastFuel.pending;
        if (p) { openPending(p); return; }
        open = { id: null, amount: '', litres: '', litresEst: null, litresTyped: false };
        show('Add fuel', 'Enter what you paid; litres are optional', false);
    }

    function show(title, detail, detected) {
        field = 'amount';
        setText('refill-title', title);
        setText('refill-detail', detail);
        $('refill-dismiss').classList.toggle('hidden', !detected);
        buildPad();
        render();
        $('refill-overlay').classList.remove('hidden');
    }

    function close() {
        open = null;
        $('refill-overlay').classList.add('hidden');
    }

    function buildPad() {
        const pad = $('refill-pad');
        if (pad.childElementCount) return;
        for (const k of ['1', '2', '3', '4', '5', '6', '7', '8', '9', ',', '0', '⌫']) {
            const b = document.createElement('button');
            b.textContent = k;
            if (k === '⌫') b.setAttribute('aria-label', 'Delete');
            b.onclick = () => key(k);
            pad.append(b);
        }
    }

    function key(k) {
        if (!open) return;
        let v = open[field];
        if (k === '⌫') v = v.slice(0, -1);
        else if (k === ',') { if (!v.includes(',')) v = (v || '0') + ','; }
        else {
            const dec = v.split(',')[1];
            if (dec != null && dec.length >= 2) return;      // cents / centilitres at most
            if (v.replace(',', '').length >= 6) return;
            v = v === '0' ? k : v + k;
        }
        open[field] = v;
        if (field === 'litres') open.litresTyped = true;
        render();
    }

    const parse = v => (v ? parseFloat(v.replace(',', '.')) : NaN);
    const trimNum = v => String(Math.round(v * 10) / 10).replace('.', ',');

    function render() {
        if (!open) return;
        setText('refill-amount-val', open.amount || '0');
        setText('refill-litres-val', open.litres || '--');
        $('refill-amount').classList.toggle('active', field === 'amount');
        $('refill-litres').classList.toggle('active', field === 'litres');
        const a = parse(open.amount), l = parse(open.litres);
        setText('refill-price', a > 0 && l > 0 ? `${(a / l).toFixed(3)} €/L` : '');
        $('refill-save').disabled = !(a > 0);
    }

    function focus(which) {
        if (!open) return;
        // Tapping a field clears a pre-filled estimate so it can be typed over.
        if (which === 'litres' && field !== 'litres' && open.litresEst != null && open.litres === trimNum(open.litresEst)) open.litres = '';
        field = which;
        render();
    }

    async function save() {
        if (!open) return;
        const amount = parse(open.amount);
        if (!(amount > 0)) return;
        // Only litres read off the pump are sent: the backend learns the gauge's
        // litres-per-point from them, and the pre-filled estimate would teach it
        // its own guess. Untouched, the server prices from that same estimate.
        const litres = open.litresTyped ? parse(open.litres) : NaN;
        const body = { id: open.id, amount_eur: amount, litres: litres > 0 ? litres : null };
        try {
            const res = await fetch('/api/fuel/refill', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body),
            });
            const out = await res.json().catch(() => ({}));
            if (!res.ok || !out.ok) { notify(`Refill not saved: ${out.error || res.status}`); return; }
            const price = out.refill && num(out.refill.price_eur_l) ? ` · ${out.refill.price_eur_l.toFixed(3)} €/L` : '';
            notify(`Refill saved: ${money(amount)}${price}`);
            close();
        } catch (e) {
            notify('Refill not saved — dashboard unreachable');
        }
    }

    async function dismiss() {
        if (!open || open.id == null) return;
        const id = open.id;
        try {
            const res = await fetch(`/api/fuel/refill/${id}/dismiss`, { method: 'POST' });
            if (!res.ok && res.status !== 404) { notify(`Not dismissed (server said ${res.status})`); return; }
        } catch (e) { notify('Not dismissed — dashboard unreachable'); return; }
        close();
    }

    function later() {
        if (open && open.id != null) snoozed[open.id] = Date.now() + SNOOZE_MS;
        close();
    }

    window.RasCar = window.RasCar || {};
    RasCar.fuel = { onWs, openManual, focus, save, dismiss, later };
})();

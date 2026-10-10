/* 4G panel: data usage, allowance and the 4G controls.
 *
 * Backend: app/src/services/datausage.rs.
 *   GET  /api/cell/usage   → {today, days[30] {key, rx, tx}, cycles[12] {key, end, rx, tx},
 *                             cycle {key, end, rx, tx, start_day, days, days_left, partial,
 *                                    tracked_from, projected},
 *                             allowance {gb, share, remaining, warn, exceeded, cut_at_limit, blocked},
 *                             controls {enabled, want_on, ota_over_4g, logs_over_4g, last_action},
 *                             link {state, dev, ip, since, iface}}
 * "This cycle" is the operator's billing cycle (start_day → day before it next
 * month), not the calendar month; days stay per calendar day.
 *   POST /api/cell/control  {action?: on|off|reconnect, allowance_gb?, cycle_start_day?, cut_at_limit?,
 *                            ota_over_4g?, logs_over_4g?} → {ok, result, usage}
 * Radio details (operator, technology, signal) come from the WS `cell` block
 * (app.js keeps it in `state.cell`). Replaces app.js's one-line showCellInfo().
 */
(function () {
    const POLL_MS = 30000;        // status-bar quota colour; the panel polls faster
    const POLL_OPEN_MS = 10000;   // = the backend's counter period
    const ALLOWANCES = [0, 1, 2, 3, 5, 8, 10, 15, 20, 25, 30, 40, 50, 60, 70, 80, 100, 150, 200];

    const $ = id => document.getElementById(id);
    let usage = null;
    let pollTimer = null;
    let selected = -1;            // tapped bar index
    let busy = false;
    const warnedMem = {};         // fallback when localStorage is unavailable

    function fmtBytes(b) {
        if (!(b > 0)) return '0 MB';
        if (b < 1e9) { const mb = b / 1e6; return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`; }
        const gb = b / 1e9;
        return `${gb < 10 ? gb.toFixed(2) : gb.toFixed(1)} GB`;
    }
    const total = t => (t ? t.rx + t.tx : 0);
    const dayLabel = key => new Date(`${key}T12:00:00`).toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' });
    const shortDate = key => new Date(`${key}T12:00:00`).toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
    const cycleLabel = c => `${shortDate(c.key)} – ${shortDate(c.end)}`;
    const ordinal = n => `${n}${[, 'st', 'nd', 'rd'][(n % 100 >> 3 ^ 1) && n % 10] || 'th'}`;

    async function refresh() {
        try {
            const r = await fetch('/api/cell/usage');
            const j = await r.json();
            if (j.error) return;
            usage = j;
            applyStatusBar();
            if (isOpen()) render();
        } catch (e) { /* the dashboard is unreachable: keep the last figures */ }
    }

    // ── Status bar: amber at 80 %, red at 100 %, "OFF" when 4G is off; toast once ──
    function applyStatusBar() {
        const el = $('sb-cell');
        if (!el || !usage) return;
        const a = usage.allowance || {};
        el.classList.toggle('quota-warn', !!a.warn && !a.exceeded);
        el.classList.toggle('quota-over', !!a.exceeded);
        el.classList.toggle('cell-off', !usage.controls?.want_on);
        if (a.warn) warnOnce(a.exceeded ? 100 : 80, a);
    }

    function warnOnce(level, a) {
        const key = `cell-quota-${usage.cycle.key}-${level}`;
        let seen = warnedMem[key];
        try { seen = seen || localStorage.getItem(key); } catch (e) { /* private mode */ }
        if (seen) return;
        warnedMem[key] = true;
        try { localStorage.setItem(key, '1'); } catch (e) { /* in-memory only */ }
        const used = `${fmtBytes(total(usage.cycle))} of ${a.gb} GB`;
        // A partial cycle only undercounts, so the warning still holds: "at least".
        const least = usage.cycle.partial ? 'at least ' : '';
        notify(level >= 100
            ? `4G allowance used up (${least}${used})${a.blocked ? ` — 4G cut until ${shortDate(nextStart())}` : ''}`
            : `4G data at ${least}${Math.round(a.share * 100)} % of the allowance (${used}, cycle ends ${shortDate(usage.cycle.end)})`);
    }

    function nextStart() {
        const d = new Date(`${usage.cycle.end}T12:00:00`);
        d.setDate(d.getDate() + 1);
        return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
    }

    // ── Panel ──
    const isOpen = () => !$('cell-overlay')?.classList.contains('hidden');

    function open() {
        $('cell-overlay').classList.remove('hidden');
        selected = -1;
        render();
        refresh();
        clearInterval(pollTimer);
        pollTimer = setInterval(refresh, POLL_OPEN_MS);
    }

    function close() {
        $('cell-overlay').classList.add('hidden');
        clearInterval(pollTimer);
        pollTimer = setInterval(refresh, POLL_MS);
    }

    function render() {
        renderStatus();
        if (!usage) return;
        renderUsage();
        renderChart();
        renderMonths();
        renderControls();
    }

    function renderStatus() {
        const c = (typeof state !== 'undefined' && state.cell) || {};
        const link = usage?.link || {};
        const on = usage ? usage.controls.want_on : true;
        $('cell-operator').textContent = c.present ? (c.operator || (c.registered ? 'Unknown operator' : 'No service')) : 'No 4G modem';
        $('cell-tech').textContent = c.registered ? (c.tech || '') : '';
        const bars = c.registered ? (c.bars ?? 0) : 0;
        $('cell-bars').querySelectorAll('.sig-bar').forEach((b, i) => b.classList.toggle('active', i < bars));
        $('cell-dbm').textContent = c.rsrp_dbm != null ? `RSRP ${c.rsrp_dbm} dBm` : (c.rssi_dbm != null ? `RSSI ${c.rssi_dbm} dBm` : 'no signal reading');
        const st = $('cell-state');
        if (!on) {
            st.textContent = usage?.allowance?.blocked ? 'Off — allowance used up' : 'Off';
            st.className = 'cell-state-off';
        } else {
            const s = link.state || (c.present ? 'unknown' : 'no modem');
            st.textContent = s === 'connected' ? (c.internet ? 'Connected' : 'Connected, no internet') : s;
            st.className = '';
        }
        $('cell-ip').textContent = link.ip || '--';
        if (link.since) {
            const t = new Date(link.since * 1000);
            const mins = Math.max(0, Math.round((Date.now() - t) / 60000));
            const dur = mins < 60 ? `${mins} min` : `${Math.floor(mins / 60)} h ${String(mins % 60).padStart(2, '0')}`;
            $('cell-since').textContent = `${t.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} · ${dur}`;
        } else {
            $('cell-since').textContent = '--';
        }
    }

    function renderUsage() {
        $('cell-today').textContent = fmtBytes(total(usage.today));
        const c = usage.cycle, a = usage.allowance;
        $('cell-month').textContent = fmtBytes(total(c));
        const left = c.days_left === 1 ? 'last day' : `${c.days_left} days left`;
        $('cell-cycle').textContent = `${cycleLabel(c)} · ${left}`;
        const meter = $('cell-meter');
        const share = a.share ?? 0;
        meter.firstElementChild.style.width = `${Math.min(100, share * 100).toFixed(1)}%`;
        meter.classList.toggle('warn', !!a.warn && !a.exceeded);
        meter.classList.toggle('over', !!a.exceeded);
        // Tracking began inside this cycle: a share of the allowance would
        // understate the real use, so the meter and percentage stay hidden.
        meter.classList.toggle('hidden', !(a.gb > 0) || c.partial);
        let line;
        if (c.partial) {
            line = `Partial cycle: counted since ${shortDate(c.tracked_from)} only`;
        } else if (a.gb > 0) {
            line = `${fmtBytes(a.remaining)} of ${a.gb} GB left`;
            if (c.projected != null) line += ` · on track for ${fmtBytes(c.projected)}`;
            if (a.blocked) line += ' · 4G cut';
        } else {
            line = c.projected != null ? `No allowance · on track for ${fmtBytes(c.projected)}` : 'No allowance set';
        }
        const allow = $('cell-allow');
        allow.textContent = line;
        allow.classList.toggle('cell-allow-warn', !c.partial && a.gb > 0 && c.projected > a.bytes);
    }

    // ── 30-day stacked bars (download below, upload on top) ──
    function renderChart() {
        const cv = $('cell-chart');
        const rect = cv.getBoundingClientRect();
        if (rect.width < 10 || rect.height < 10) return;
        const dpr = window.devicePixelRatio || 1;
        cv.width = Math.round(rect.width * dpr);
        cv.height = Math.round(rect.height * dpr);
        const ctx = cv.getContext('2d');
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        const css = getComputedStyle(cv);
        const col = n => css.getPropertyValue(n).trim();
        const W = rect.width, H = rect.height;
        const padL = 54, padB = 22, padT = 6;
        const days = usage.days;
        const max = Math.max(1, ...days.map(total));
        // Round the scale to a readable step so the gridline labels are clean.
        const step = niceStep(max / 3);
        const top = step * Math.ceil(max / step);
        const plotH = H - padB - padT, plotW = W - padL;
        const y = v => padT + plotH - (v / top) * plotH;
        ctx.clearRect(0, 0, W, H);
        ctx.font = '12px Manrope, system-ui, sans-serif';
        ctx.textBaseline = 'middle';
        ctx.fillStyle = col('--text-secondary');
        ctx.strokeStyle = col('--border');
        ctx.lineWidth = 1;
        for (let v = 0; v <= top + 1; v += step) {
            const yy = Math.round(y(v)) + 0.5;
            ctx.beginPath(); ctx.moveTo(padL, yy); ctx.lineTo(W, yy); ctx.stroke();
            ctx.textAlign = 'right';
            // Axis ticks are round steps: no trailing zeros ("5 GB", not "5.00 GB").
            ctx.fillText(v === 0 ? "0" : v >= 1e9 ? `${+(v / 1e9).toFixed(2)} GB` : `${Math.round(v / 1e6)} MB`, padL - 8, yy);
        }
        const slot = plotW / days.length;
        const bw = Math.max(3, slot - 4);
        days.forEach((d, i) => {
            const x = padL + i * slot + (slot - bw) / 2;
            const yRx = y(d.rx), yAll = y(d.rx + d.tx);
            const dim = selected >= 0 && selected !== i;
            ctx.globalAlpha = dim ? 0.45 : 1;
            ctx.fillStyle = col('--cell-rx');
            if (d.rx > 0) roundTop(ctx, x, yRx, bw, y(0) - yRx, d.tx > 0 ? 0 : 3);
            ctx.fillStyle = col('--cell-tx');
            // 2px surface gap between the two segments (secondary encoding).
            if (d.tx > 0) roundTop(ctx, x, yAll, bw, Math.max(1, yRx - yAll - (d.rx > 0 ? 2 : 0)), 3);
            ctx.globalAlpha = 1;
            if (i % 7 === (days.length - 1) % 7) {
                ctx.fillStyle = col('--text-secondary');
                ctx.textAlign = 'center';
                const label = shortDate(d.key);
                // Kept inside the canvas: the newest day's label sits at the right edge.
                const half = ctx.measureText(label).width / 2;
                ctx.fillText(label, Math.min(W - half, Math.max(padL + half, x + bw / 2)), H - padB / 2 + 2);
            }
        });
        cv._geom = { padL, slot, n: days.length };
    }

    function roundTop(ctx, x, y, w, h, r) {
        if (h <= 0) return;
        r = Math.min(r, h, w / 2);
        ctx.beginPath();
        ctx.moveTo(x, y + h);
        ctx.lineTo(x, y + r);
        ctx.arcTo(x, y, x + r, y, r);
        ctx.arcTo(x + w, y, x + w, y + r, r);
        ctx.lineTo(x + w, y + h);
        ctx.closePath();
        ctx.fill();
    }

    function niceStep(v) {
        const steps = [1e6, 2e6, 5e6, 1e7, 2e7, 5e7, 1e8, 2e8, 2.5e8, 5e8, 1e9, 2e9, 5e9, 1e10];
        return steps.find(s => s >= v) || 1e10;
    }

    function onChartTap(e) {
        const g = $('cell-chart')._geom;
        if (!g || !usage) return;
        const rect = e.currentTarget.getBoundingClientRect();
        const i = Math.floor((e.clientX - rect.left - g.padL) / g.slot);
        if (i < 0 || i >= g.n) return;
        selected = selected === i ? -1 : i;
        const d = usage.days[i];
        $('cell-readout').textContent = selected < 0 ? 'Tap a bar for the day’s figures'
            : `${dayLabel(d.key)} · ${fmtBytes(total(d))}  (↓ ${fmtBytes(d.rx)}  ↑ ${fmtBytes(d.tx)})`;
        renderChart();
    }

    function renderMonths() {
        const cur = usage.cycle.key;
        $('cell-months').innerHTML = usage.cycles.slice().reverse().map(m => {
            const cls = m.key === cur ? 'current' : (total(m) === 0 ? 'zero' : '');
            return `<tr class="${cls}"><td>${cycleLabel(m)}</td><td>${total(m) ? fmtBytes(total(m)) : '—'}</td></tr>`;
        }).join('');
    }

    function renderControls() {
        const c = usage.controls, a = usage.allowance;
        const pw = $('cell-power');
        pw.classList.toggle('active', c.want_on);
        pw.classList.toggle('off', !c.want_on);
        pw.querySelector('span').textContent = c.want_on ? '4G on' : '4G off';
        $('cell-allowance').textContent = a.gb > 0 ? `${a.gb} GB` : 'None';
        $('cell-startday').textContent = ordinal(usage.cycle.start_day);
        $('cell-cut').classList.toggle('active', !!a.cut_at_limit);
        $('cell-ota').classList.toggle('active', !!c.ota_over_4g);
        $('cell-logs').classList.toggle('active', !!c.logs_over_4g);
        $('cell-action').textContent = c.last_action || '';
    }

    async function control(body, pending) {
        if (busy) return;
        busy = true;
        const panel = document.querySelector('#cell-overlay .cell-controls');
        panel?.classList.add('cell-busy');
        if (pending) $('cell-action').textContent = pending;
        try {
            const r = await fetch('/api/cell/control', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
            const j = await r.json();
            if (j.usage && !j.usage.error) usage = j.usage;
            if (j.result) notify(j.result.detail);
            else if (j.ok === false) notify(j.error || '4G setting not saved');
        } catch (e) {
            notify('4G: dashboard unreachable');
        } finally {
            busy = false;
            panel?.classList.remove('cell-busy');
            applyStatusBar();
            render();
        }
    }

    function power() {
        if (!usage) return;
        const on = usage.controls.want_on;
        control({ action: on ? 'off' : 'on' }, on ? 'Turning 4G off…' : 'Turning 4G on…');
    }
    const reconnect = () => control({ action: 'reconnect' }, 'Reconnecting…');

    function stepAllowance(dir) {
        if (!usage) return;
        const cur = usage.allowance.gb || 0;
        let i = ALLOWANCES.findIndex(v => v >= cur);
        if (i < 0) i = ALLOWANCES.length - 1;
        else if (ALLOWANCES[i] !== cur && dir < 0) i += 1; // between two steps: go to the lower one
        const next = ALLOWANCES[Math.max(0, Math.min(ALLOWANCES.length - 1, i + dir))];
        control({ allowance_gb: next });
    }

    function stepStartDay(dir) {
        if (!usage) return;
        const d = usage.cycle.start_day + dir;
        control({ cycle_start_day: d < 1 ? 28 : (d > 28 ? 1 : d) });
    }

    function toggle(field) {
        if (!usage) return;
        const now = field === 'cut_at_limit' ? usage.allowance.cut_at_limit : usage.controls[field];
        control({ [field]: !now });
    }

    function init() {
        const ov = $('cell-overlay');
        if (!ov) return;
        ov.addEventListener('click', e => { if (e.target === ov) close(); });
        document.addEventListener('keydown', e => { if (e.key === 'Escape' && isOpen()) close(); });
        $('cell-chart').addEventListener('click', onChartTap);
        window.addEventListener('resize', () => { if (isOpen() && usage) renderChart(); });
        refresh();
        pollTimer = setInterval(refresh, POLL_MS);
    }

    window.RasCar = window.RasCar || {};
    RasCar.cell = { open, close, power, reconnect, stepAllowance, stepStartDay, toggle, refresh };
    // The status-bar button (index.html) calls showCellInfo(): open the panel
    // instead of app.js's one-line toast.
    window.showCellInfo = open;
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
    else init();
})();

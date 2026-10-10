/* Home page (page 2) and the arrival popup: the house, through the optional
 * home server (see app/README.md, "Home server").
 *
 * Everything goes through the dashboard's /api/home/* (app/src/home_api.rs),
 * which holds the car's credentials and picks the LAN or the pinned public
 * endpoint. Nothing here talks to the home server directly. When no home server
 * is configured the page is disabled (skipped by swipes, no Apps tile) and no
 * request beyond the one /api/home/status check is made.
 */
(function () {
    const HOME_PAGE = 2;
    const ARM_MS = 4000;           // second tap must follow within this
    const ARRIVAL_AUTOCLOSE_MS = 90000;

    let topology = null;           // server floors → rooms → elements
    let values = {};               // "1.2.3" → latest KNX value
    let persistent = {};           // server key/value store (weather.temp, …)
    let doors = [];                // GarageSnapshot[]
    let level = null;              // index into floors(), null = level list
    let room = null;               // index into that level's rooms, null = room list
    let lastValuesAt = 0;
    let arrivalTimer = null;

    const $ = id => document.getElementById(id);
    const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

    async function api(path, opts = {}) {
        const res = await fetch('/api/home' + path, {
            method: opts.method || 'GET',
            headers: opts.body ? { 'Content-Type': 'application/json' } : undefined,
            body: opts.body ? JSON.stringify(opts.body) : undefined,
        });
        const text = await res.text();
        if (!res.ok) {
            // The server's own reasons (409 interlock, 403 role) are plain text.
            const err = new Error(text || `HTTP ${res.status}`);
            err.status = res.status;
            throw err;
        }
        try { return text ? JSON.parse(text) : null; } catch (e) { return text; }
    }

    function notify(msg) { if (typeof window.notify === 'function') window.notify(msg); }

    function pageVisible() {
        return typeof currentPage !== 'undefined' && currentPage === HOME_PAGE && !document.hidden;
    }

    // --- Model helpers (topology format: see app/README.md) ---
    const lightGa = e => e.groupaddress || e.address;
    const blindGa = e => e.groupaddress_updown || e.groupaddress || e.address;
    const num = ga => (ga && values[ga] !== undefined ? Number(values[ga]) : null);

    function roomControls(room) {
        const rows = [];
        for (const e of room.elements || []) {
            if (e.kind === 'lights' && lightGa(e)) rows.push({ type: 'light', name: e.name, ga: lightGa(e), status: e.groupaddress_dim_status });
            else if (e.kind === 'store' && blindGa(e)) rows.push({ type: 'blind', name: e.name, ga: blindGa(e), stop: e.groupaddress_stop });
            else if (e.kind === 'prises') for (const o of e.outlets || []) if (o.groupaddress) rows.push({ type: 'outlet', name: o.name, ga: o.groupaddress });
        }
        return rows;
    }

    function roomTemperature(room) {
        for (const e of room.elements || []) {
            const t = num(e.temperature);
            if (t !== null && e.kind !== 'thermostat') return t;
        }
        return null;
    }

    function floors() {
        return (topology || []).filter(f => f.kind === 'floor' &&
            (f.elements || []).some(r => r.kind === 'room' && roomControls(r).length));
    }

    // --- Rendering ---
    const roomsOf = fl => (fl.elements || []).filter(r => r.kind === 'room' && roomControls(r).length);
    const lightsOn = rooms => rooms.reduce((n, r) =>
        n + roomControls(r).filter(c => c.type === 'light' && (num(c.status) ?? 0) > 0).length, 0);

    function pickerRow(icon, title, detail, onclick) {
        return `<button class="dialer-row" onclick="${onclick}"><span class="dialer-avatar"><i class="fas ${icon}"></i></span>
            <span class="dialer-row-text"><strong>${esc(title)}</strong><small class="muted">${esc(detail)}</small></span></button>`;
    }

    function plural(n, word) { return `${n} ${word}${n > 1 ? 's' : ''}`; }

    function setCrumb(sub, name) {
        $('home-crumb-sub').textContent = sub;
        $('home-crumb').textContent = name;
        $('home-back').classList.toggle('hidden', level === null);
    }

    function onOff(row) {
        const level = num(row.status);
        const on = level !== null ? level > 0 : null;
        const pct = level !== null && level > 0 && level < 100 ? ` ${Math.round(level)}%` : '';
        return `<div class="media-tabs home-ctl">
            <button class="media-tab${on === true ? ' active' : ''}" onclick="RasCar.home.knx('${row.ga}','1',this)">On${pct}</button>
            <button class="media-tab${on === false ? ' active' : ''}" onclick="RasCar.home.knx('${row.ga}','0',this)">Off</button></div>`;
    }

    function blind(row) {
        // Up is 0, down is 1, and a stop is a 1 on the stop address — the
        // telegrams the wall switch sends.
        return `<div class="media-tabs home-ctl">
            <button class="media-tab" aria-label="Up" onclick="RasCar.home.knx('${row.ga}','0',this)"><i class="fas fa-chevron-up"></i></button>
            ${row.stop ? `<button class="media-tab" aria-label="Stop" onclick="RasCar.home.knx('${row.stop}','1',this)"><i class="fas fa-stop"></i></button>` : ''}
            <button class="media-tab" aria-label="Down" onclick="RasCar.home.knx('${row.ga}','1',this)"><i class="fas fa-chevron-down"></i></button></div>`;
    }

    const ICON = { light: 'fa-lightbulb', blind: 'fa-bars-staggered', outlet: 'fa-plug' };

    // Status strip on the first screen: how the house is, before what to change.
    function statusStrip() {
        const rooms = floors().flatMap(roomsOf);
        const temps = rooms.map(roomTemperature).filter(t => t !== null);
        const inside = temps.length ? `${(temps.reduce((a, b) => a + b, 0) / temps.length).toFixed(1)}°` : '--';
        const out = Number(persistent['weather.temp']);
        const outside = Number.isFinite(out) ? `${out.toFixed(1)}°` : '--';
        let co2 = null;
        for (const r of rooms) for (const e of r.elements || []) {
            const v = num(e.co2);
            if (v !== null && v > 0 && (co2 === null || v > co2)) co2 = v;
        }
        const card = (label, value) => `<div class="trip-stat-card"><span>${esc(label)}</span><strong>${esc(value)}</strong></div>`;
        return `<div class="trip-stats-grid">${card('Inside', inside)}${card('Outside', outside)}${card('Lights on', String(lightsOn(rooms)))}${card('CO₂ max', co2 !== null ? `${Math.round(co2)} ppm` : '--')}</div>`;
    }

    function renderRooms() {
        const box = $('home-rooms');
        const list = floors();
        if (level !== null && !list[level]) { level = null; room = null; }
        const keep = box.scrollTop;
        if (level === null) {
            setCrumb('The house', 'Levels');
            box.innerHTML = list.length ? statusStrip() + `<div class="home-picker">${list.map((fl, i) => {
                const rooms = roomsOf(fl), on = lightsOn(rooms);
                return pickerRow('fa-layer-group', fl.name, plural(rooms.length, 'room') + (on ? ` · ${plural(on, 'light')} on` : ''), `RasCar.home.level(${i})`);
            }).join('')}</div>` : '<div class="list-empty">No controls in the house</div>';
        } else {
            const fl = list[level], rooms = roomsOf(fl);
            if (room !== null && !rooms[room]) room = null;
            if (room === null) {
                setCrumb('Level', fl.name);
                box.innerHTML = `<div class="home-picker">${rooms.map((r, i) => {
                    const t = roomTemperature(r), on = lightsOn([r]);
                    const detail = [t !== null ? `${t.toFixed(1)}°` : null, on ? `${plural(on, 'light')} on` : plural(roomControls(r).length, 'control')].filter(Boolean).join(' · ');
                    return pickerRow('fa-door-open', r.name, detail, `RasCar.home.room(${i})`);
                }).join('')}</div>`;
            } else {
                const r = rooms[room], t = roomTemperature(r);
                setCrumb(fl.name, r.name + (t !== null ? ` · ${t.toFixed(1)}°` : ''));
                box.innerHTML = roomControls(r).map(c =>
                    `<div class="metric-row"><span><i class="fas ${ICON[c.type]} cal-mini-icon"></i>${esc(c.name)}</span>${c.type === 'blind' ? blind(c) : onOff(c)}</div>`).join('');
            }
        }
        box.scrollTop = keep;
    }

    const STATE_LABEL = {
        open: 'Open', closed: 'Closed', opening: 'Opening…', closing: 'Closing…', stopped: 'Stopped',
        intermediate_open: 'Partly open', intermediate_closed: 'Partly open',
    };
    function garageLabel(d) {
        if (!d) return 'Unavailable';
        if (!d.available) return 'Offline';
        // null means the server does not know: never present it as closed.
        return STATE_LABEL[d.state] || 'Unknown';
    }

    function renderGarage() {
        const d = doors[0];
        const tile = $('home-garage');
        if (!tile.classList.contains('armed')) tile.querySelector('small').textContent = garageLabel(d);
        $('home-garage-light').classList.toggle('active', !!(d && d.light));
        const a = $('arrival-garage');
        if (a && !a.dataset.busy) a.querySelector('small').textContent = garageLabel(d);
    }

    // --- Loading ---
    async function refreshStatus() {
        try {
            const st = await api('/status');
            const empty = $('home-empty');
            if (!st.configured) {
                $('home-empty-msg').textContent = 'The house is not configured on this car';
                empty.classList.remove('hidden');
                return false;
            }
            empty.classList.add('hidden');
            $('home-link').textContent = st.via === 'lan' ? 'Connected to the house (home network)'
                : st.via === 'remote' ? 'Connected to the house (over the Internet)'
                : st.error ? `House unreachable — ${st.error}` : 'Connecting to the house…';
            return true;
        } catch (e) {
            $('home-link').textContent = e.status === 403 ? 'Home controls only work on the car screen' : 'Dashboard unreachable';
            return false;
        }
    }

    async function loadStatic() {
        if (topology) return;
        try {
            const t = await api('/topology');
            topology = Array.isArray(t) ? t : [];
            renderRooms();
        } catch (e) {
            topology = null;       // retried on the next tick
        }
    }

    async function refreshGarage() {
        try { doors = await api('/garage') || []; } catch (e) { /* keep the last reading */ }
        renderGarage();
    }

    async function refreshValues(force) {
        // /values rebuilds from the server's event log: every 20 s is plenty, and
        // matters on a metered 4G link.
        if (!force && Date.now() - lastValuesAt < 20000) return;
        lastValuesAt = Date.now();
        try {
            const [v, p] = await Promise.all([api('/values'), api('/persistent').catch(() => persistent)]);
            values = v || {};
            persistent = p || {};
            renderRooms();
        } catch (e) { /* keep the last map */ }
    }

    async function tick() {
        const arrivalOpen = !$('arrival-overlay').classList.contains('hidden');
        if (pageVisible()) {
            if (await refreshStatus()) {
                await loadStatic();
                await Promise.all([refreshGarage(), refreshValues(false)]);
            }
        } else if (arrivalOpen) {
            await refreshGarage();
        }
        setTimeout(tick, 3000);
    }

    // --- Actions ---
    // A tile that opens something on the house needs a second tap: a brush of
    // the screen while driving must not leave the front door open.
    function armed(tile, prompt) {
        if (tile.classList.contains('armed')) {
            tile.classList.remove('armed');
            clearTimeout(tile._armTimer);
            return true;
        }
        tile.classList.add('armed');
        const note = tile.querySelector('small');
        tile._note = note.textContent;
        note.textContent = prompt;
        tile._armTimer = setTimeout(() => { tile.classList.remove('armed'); note.textContent = tile._note; renderGarage(); }, ARM_MS);
        return false;
    }

    async function openDoor(note) {
        note.textContent = 'Opening…';
        try {
            await api('/door/open', { method: 'POST' });
            note.textContent = 'Door released';
            return true;
        } catch (e) {
            note.textContent = 'Failed';
            notify(`Front door: ${e.message}`);
            return false;
        }
    }

    async function garageAction(action, note) {
        const d = doors[0];
        if (!d) { notify('Garage: no door known'); return false; }
        if (note) note.textContent = action === 'open' ? 'Opening…' : action === 'close' ? 'Closing…' : 'Stopping…';
        try {
            const snap = await api(`/garage/${encodeURIComponent(d.id)}/${action}`, { method: 'POST' });
            if (snap && snap.id) doors = [snap];
            renderGarage();
            return true;
        } catch (e) {
            // 409 = the server's interlock (already closed, moving, position unknown).
            notify(`Garage: ${e.message}`);
            if (note) note.textContent = garageLabel(d);
            return false;
        }
    }

    const home = {
        level(i) { level = i; room = null; $('home-rooms').scrollTop = 0; renderRooms(); },
        room(i) { room = i; $('home-rooms').scrollTop = 0; renderRooms(); },
        back() { if (room !== null) room = null; else level = null; $('home-rooms').scrollTop = 0; renderRooms(); },

        async knx(address, value, btn) {
            btn.disabled = true;
            try {
                await api('/knx', { method: 'POST', body: { address, value } });
                // The bus answers with feedback a moment later; read it back.
                setTimeout(() => refreshValues(true), 1500);
            } catch (e) {
                notify(`House: ${e.message}`);
            } finally {
                btn.disabled = false;
            }
        },

        async tapDoor(tile) {
            if (!armed(tile, 'Tap again to open')) return;
            const note = tile.querySelector('small');
            await openDoor(note);
            setTimeout(() => { note.textContent = tile._note; }, 6000);
        },

        async tapGarage(tile) {
            const d = doors[0];
            const closing = d && ['open', 'intermediate_open', 'intermediate_closed', 'stopped'].includes(d.state);
            if (!armed(tile, closing ? 'Tap again to close' : 'Tap again to open')) return;
            await garageAction(closing ? 'close' : 'open', tile.querySelector('small'));
        },

        garage(action) { return garageAction(action, null); },

        async garageLight() {
            const d = doors[0];
            if (!d) return;
            try {
                const snap = await api(`/garage/${encodeURIComponent(d.id)}/light`, { method: 'POST', body: { on: !d.light } });
                if (snap && snap.id) doors = [snap];
                renderGarage();
            } catch (e) { notify(`Garage light: ${e.message}`); }
        },

        // --- Arrival popup ---
        arrival() {
            const ov = $('arrival-overlay');
            ['arrival-door', 'arrival-garage'].forEach(id => { const t = $(id); delete t.dataset.busy; t.querySelector('small').textContent = ''; });
            ov.classList.remove('hidden');
            refreshGarage();
            clearTimeout(arrivalTimer);
            arrivalTimer = setTimeout(home.closeArrival, ARRIVAL_AUTOCLOSE_MS);
        },

        closeArrival() {
            clearTimeout(arrivalTimer);
            $('arrival-overlay').classList.add('hidden');
        },

        // In the popup the choice itself is the confirmation: one tap acts.
        async arrivalDoor(tile) {
            if (tile.dataset.busy) return;
            tile.dataset.busy = '1';
            if (await openDoor(tile.querySelector('small'))) setTimeout(home.closeArrival, 3000);
            delete tile.dataset.busy;
        },

        async arrivalGarage(tile) {
            if (tile.dataset.busy) return;
            tile.dataset.busy = '1';
            if (await garageAction('open', tile.querySelector('small'))) setTimeout(home.closeArrival, 3000);
            delete tile.dataset.busy;
        },
    };

    window.RasCar = window.RasCar || {};
    RasCar.home = home;

    // Feature switch: without a home server the page and its tile disappear.
    (async function detect() {
        let st = null;
        try { st = await api('/status'); } catch (e) { /* not on the car screen: leave it off */ }
        const on = !!(st && st.configured);
        document.querySelector('.page[data-page="' + HOME_PAGE + '"]')?.classList.toggle('page-disabled', !on);
        ['home-app-tile', 'home-nav-tile'].forEach(id => $(id)?.classList.toggle('hidden', !on));
        ['home-door', 'arrival-door'].forEach(id => $(id)?.classList.toggle('hidden', !(st && st.front_door)));
        if (!on) {
            if (typeof currentPage !== 'undefined' && currentPage === HOME_PAGE && typeof goToPage === 'function') goToPage(4);
            return;
        }
        tick();
    })();
})();

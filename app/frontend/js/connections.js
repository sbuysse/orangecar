/* Settings ▸ Wi-Fi and Settings ▸ OBD adapter.
 *
 * Wi-Fi: status, scan, connect to a saved or new network (password typed on
 * the on-screen keyboard), reconnect, forget — through /api/wifi/*
 * (app/src/services/connections.rs → wpa_supplicant).
 * OBD adapter: list the Bluetooth devices around and choose the ELM327. The
 * choice goes to settings.json; the root service rascar-obd-link pairs it
 * and binds /dev/rfcomm0, and reports its progress, polled here.
 */
(function () {
    const OBD_NAME_RE = /OBD|ELM|V-?LINK|VGATE|KONNWEI|VEEPEAK|CARISTA|OBDLINK/i;
    const POLL_MS = 3000;

    let section = null;            // 'wifi' | 'bluetooth' | other
    let pollTimer = null;
    let networks = [];
    let pending = null;            // network waiting for its password
    let password = '';
    let showPassword = false;
    let btDevices = [];
    let obd = {};
    let scanning = false;

    const $ = id => document.getElementById(id);
    const toast = msg => (typeof notify === 'function' ? notify(msg) : console.warn(msg));

    async function api(path, body) {
        const res = await fetch(path, body === undefined ? {} : {
            method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
        });
        let data = {};
        try { data = await res.json(); } catch (_) { /* empty body */ }
        if (!res.ok || data.ok === false) throw new Error(data.error || `HTTP ${res.status}`);
        return data;
    }

    function el(tag, cls, text) {
        const e = document.createElement(tag);
        if (cls) e.className = cls;
        if (text !== undefined) e.textContent = text;
        return e;
    }

    function icon(cls) { return el('i', cls); }

    function button(cls, label, iconCls, onClick) {
        const b = el('button', cls);
        if (iconCls) b.append(icon(iconCls), label ? ` ${label}` : '');
        else b.textContent = label;
        b.addEventListener('click', onClick);
        return b;
    }

    function signalWord(dbm) {
        if (dbm == null || dbm <= -120) return 'out of range';
        if (dbm >= -60) return 'strong';
        if (dbm >= -72) return 'good';
        return 'weak';
    }

    function row(title, detail, actions, cls) {
        const r = el('div', 'setting-row' + (cls ? ` ${cls}` : ''));
        const text = el('div');
        text.append(el('strong', '', title), el('small', 'muted', detail));
        const act = el('div', 'conn-actions');
        act.append(...actions);
        r.append(text, act);
        return r;
    }

    // --- Wi-Fi ---

    async function wifiStatus() {
        try {
            const { status } = await api('/api/wifi');
            const up = status.state === 'COMPLETED';
            $('wifi-state').textContent = up ? `Connected to ${status.ssid}` : stateWord(status.state);
            $('wifi-detail').textContent = up
                ? [status.ip, status.signal_dbm != null ? `${signalWord(status.signal_dbm)} signal (${status.signal_dbm} dBm)` : ''].filter(Boolean).join(' · ')
                : 'Tap Scan to choose a network';
        } catch (e) {
            $('wifi-state').textContent = 'Wi-Fi unavailable';
            $('wifi-detail').textContent = e.message;
        }
    }

    function stateWord(s) {
        return ({
            SCANNING: 'Searching…', ASSOCIATING: 'Connecting…', ASSOCIATED: 'Connecting…',
            AUTHENTICATING: 'Connecting…', '4WAY_HANDSHAKE': 'Checking password…', GROUP_HANDSHAKE: 'Checking password…',
            DISCONNECTED: 'Not connected', INACTIVE: 'Not connected', INTERFACE_DISABLED: 'Wi-Fi off',
        })[s] || s || 'Not connected';
    }

    function renderNetworks() {
        const list = $('wifi-list');
        if (!networks.length) {
            list.replaceChildren(el('p', 'muted conn-empty', 'No networks yet — tap Scan.'));
            return;
        }
        list.replaceChildren(...networks.map(n => {
            const bits = [n.current ? 'connected' : n.saved_id != null ? 'saved' : '', signalWord(n.signal_dbm), n.secure ? 'secured' : 'open'];
            const actions = [];
            if (n.saved_id != null) actions.push(button('btn-danger', 'Forget', null, () => wifiForget(n)));
            if (!n.current && n.signal_dbm > -120) actions.push(button('btn-primary', 'Connect', null, () => wifiConnect(n)));
            return row(n.ssid, bits.filter(Boolean).join(' · '), actions, n.current ? 'conn-current' : '');
        }));
    }

    async function wifiScan() {
        if (scanning) return;
        scanning = true;
        $('wifi-list').replaceChildren(el('p', 'muted conn-empty', 'Scanning…'));
        try {
            networks = (await api('/api/wifi/scan', {})).networks || [];
        } catch (e) {
            toast(`Wi-Fi scan failed: ${e.message}`);
        }
        scanning = false;
        renderNetworks();
    }

    function wifiConnect(n) {
        if (n.saved_id == null && n.secure) {
            pending = n;
            password = '';
            showPassword = false;
            renderPassword();
            $('wifi-list').classList.add('hidden');
            $('wifi-status-row').classList.add('hidden');
            $('wifi-password').classList.remove('hidden');
            return;
        }
        sendConnect(n.ssid, null);
    }

    async function sendConnect(ssid, pw) {
        try {
            await api('/api/wifi/connect', { ssid, password: pw });
            toast(`Connecting to ${ssid}…`);
        } catch (e) {
            toast(`Could not connect: ${e.message}`);
            return;
        }
        cancelPassword();
        // Report the outcome: associated, or the password was refused.
        for (let i = 0; i < 8; i++) {
            await new Promise(r => setTimeout(r, 2000));
            await wifiStatus();
            if ($('wifi-state').textContent === `Connected to ${ssid}`) { wifiScan(); return; }
        }
        toast(`${ssid}: not connected yet — check the password`);
    }

    async function wifiForget(n) {
        try {
            await api('/api/wifi/forget', { id: n.saved_id });
            toast(`${n.ssid} forgotten`);
        } catch (e) {
            toast(`Could not forget: ${e.message}`);
        }
        wifiScan();
    }

    async function wifiRetry() {
        try {
            await api('/api/wifi/retry', {});
            toast('Reconnecting…');
        } catch (e) {
            toast(`Reconnect failed: ${e.message}`);
        }
        wifiStatus();
    }

    function renderPassword() {
        const t = $('wifi-password-text');
        t.textContent = password
            ? (showPassword ? password : '•'.repeat(password.length))
            : `Password for ${pending ? pending.ssid : ''}`;
        t.classList.toggle('muted', !password);
        $('wifi-password-eye').className = showPassword ? 'fas fa-eye-slash' : 'fas fa-eye';
    }

    function togglePasswordVisible() { showPassword = !showPassword; renderPassword(); }

    function cancelPassword() {
        pending = null;
        password = '';
        $('wifi-password').classList.add('hidden');
        $('wifi-list').classList.remove('hidden');
        $('wifi-status-row').classList.remove('hidden');
    }

    function submitPassword() {
        if (!pending) return;
        if (password.length < 8) { toast('A Wi-Fi password has at least 8 characters'); return; }
        sendConnect(pending.ssid, password);
    }

    // --- Bluetooth (OBD adapter) ---

    function linkWord() {
        const link = obd.link || '';
        if (!obd.mac) {
            if (link.startsWith('bound')) return ['Found automatically', `Linked · ${link.slice(6)}`];
            return ['Automatic', 'Looks for a device named OBD / ELM / Vgate… · or choose it below'];
        }
        const name = obd.name || obd.mac;
        if (link === `bound ${obd.mac}`) return [name, obd.bound ? 'Linked · the dashboard reads it' : 'Linked'];
        if (link === `pairing failed ${obd.mac}`) return [name, 'Pairing failed — is the ignition on? Retrying…'];
        if (link === `bind failed ${obd.mac}`) return [name, 'Paired, but the serial link failed — retrying…'];
        return [name, 'Pairing… (PIN 1234 / 0000)'];
    }

    function renderBluetooth() {
        const [title, detail] = linkWord();
        $('bt-obd-state').textContent = title;
        $('bt-obd-detail').textContent = detail;
        $('bt-obd-forget').classList.toggle('hidden', !obd.mac);
        const list = $('bt-list');
        const named = btDevices.filter(d => d.name && d.name !== d.mac.replace(/:/g, '-'));
        if (!named.length) {
            list.replaceChildren(el('p', 'muted conn-empty', scanning ? 'Scanning… (10 s)' : 'Turn the ignition on so the adapter is powered, then tap Scan.'));
            return;
        }
        // Likely adapters first.
        named.sort((a, b) => OBD_NAME_RE.test(b.name) - OBD_NAME_RE.test(a.name));
        list.replaceChildren(...named.map(d => {
            const chosen = obd.mac === d.mac;
            const bits = [OBD_NAME_RE.test(d.name) ? 'looks like an OBD adapter' : '', d.paired ? 'paired' : '',
                d.rssi != null ? `${signalWord(d.rssi)} signal` : '', d.mac];
            const actions = chosen ? [el('span', 'setting-value', 'In use')]
                : [button('btn-primary', 'Use', null, () => obdChoose(d))];
            return row(d.name, bits.filter(Boolean).join(' · '), actions, chosen ? 'conn-current' : '');
        }));
    }

    async function btRefresh() {
        try {
            const data = await api('/api/bluetooth');
            btDevices = data.devices || [];
            obd = data.obd || {};
        } catch (e) {
            $('bt-obd-state').textContent = 'Bluetooth unavailable';
            $('bt-obd-detail').textContent = e.message;
            return;
        }
        renderBluetooth();
    }

    async function btScan() {
        if (scanning) return;
        scanning = true;
        $('bt-scan').disabled = true;
        renderBluetooth();
        try {
            const data = await api('/api/bluetooth/scan', {});
            btDevices = data.devices || [];
            obd = data.obd || {};
        } catch (e) {
            toast(`Bluetooth scan failed: ${e.message}`);
        }
        scanning = false;
        $('bt-scan').disabled = false;
        renderBluetooth();
    }

    async function obdChoose(d) {
        try {
            await api('/api/bluetooth/obd', { mac: d.mac, name: d.name });
            toast(`${d.name} will be used as the OBD adapter`);
        } catch (e) {
            toast(`Could not save: ${e.message}`);
        }
        btRefresh();
    }

    async function obdForget() {
        try {
            await api('/api/bluetooth/obd', { mac: null });
            toast('OBD adapter: automatic discovery');
        } catch (e) {
            toast(`Could not save: ${e.message}`);
        }
        btRefresh();
    }

    // --- Section lifecycle ---

    function stopPolling() { clearInterval(pollTimer); pollTimer = null; }

    function onSection(name) {
        section = name;
        stopPolling();
        if (name === 'wifi') {
            wifiStatus();
            if (!networks.length) wifiScan();
            pollTimer = setInterval(() => { if (isOpen()) wifiStatus(); else stopPolling(); }, POLL_MS);
        } else if (name === 'bluetooth') {
            btRefresh();
            pollTimer = setInterval(() => { if (isOpen() && !scanning) btRefresh(); else if (!isOpen()) stopPolling(); }, POLL_MS);
        }
    }

    function isOpen() {
        const o = $('settings-overlay');
        return o && !o.classList.contains('hidden') && section && document.querySelector(`.settings-section[data-section="${section}"]:not(.hidden)`);
    }

    document.addEventListener('DOMContentLoaded', () => {
        buildKeyboard($('wifi-osk'), {
            onKey: c => { if (password.length < 63) { password += c; renderPassword(); } },
            onBackspace: () => { password = password.slice(0, -1); renderPassword(); },
            onGo: submitPassword, goLabel: 'Connect', goIcon: 'fa-wifi', password: true,
        });
    });

    window.RasCar = window.RasCar || {};
    RasCar.connections = {
        onSection, wifiScan, wifiRetry, togglePasswordVisible, cancelPassword, btScan, obdForget,
    };
})();

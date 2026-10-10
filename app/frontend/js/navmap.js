/**
 * RasCar offline navigation map — MapLibre GL on the GPU (the kiosk runs on a
 * Wayland compositor with WebGL since 2026-09-30), vector tiles from a local
 * PMTiles archive served by rascar-app at /maps (a regional extract,
 * Protomaps/OpenStreetMap), no network needed.
 *
 * Follow mode mirrors OsmAnd's car behaviour:
 *  - heading-up: the map turns with the GPS course (kept while stopped, where
 *    the course is noise);
 *  - the car sits low on the screen (≈85 % of the height), so the road ahead
 *    fills the view;
 *  - auto-zoom shows about 45 s of driving ahead (at least 200 m), changing at
 *    most 0.1 zoom level per second so the map never "breathes";
 *  - each 1 Hz fix is reached by a LINEAR move over the time since the last
 *    one, so the car glides instead of jumping;
 *  - touching the map releases the camera; it relinks 15 s after the last touch
 *    (or at once with the recenter button).
 * Routing and turn-by-turn: Valhalla on the Pi (services/navigation.rs).
 *
 * Optional layers (buttons top-right, remembered):
 *  - live traffic everywhere: Google Maps' own traffic tiles, fetched
 *    straight from Google by the kiosk, refreshed every TRAFFIC_TILES_MS while
 *    shown (off by default; check Google's terms before using them over a
 *    non-Google map);
 *  - 3D buildings from zoom 15 (Protomaps `buildings` heights where OSM has
 *    them, BUILDING_DEFAULT_M otherwise).
 * The "time to home" chip sits in the nav panel on every source (Google,
 * Waze, offline): /api/nav/home-eta, cached server-side.
 */
(function () {
    const MAPS = `${location.origin}/maps`;
    const ARCHIVE = 'region.pmtiles';
    const RELINK_MS = 15000;
    const AHEAD_S = 45;
    const AHEAD_MIN_M = 200;
    const ZOOM_MIN = 12.5, ZOOM_MAX = 17;
    const ZOOM_RATE = 0.1;           // zoom levels per second
    const PITCH = 45;
    const CAR_Y = 0.85;              // car position, fraction of the height from the top
    const MOVING_KMH = 5;            // below this the GPS course is noise
    // Google traffic tiles: roads with traffic drawn, everything else
    // transparent. `apistyle=p.v:off` hides Google's base map and labels but
    // not the traffic layer (lyrs=traffic alone answers 1x1 empty PNGs).
    // CORS is open (Access-Control-Allow-Origin: *); a request without a
    // User-Agent is refused (403 "automated queries"), the kiosk sends one.
    const TRAFFIC_TILE_URL = 'https://mt{s}.google.com/vt?lyrs=h,traffic&x={x}&y={y}&z={z}&apistyle=p.v%3Aoff';
    const TRAFFIC_TILES_MS = 150000; // Google's own refresh is ~1-2 min (max-age=60)
    const BUILDINGS_MIN_ZOOM = 15;
    const BUILDING_DEFAULT_M = 7;    // no height in OSM (most houses): two storeys
    const DEFAULT_CENTER = [10, 50]; // [lon, lat] shown before the first fix
    const DEFAULT_ZOOM = 4;

    let map = null;
    let ready = false;
    let available = null;            // null = unknown, then true/false
    let following = true;
    let lastTouch = 0;
    let lastFix = null;              // {lon, lat, speed, bearing, at}
    let bearing = 0;
    let zoom = 15.5;
    let relinkTimer = null;
    // Diagnostics for the kiosk log (the map froze in the car on 2026-10-01
    // with the position flowing and nothing recorded to say why).
    const diag = { fixes: 0, ignored: {}, placed: 0, lastPlacedAt: 0 };
    const klog = (m) => window.RasCar?.kioskLog?.(`navmap: ${m}`);

    function mapEl() { return document.getElementById('nav-offline-map'); }
    function visible() {
        const el = mapEl();
        const onNavPage = typeof currentPage === 'undefined' || currentPage === 3;
        return el && onNavPage && !el.classList.contains('hidden') && !document.hidden;
    }

    async function checkAvailable() {
        if (available !== null) return available;
        try {
            const r = await fetch(`${MAPS}/${ARCHIVE}`, { method: 'HEAD' });
            available = r.ok;
        } catch (e) { available = false; }
        return available;
    }

    function pref(key, dflt) {
        try { const v = localStorage.getItem(key); return v === null ? dflt : v === 'on'; } catch (e) { return dflt; }
    }
    let trafficOn = pref('rascar-nav-traffic', false); // opt-in: Google traffic tiles
    let buildingsOn = pref('rascar-nav-3d', true);

    // `traffic.{0..3}` URLs for the current refresh bucket: changing the URL
    // makes MapLibre fetch every visible tile again.
    function trafficTiles() {
        const bucket = Math.floor(Date.now() / TRAFFIC_TILES_MS);
        const scale = (window.devicePixelRatio || 1) >= 1.5 ? '&scale=2' : '';
        return [0, 1, 2, 3].map((n) => `${TRAFFIC_TILE_URL.replace('{s}', n)}${scale}&t=${bucket}`);
    }

    // Base map split so our layers can go between the ground and the labels.
    function baseLayers() {
        const all = basemaps.layers('protomaps', roadFlavor(), { lang: 'fr' });
        const firstLabel = all.findIndex((l) => l.type === 'symbol');
        const cut = firstLabel < 0 ? all.length : firstLabel;
        return { ground: all.slice(0, cut), labels: all.slice(cut) };
    }

    // Extruded buildings, in a muted theme tone. Layer-wide opacity only (the
    // GPU cost grows with what is drawn, so they start at zoom 15).
    function buildingsLayer() {
        return {
            id: 'buildings-3d', type: 'fill-extrusion', source: 'protomaps', 'source-layer': 'buildings',
            minzoom: BUILDINGS_MIN_ZOOM,
            filter: ['in', ['get', 'kind'], ['literal', ['building', 'building_part']]],
            layout: { visibility: buildingsOn ? 'visible' : 'none' },
            paint: {
                'fill-extrusion-color': mixColor('--text-secondary', '--bg-primary', 0.35),
                'fill-extrusion-height': ['interpolate', ['linear'], ['zoom'],
                    BUILDINGS_MIN_ZOOM, 0, BUILDINGS_MIN_ZOOM + 0.5, ['coalesce', ['get', 'height'], BUILDING_DEFAULT_M]],
                'fill-extrusion-base': ['coalesce', ['get', 'min_height'], 0],
                'fill-extrusion-opacity': 0.75,
                'fill-extrusion-vertical-gradient': true,
            },
        };
    }

    function style() {
        const { ground, labels } = baseLayers();
        return {
            version: 8,
            glyphs: `${MAPS}/fonts/{fontstack}/{range}.pbf`,
            sprite: `${MAPS}/sprites/v4/dark`,
            sources: {
                protomaps: {
                    type: 'vector',
                    url: `pmtiles://${MAPS}/${ARCHIVE}`,
                    attribution: '© OpenStreetMap · Protomaps',
                },
                car: { type: 'geojson', data: carFeature(0, 0) },
                route: { type: 'geojson', data: { type: 'FeatureCollection', features: [] } },
                traffic: { type: 'geojson', data: { type: 'FeatureCollection', features: [] } },
                gtraffic: {
                    type: 'raster', tiles: trafficTiles(), tileSize: 256, minzoom: 6, maxzoom: 19,
                    attribution: 'Trafic © Google',
                },
            },
            layers: [
                ...ground,
                // Google traffic on the roads, under the buildings, labels and route.
                { id: 'gtraffic', type: 'raster', source: 'gtraffic',
                  layout: { visibility: trafficOn ? 'visible' : 'none' },
                  paint: { 'raster-opacity': 0.85, 'raster-fade-duration': 0 } },
                buildingsLayer(),
                ...labels,
                // The route, under the car: theme accent on a background-coloured casing.
                { id: 'route-casing', type: 'line', source: 'route', layout: { 'line-join': 'round', 'line-cap': 'round' },
                  paint: { 'line-color': themeColor('--bg-primary'), 'line-width': ['interpolate', ['linear'], ['zoom'], 10, 7, 16, 18] } },
                { id: 'route', type: 'line', source: 'route', layout: { 'line-join': 'round', 'line-cap': 'round' },
                  paint: { 'line-color': themeColor('--accent'), 'line-width': ['interpolate', ['linear'], ['zoom'], 10, 4, 16, 12] } },
                // Live traffic (Google): slow = warning, jam = danger, over the route line.
                { id: 'traffic', type: 'line', source: 'traffic', layout: { 'line-join': 'round', 'line-cap': 'round' },
                  paint: {
                      'line-color': ['match', ['get', 'speed'], 'TRAFFIC_JAM', themeColor('--danger'), themeColor('--warning')],
                      'line-width': ['interpolate', ['linear'], ['zoom'], 10, 4, 16, 12],
                  } },
                {
                    id: 'car', type: 'symbol', source: 'car',
                    layout: {
                        'icon-image': 'car-arrow',
                        'icon-size': 1.6,
                        'icon-rotate': ['get', 'bearing'],
                        'icon-rotation-alignment': 'map',
                        // Upright, not flattened into the pitched road.
                        'icon-pitch-alignment': 'viewport',
                        'icon-allow-overlap': true,
                        'icon-ignore-placement': true,
                    },
                },
            ],
        };
    }

    // Protomaps' dark flavour draws roads barely lighter than the land: fine
    // for a background map, too faint to read at a glance while driving. Roads
    // take the theme's secondary text tone, stronger for bigger roads.
    function roadFlavor() {
        const flavor = { ...basemaps.namedFlavor('dark') };
        const tone = getComputedStyle(document.documentElement).getPropertyValue('--text-secondary').trim();
        const m = /^#([0-9a-f]{6})$/i.exec(tone);
        if (!m) return flavor;
        const n = parseInt(m[1], 16);
        const rgba = (a) => `rgba(${n >> 16}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
        Object.assign(flavor, {
            highway: rgba(0.95), major: rgba(0.8), link: rgba(0.8),
            minor_a: rgba(0.55), minor_b: rgba(0.45), minor_service: rgba(0.3),
        });
        return flavor;
    }

    function themeColor(token) {
        return getComputedStyle(document.documentElement).getPropertyValue(token).trim();
    }

    // `frac` of token a over token b (both #rrggbb), as rgb(); a's own value
    // when either is not a plain hex colour.
    function mixColor(a, b, frac) {
        const ca = /^#([0-9a-f]{6})$/i.exec(themeColor(a)), cb = /^#([0-9a-f]{6})$/i.exec(themeColor(b));
        if (!ca || !cb) return themeColor(a) || '#555';
        const na = parseInt(ca[1], 16), nb = parseInt(cb[1], 16);
        const ch = (sh) => Math.round(((na >> sh) & 255) * frac + ((nb >> sh) & 255) * (1 - frac));
        return `rgb(${ch(16)}, ${ch(8)}, ${ch(0)})`;
    }

    function carFeature(lon, lat) {
        return { type: 'FeatureCollection', features: [{ type: 'Feature', properties: { bearing }, geometry: { type: 'Point', coordinates: [lon, lat] } }] };
    }

    // The car arrow, drawn in the theme's accent colour (no colour defined here).
    function carImage() {
        const size = 64, c = document.createElement('canvas');
        c.width = c.height = size;
        const x = c.getContext('2d');
        const css = getComputedStyle(document.documentElement);
        const accent = css.getPropertyValue('--accent').trim();
        const edge = css.getPropertyValue('--bg-primary').trim();
        x.beginPath();
        x.moveTo(32, 6); x.lineTo(54, 56); x.lineTo(32, 44); x.lineTo(10, 56); x.closePath();
        x.fillStyle = accent; x.fill();
        x.lineWidth = 4; x.strokeStyle = edge; x.stroke();
        return x.getImageData(0, 0, size, size);
    }

    function padding() {
        const h = mapEl()?.clientHeight || 512;
        // The map centre sits halfway between the top padding and the bottom:
        // (h + top) / 2 = CAR_Y * h.
        return { top: Math.round((2 * CAR_Y - 1) * h), bottom: 0, left: 0, right: 0 };
    }

    // Zoom showing AHEAD_S of driving between the car and the top of the view.
    function targetZoom(lat, kmh) {
        const h = mapEl()?.clientHeight || 512;
        const aheadM = Math.max(AHEAD_MIN_M, (kmh / 3.6) * AHEAD_S);
        const aheadPx = CAR_Y * h;
        const mpp = aheadM / aheadPx;                              // metres per screen pixel wanted
        const flat = Math.log2((40075016.686 * Math.cos(lat * Math.PI / 180)) / (512 * mpp));
        // Pitched, the top of the view reaches roughly twice as far.
        return Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, flat + 1));
    }

    function init() {
        if (map || !window.maplibregl || !window.pmtiles || !window.basemaps) return;
        const protocol = new pmtiles.Protocol();
        maplibregl.addProtocol('pmtiles', protocol.tile);
        // No fix yet: a whole-Europe view until the first position arrives.
        const start = lastFix ? [lastFix.lon, lastFix.lat] : DEFAULT_CENTER;
        map = new maplibregl.Map({
            container: mapEl(),
            style: style(),
            center: start,
            zoom: lastFix ? zoom : DEFAULT_ZOOM,
            pitch: PITCH,
            bearing,
            // Added top-left below: bottom-right, the expanded attribution
            // covered the end of the nav panel (route card, home chip).
            attributionControl: false,
            dragRotate: false,
            pitchWithRotate: false,
            fadeDuration: 0,
        });
        map.touchZoomRotate.disableRotation();
        map.addControl(new maplibregl.AttributionControl({ compact: true }), 'top-left');
        klog(`init at ${start[1].toFixed(5)},${start[0].toFixed(5)}`);
        map.on('error', (e) => {
            // Google traffic tiles fail as a group when the car is offline:
            // count them, do not log each tile.
            if (e.sourceId === 'gtraffic') { diag.trafficErrors = (diag.trafficErrors || 0) + 1; return; }
            klog(`map error: ${e.error?.message || e.error || e}`);
        });
        map.getCanvas().addEventListener('webglcontextlost', () => klog('WebGL context LOST'));
        map.getCanvas().addEventListener('webglcontextrestored', () => klog('WebGL context restored'));
        map.on('load', () => {
            map.addImage('car-arrow', carImage(), { pixelRatio: 2 });
            ready = true;
            refreshLayerButtons();
            klog('style loaded, ready');
            if (lastFix) place(lastFix, 0);
        });
        // A touch releases the camera; programmatic moves have no originalEvent.
        const release = (e) => { if (!e.originalEvent) return; following = false; lastTouch = Date.now(); scheduleRelink(); refreshRecenter(); };
        map.on('dragstart', release);
        map.on('zoomstart', release);
    }

    function scheduleRelink() {
        clearTimeout(relinkTimer);
        relinkTimer = setTimeout(() => { if (Date.now() - lastTouch >= RELINK_MS) recenter(); }, RELINK_MS + 50);
    }

    function refreshRecenter() {
        document.getElementById('nav-recenter')?.classList.toggle('hidden', following || !visible());
    }

    function recenter() {
        following = true;
        refreshRecenter();
        if (lastFix) place(lastFix, 600);
    }

    // Move the car marker and (when following) the camera to a fix.
    function place(fix, duration) {
        if (!map || !ready) return;
        diag.placed++; diag.lastPlacedAt = performance.now();
        map.getSource('car')?.setData(carFeature(fix.lon, fix.lat));
        if (!following) return;
        map.easeTo({
            center: [fix.lon, fix.lat],
            bearing,
            zoom,
            pitch: PITCH,
            padding: padding(),
            duration,
            easing: (t) => t,           // linear: constant speed between fixes
        });
    }

    // Called with state.gps once per second (app.js).
    function onGps(g) {
        const why = !g ? 'none' : g.fix === false ? 'nofix' : g.mock ? 'mock' : !(g.lat || g.lon) ? 'zero' : '';
        if (why) { diag.ignored[why] = (diag.ignored[why] || 0) + 1; return; }
        diag.fixes++;
        const now = performance.now();
        const kmh = g.speed || 0;
        if (kmh >= MOVING_KMH && Number.isFinite(g.bearing)) bearing = g.bearing;
        const dt = lastFix ? Math.min(2000, now - lastFix.at) : 0;
        const want = targetZoom(g.lat, kmh);
        const step = ZOOM_RATE * (dt / 1000 || 1);
        zoom += Math.max(-step, Math.min(step, want - zoom));
        const first = !lastFix;
        lastFix = { lon: g.lon, lat: g.lat, speed: kmh, bearing, at: now };
        if (first) pollHome(true);
        if (route) guide(lastFix);      // guidance runs even off screen (reroute, arrival)
        if (!visible()) return;         // hidden: no GPU work, catch up on show
        place(lastFix, dt || 0);
    }

    // ── Destination search ───────────────────────────────────────
    let query = '';
    let searchTimer = null;
    let keyboardBuilt = false;

    function openSearch() {
        document.getElementById('navmap-search')?.classList.remove('hidden');
        const osk = document.getElementById('navmap-osk');
        if (osk && !keyboardBuilt && typeof buildKeyboard === 'function') {
            buildKeyboard(osk, {
                onKey: (c) => { query += c; typed(); },
                onBackspace: () => { query = query.slice(0, -1); typed(); },
                onGo: () => { const first = document.querySelector('#navmap-results .ytm-track'); if (first) first.click(); },
                goLabel: 'Go', goIcon: 'fa-arrow-right',
            });
            keyboardBuilt = true;
        }
        osk?.classList.remove('hidden');
        typed();
    }

    // Favourite destinations: settings.json `nav_favorites` (hand-edited, see
    // app/README.md), [{name, address, icon?, lat, lon}]. Coordinates for the
    // offline router; online, Google Maps / Waze get the address text and
    // geocode it themselves.
    function favourites() {
        const list = typeof currentSettings !== 'undefined' && Array.isArray(currentSettings.nav_favorites)
            ? currentSettings.nav_favorites : [];
        return list.filter(f => f && f.name && Number.isFinite(f.lat) && Number.isFinite(f.lon));
    }

    function driveTo(f) {
        closeSearch();
        const offline = typeof navSource === 'undefined' || navSource === 'offline';
        if (offline) { startRoute({ name: f.name, detail: f.address, lat: f.lat, lon: f.lon }); return; }
        if (typeof navigateTo === 'function') navigateTo(f.address);
    }

    // The search panel without keyboard, listing the favourites.
    function openFavourites() {
        clearTimeout(searchTimer);
        document.getElementById('navmap-osk')?.classList.add('hidden');
        document.getElementById('navmap-search')?.classList.remove('hidden');
        setText('navmap-query', 'Favourites');
        const favs = favourites();
        document.getElementById('navmap-results')?.replaceChildren(...(favs.length
            ? favs.map((f) => row(f.icon || 'fa-star', f.name, f.address || '', () => driveTo(f)))
            : [row('fa-circle-info', 'No favourites yet', 'Add nav_favorites to settings.json', () => {})]));
    }

    function closeSearch() {
        document.getElementById('navmap-search')?.classList.add('hidden');
        document.getElementById('navmap-osk')?.classList.add('hidden');
    }

    function typed() {
        const q = document.getElementById('navmap-query');
        if (q) q.textContent = query || 'Destination: town, street, number, place…';
        clearTimeout(searchTimer);
        searchTimer = setTimeout(search, 250);
    }

    const KIND_ICON = { place: 'fa-city', street: 'fa-road', address: 'fa-house', poi: 'fa-location-dot' };

    function row(icon, title, sub, onTap, action) {
        const b = document.createElement('button');
        b.className = 'ytm-track';
        const i = document.createElement('i');
        i.className = `fas ${icon}`;
        const text = document.createElement('span');
        text.className = 'ytm-track-text';
        const strong = document.createElement('strong');
        strong.textContent = title;
        const small = document.createElement('small');
        small.textContent = sub;
        text.append(strong, small);
        b.append(i, text);
        if (action) {
            const a = document.createElement('span');
            a.className = 'navmap-row-action';
            a.setAttribute('role', 'button');
            a.setAttribute('aria-label', action.label);
            const ai = document.createElement('i');
            ai.className = `fas ${action.icon}`;
            a.append(ai);
            a.addEventListener('click', (e) => { e.stopPropagation(); action.onTap(); });
            b.append(a);
        }
        b.addEventListener('click', onTap);
        return b;
    }

    // "Set as home" on a search result or recent destination.
    const setHomeAction = (p) => ({
        icon: 'fa-house', label: 'Set as home',
        onTap: () => setHome({ lat: p.lat, lon: p.lon, label: p.name }),
    });

    // Before anything is typed: the next calendar event with a place, then the
    // recent destinations (remembered on this screen).
    const RECENT_KEY = 'rascar-nav-recent';
    function recents() {
        try { return JSON.parse(localStorage.getItem(RECENT_KEY) || '[]'); } catch (e) { return []; }
    }
    function remember(p) {
        const list = [p, ...recents().filter((r) => r.name !== p.name || r.detail !== p.detail)].slice(0, 8);
        try { localStorage.setItem(RECENT_KEY, JSON.stringify(list)); } catch (e) { /* not remembered */ }
    }
    function suggestions() {
        const out = [];
        if (home.place) {
            const p = home.place;
            const sub = home.eta && !home.eta.at_home ? `${Math.round(home.eta.seconds / 60)} min` : 'Home';
            out.push(row('fa-house', p.label || 'Home', sub, () => { closeSearch(); startRoute({ name: p.label || 'Home', detail: 'Home', lat: p.lat, lon: p.lon }); }));
        }
        const ev = typeof nextEventLocation !== 'undefined' ? nextEventLocation : '';
        if (ev) {
            out.push(row('fa-calendar-day', ev, 'Next calendar event', async () => {
                const first = (await lookup(ev))[0];
                if (!first) { window.RasCar?.media?.toast?.('Event place not found in the offline map'); return; }
                closeSearch(); startRoute(first);
            }));
        }
        for (const p of recents()) {
            out.push(row('fa-clock-rotate-left', p.name, p.detail || '', () => { closeSearch(); startRoute(p); }, setHomeAction(p)));
        }
        return out;
    }

    async function lookup(q) {
        const params = new URLSearchParams({ q });
        if (lastFix) { params.set('lat', lastFix.lat); params.set('lon', lastFix.lon); }
        const r = await fetch(`/api/geo/search?${params}`);
        const j = await r.json();
        if (!r.ok) throw new Error(j.error || r.status);
        return j.places || [];
    }

    async function search() {
        const list = document.getElementById('navmap-results');
        if (!list) return;
        if (query.trim().length < 2) { list.replaceChildren(...suggestions()); return; }
        let places = [];
        try {
            places = await lookup(query);
        } catch (e) {
            window.RasCar?.media?.toast?.('Destination search unavailable on this car');
            return;
        }
        list.replaceChildren(...places.map((p) => row(
            KIND_ICON[p.kind] || 'fa-location-dot', p.name,
            [p.detail, p.km != null ? `${p.km} km` : ''].filter(Boolean).join(' · '),
            () => { closeSearch(); remember(p); startRoute(p); },
            setHomeAction(p),
        )));
    }

    // ── Route + guidance ─────────────────────────────────────────
    const OFF_ROUTE_M = 60;       // further than this from the line...
    const OFF_ROUTE_FIXES = 3;    // ...for this many fixes in a row → reroute
    const REROUTE_MIN_MS = 10000; // and never more often than this
    const ARRIVED_M = 35;
    let route = null;             // { shape, maneuvers, limits, km, seconds, dest, idx, offCount, lastReroute, said }

    // ── Live traffic (Google Routes via rascar-app, refreshed while guiding) ──
    const TRAFFIC_EVERY_MS = 5 * 60 * 1000;
    const DELAY_SAY_S = 5 * 60;   // announce when the delay grows by this much
    let traffic = null;           // { seconds, free_seconds, km, jams, at, delaySaid }
    let trafficTimer = null;

    const FASTER_MIN_S = 5 * 60;  // switch only for a real gain...
    const FASTER_MIN_FRAC = 0.1;  // ...and at least 10 % of the trip
    const OTHER_ROAD_M = 150;     // a Google point further than this from our line is another road
    const OTHER_ROAD_FRAC = 0.2;
    const SWITCH_EVERY_MS = 10 * 60 * 1000; // never flip-flop between routes
    let lastSwitch = 0;

    // n points evenly spread (by distance) along line[from..], ends excluded.
    function samplePath(line, from, n) {
        const pts = line.slice(from);
        if (pts.length < 3) return [];
        const cum = [0];
        for (let i = 1; i < pts.length; i++) cum.push(cum[i - 1] + metres(pts[i - 1], pts[i]));
        const total = cum[cum.length - 1];
        const out = [];
        for (let k = 1; k <= n; k++) {
            const d = total * k / (n + 1);
            const i = cum.findIndex((c) => c >= d);
            if (i > 0) out.push(pts[i]);
        }
        return out;
    }

    // Share of Google's line that is not on our remaining route.
    function offOurRoad(line, r) {
        const ours = r.shape.slice(r.idx);
        const probe = samplePath(line, 0, 40);
        if (!probe.length || ours.length < 2) return 0;
        let off = 0;
        for (const p of probe) {
            let best = Infinity;
            for (let i = 0; i + 1 < ours.length && best > OTHER_ROAD_M; i++) best = Math.min(best, segDist(p, ours[i], ours[i + 1]));
            if (best > OTHER_ROAD_M) off++;
        }
        return off / probe.length;
    }

    async function trafficFor(r, via) {
        const resp = await fetch('/api/traffic', {
            method: 'POST', headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ from: [lastFix.lon, lastFix.lat], to: [r.dest.lon, r.dest.lat], via }),
        });
        if (!resp.ok) return null;    // no key / no network: no traffic, nothing to show
        return resp.json();
    }

    async function fetchTraffic() {
        const r = route;
        if (!r || !lastFix) return;
        try {
            // Two answers: the traffic time along OUR route (Google through
            // points sampled from it) and Google's own best route.
            const [t, best] = await Promise.all([trafficFor(r, samplePath(r.shape, r.idx, 8)), trafficFor(r, [])]);
            if (!t || route !== r) return;
            if (best && Date.now() - lastSwitch > SWITCH_EVERY_MS
                && best.seconds + Math.max(FASTER_MIN_S, FASTER_MIN_FRAC * t.seconds) <= t.seconds
                && offOurRoad(best.line, r) >= OTHER_ROAD_FRAC) {
                lastSwitch = Date.now();
                const gain = Math.round((t.seconds - best.seconds) / 60);
                say(`Itinéraire plus rapide trouvé : ${gain} minutes gagnées.`);
                window.RasCar?.media?.toast?.(`Itinéraire plus rapide : −${gain} min`);
                startRoute(r.dest, true, samplePath(best.line, 0, 6));
                return;
            }
            const delay = Math.max(0, t.seconds - t.free_seconds);
            const prevDelay = traffic ? Math.max(0, traffic.seconds - traffic.free_seconds) : 0;
            if (traffic && delay - prevDelay >= DELAY_SAY_S) say(`Ralentissement : ${Math.round(delay / 60)} minutes de retard.`);
            traffic = { ...t, at: Date.now() };
            map?.getSource('traffic')?.setData({
                type: 'FeatureCollection',
                features: t.jams.map((j) => ({ type: 'Feature', properties: { speed: j.speed }, geometry: { type: 'LineString', coordinates: j.coords } })),
            });
        } catch (e) { /* offline: keep the last traffic, if any */ }
    }

    function clearTraffic() {
        clearInterval(trafficTimer);
        trafficTimer = null;
        traffic = null;
        map?.getSource('traffic')?.setData({ type: 'FeatureCollection', features: [] });
    }

    // ── Voice (OsmAnd-like timing: prepare ~115 s ahead, act ~22 s ahead) ──
    const PREPARE_S = 115, ACT_S = 22, ACT_M = 80, PREPARE_MIN_M = 250, POST_MIN_M = 1500;
    let voiceOn = true;
    try { voiceOn = localStorage.getItem('rascar-nav-voice') !== 'off'; } catch (e) { /* default on */ }

    function say(text) {
        if (!voiceOn || !text) return;
        fetch('/api/nav/say', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text }) }).catch(() => {});
    }
    function toggleVoice() {
        voiceOn = !voiceOn;
        try { localStorage.setItem('rascar-nav-voice', voiceOn ? 'on' : 'off'); } catch (e) { /* not remembered */ }
        refreshVoiceBtn();
        if (voiceOn) say('Guidage vocal activé.');
    }
    function refreshVoiceBtn() {
        const b = document.getElementById('navmap-voice-btn');
        if (!b) return;
        b.classList.toggle('hidden', !route || mapEl()?.classList.contains('hidden'));
        b.querySelector('i').className = `fas ${voiceOn ? 'fa-volume-high' : 'fa-volume-xmark'}`;
    }
    function spokenDist(m) {
        if (m >= 950) {             // never "1000 mètres"
            const km = Math.round(m / 100) / 10;
            return `${String(km).replace('.', ',')} kilomètre${km >= 2 ? 's' : ''}`;
        }
        return `${Math.max(50, Math.round(m / 50) * 50)} mètres`;
    }

    const M_PER_DEG = 111320;
    function metres(a, b) {
        const x = (b[0] - a[0]) * M_PER_DEG * Math.cos((a[1] + b[1]) * Math.PI / 360);
        const y = (b[1] - a[1]) * M_PER_DEG;
        return Math.hypot(x, y);
    }
    // Distance from p to segment ab (flat approximation, fine at street scale).
    function segDist(p, a, b) {
        const k = Math.cos(p[1] * Math.PI / 180) * M_PER_DEG;
        const ax = (a[0] - p[0]) * k, ay = (a[1] - p[1]) * M_PER_DEG;
        const bx = (b[0] - p[0]) * k, by = (b[1] - p[1]) * M_PER_DEG;
        const dx = bx - ax, dy = by - ay;
        const t = Math.max(0, Math.min(1, -(ax * dx + ay * dy) / ((dx * dx + dy * dy) || 1)));
        return Math.hypot(ax + t * dx, ay + t * dy);
    }

    async function startRoute(dest, silent, via) {
        if (!lastFix) { window.RasCar?.media?.toast?.('No GPS position yet'); return; }
        let r;
        try {
            const resp = await fetch('/api/route', {
                method: 'POST', headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ from: [lastFix.lon, lastFix.lat], to: [dest.lon, dest.lat], via: via || [] }),
            });
            r = await resp.json();
            if (!resp.ok) throw new Error(r.error || resp.status);
        } catch (e) {
            window.RasCar?.media?.toast?.(`No route: ${e.message}`);
            return;
        }
        const prev = route;
        route = { ...r, dest, idx: 0, offCount: 0, lastReroute: Date.now(), said: {}, lastNext: -1 };
        if (!silent || !prev) say(r.maneuvers[0]?.pre || r.maneuvers[0]?.instruction);
        refreshVoiceBtn();
        clearTraffic();
        fetchTraffic();
        trafficTimer = setInterval(fetchTraffic, TRAFFIC_EVERY_MS);
        map?.getSource('route')?.setData({ type: 'Feature', geometry: { type: 'LineString', coordinates: r.shape } });
        document.getElementById('navmap-route-card')?.classList.remove('hidden');
        document.getElementById('navmap-banner')?.classList.remove('hidden');
        setText('navmap-route-eta', dest.name);
        renderHome();
        if (!silent && map && r.shape.length > 1) {
            // Show the whole route for a moment, then back to following the car.
            const b = r.shape.reduce((bb, c) => bb.extend(c), new maplibregl.LngLatBounds(r.shape[0], r.shape[0]));
            following = false;
            map.fitBounds(b, { padding: 60, pitch: 0, bearing: 0, duration: 800 });
            setTimeout(recenter, 4000);
        }
        guide(lastFix);
    }

    function cancelRoute() {
        route = null;
        clearTraffic();
        refreshVoiceBtn();
        document.getElementById('navmap-limit')?.classList.add('hidden');
        map?.getSource('route')?.setData({ type: 'FeatureCollection', features: [] });
        document.getElementById('navmap-route-card')?.classList.add('hidden');
        document.getElementById('navmap-banner')?.classList.add('hidden');
        renderHome();
    }

    function setText(id, t) { const el = document.getElementById(id); if (el) el.textContent = t; }

    function fmtDist(m) {
        if (m >= 1000) return `${(m / 1000).toFixed(m >= 10000 ? 0 : 1).replace('.', ',')} km`;
        return `${Math.max(10, Math.round(m / 10) * 10)} m`;
    }

    // Valhalla maneuver type → icon.
    function maneuverIcon(t) {
        if (t >= 4 && t <= 6) return 'fa-flag-checkered';
        if ([9, 10, 11, 18, 20, 23].includes(t)) return 'fa-arrow-right';
        if ([14, 15, 16, 19, 21, 24].includes(t)) return 'fa-arrow-left';
        if (t === 12 || t === 13) return 'fa-arrow-rotate-left';
        if (t === 26 || t === 27) return 'fa-rotate';
        if (t === 25) return 'fa-code-merge';
        if (t === 28 || t === 29) return 'fa-ship';
        return 'fa-arrow-up';
    }

    // Progress along the route for a fix: nearest segment (searching forward
    // from the last one), next maneuver and distance to it, remaining totals.
    function guide(fix) {
        const r = route;
        if (!r || r.shape.length < 2) return;
        const p = [fix.lon, fix.lat];
        let best = r.idx, bestD = Infinity;
        const end = Math.min(r.shape.length - 1, r.idx + 400);
        for (let i = Math.max(0, r.idx - 5); i < end; i++) {
            const d = segDist(p, r.shape[i], r.shape[i + 1]);
            if (d < bestD) { bestD = d; best = i; }
        }
        // Arrival.
        const destM = metres(p, r.shape[r.shape.length - 1]);
        if (destM < ARRIVED_M) {
            if (!r.said.arrived) { r.said.arrived = true; say(`Vous êtes arrivé : ${r.dest.name}.`); }
            setText('navmap-banner-text', `Vous êtes arrivé : ${r.dest.name}`);
            setText('navmap-banner-dist', '');
            document.getElementById('navmap-banner-icon').className = 'fas fa-flag-checkered';
            setTimeout(() => { if (route === r) cancelRoute(); }, 10000);
            return;
        }
        // Off route → reroute (bounded frequency).
        if (bestD > OFF_ROUTE_M) {
            r.offCount++;
            if (r.offCount >= OFF_ROUTE_FIXES && Date.now() - r.lastReroute > REROUTE_MIN_MS) {
                r.lastReroute = Date.now();
                window.RasCar?.media?.toast?.('Recalcul de l\'itinéraire…');
                say('Recalcul de l\'itinéraire.');
                startRoute(r.dest, true);
            }
            return;
        }
        r.offCount = 0;
        r.idx = best;
        // Next maneuver = first one starting after our segment.
        const next = r.maneuvers.find((m) => m.at > best) || r.maneuvers[r.maneuvers.length - 1];
        let toNext = metres(p, r.shape[Math.min(best + 1, r.shape.length - 1)]);
        for (let i = best + 1; i < Math.min(next.at, r.shape.length - 1); i++) toNext += metres(r.shape[i], r.shape[i + 1]);
        let remaining = toNext;
        for (let i = Math.max(next.at, best + 1); i < r.shape.length - 1; i++) remaining += metres(r.shape[i], r.shape[i + 1]);
        let secs = r.km > 0 ? r.seconds * (remaining / 1000) / r.km : 0;
        // With live traffic (Google's time along OUR route), the share of that
        // trip still ahead. "Duration minus time elapsed since the fetch"
        // drifted as soon as progress differed from Google's prediction.
        let delayMin = 0;
        if (traffic && traffic.km > 0.5) {
            const share = Math.min(1, (remaining / 1000) / traffic.km);
            secs = traffic.seconds * share;
            delayMin = Math.round(Math.max(0, traffic.seconds - traffic.free_seconds) * share / 60);
        }
        const eta = new Date(Date.now() + secs * 1000);
        // Motorway signage first: "Exit 12 · City Centre", or the
        // branches of a ramp "A1 / A2 → Centre"; plain instruction otherwise.
        let head = next.instruction, sub = `dans ${fmtDist(toNext)}`;
        if (next.exit) {
            head = `Sortie ${next.exit}${next.toward ? ` · ${next.toward}` : ''}`;
            if (next.exit_name) sub += ` · ${next.exit_name}`;
        } else if (next.branch) {
            head = `${next.branch}${next.toward ? ` → ${next.toward}` : ''}`;
        }
        setText('navmap-banner-text', head);
        setText('navmap-banner-dist', sub);
        document.getElementById('navmap-banner-icon').className = `fas ${maneuverIcon(next.type)}`;
        // Arrival time big (read at a glance), the rest small under it.
        setText('navmap-route-arrival', eta.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' }));
        setText('navmap-route-eta', [
            fmtDist(remaining), `${Math.round(secs / 60)} min`, delayMin >= 2 ? `+${delayMin} min trafic` : '', r.dest.name,
        ].filter(Boolean).join(' · '));
        // Speed limit as a road sign; it turns red above the limit (+5 km/h tolerance).
        const lim = (r.limits || []).find((l) => l.from <= best && best < l.to);
        const sign = document.getElementById('navmap-limit');
        if (sign) {
            sign.classList.toggle('hidden', !lim || !visible());
            sign.classList.toggle('over', !!lim && fix.speed > lim.kmh + 5);
            if (lim) setText('navmap-limit-value', String(lim.kmh));
        }
        // Voice. Each maneuver is announced at most twice (prepare, act);
        // the one just completed gets its "continue for…" if the next is far.
        const ni = r.maneuvers.indexOf(next);
        if (ni !== r.lastNext) {
            const done = r.maneuvers[ni - 1];
            if (r.lastNext >= 0 && done?.post && toNext > POST_MIN_M) say(done.post);
            r.lastNext = ni;
        }
        const mps = Math.max(5, (fix.speed || 0) / 3.6);
        const t = toNext / mps;
        const key = `${ni}`;
        if (!r.said[key + 'act'] && (t <= ACT_S || toNext <= ACT_M)) {
            r.said[key + 'act'] = r.said[key + 'prep'] = true;
            say(next.pre || next.instruction);
        } else if (!r.said[key + 'prep'] && t <= PREPARE_S && toNext >= PREPARE_MIN_M) {
            r.said[key + 'prep'] = true;
            say(`Dans ${spokenDist(toNext)}, ${(next.alert || next.instruction).replace(/^./, (c) => c.toLowerCase())}`);
        }
    }

    // The offline source was selected, or the nav page came back into view.
    async function show() {
        const el = mapEl();
        if (!el) return;
        if (!(await checkAvailable())) {
            window.RasCar?.media?.toast?.('Offline map not installed on this car');
            return false;
        }
        el.classList.remove('hidden');
        init();
        map?.resize();
        if (lastFix) place(lastFix, 0);
        refreshRecenter();
        // A route in progress survives a trip to Google/Waze and back.
        for (const id of ['navmap-route-card', 'navmap-banner']) document.getElementById(id)?.classList.toggle('hidden', !route);
        refreshVoiceBtn();
        refreshLayerButtons();
        renderHome();
        return true;
    }

    function hide() {
        mapEl()?.classList.add('hidden');
        closeSearch();
        for (const id of ['navmap-route-card', 'navmap-banner', 'navmap-voice-btn', 'navmap-limit']) document.getElementById(id)?.classList.add('hidden');
        refreshRecenter();
        refreshLayerButtons();
        renderHome();
    }


    // ── Optional layers: Google traffic tiles, 3D buildings ─────────────
    let trafficTilesTimer = null;

    function layerButtons() {
        let box = document.getElementById('navmap-layers');
        if (box) return box;
        const page = mapEl()?.parentElement;
        if (!page) return null;
        box = document.createElement('div');
        box.id = 'navmap-layers';
        box.className = 'navmap-layers hidden';
        const btn = (id, icon, label, onTap) => {
            const b = document.createElement('button');
            b.id = id;
            b.className = 'nav-kbd-btn';
            b.setAttribute('aria-label', label);
            b.innerHTML = `<i class="fas ${icon}"></i>`;
            b.addEventListener('click', onTap);
            return b;
        };
        box.append(
            btn('navmap-traffic-btn', 'fa-traffic-light', 'Live traffic', toggleTraffic),
            btn('navmap-3d-btn', 'fa-cube', '3D buildings', toggleBuildings),
        );
        page.append(box);
        return box;
    }

    function refreshLayerButtons() {
        const box = layerButtons();
        if (!box) return;
        box.classList.toggle('hidden', !visible());
        document.getElementById('navmap-traffic-btn')?.classList.toggle('off', !trafficOn);
        document.getElementById('navmap-3d-btn')?.classList.toggle('off', !buildingsOn);
        if (!map || !ready) return;
        map.setLayoutProperty('gtraffic', 'visibility', trafficOn ? 'visible' : 'none');
        map.setLayoutProperty('buildings-3d', 'visibility', buildingsOn ? 'visible' : 'none');
        // Refresh the traffic tiles only while they are on screen.
        clearInterval(trafficTilesTimer);
        trafficTilesTimer = null;
        if (trafficOn && visible()) {
            map.getSource('gtraffic')?.setTiles(trafficTiles());
            trafficTilesTimer = setInterval(() => {
                if (visible()) map.getSource('gtraffic')?.setTiles(trafficTiles());
            }, TRAFFIC_TILES_MS);
        }
    }

    function savePref(key, on) {
        try { localStorage.setItem(key, on ? 'on' : 'off'); } catch (e) { /* not remembered */ }
    }
    function toggleTraffic() { trafficOn = !trafficOn; savePref('rascar-nav-traffic', trafficOn); refreshLayerButtons(); }
    function toggleBuildings() { buildingsOn = !buildingsOn; savePref('rascar-nav-3d', buildingsOn); refreshLayerButtons(); }

    // ── Time to home (every source: Google, Waze, offline) ──────────────
    // The server caches (Google at most every 5 min, local Valhalla when the
    // car moved a lot); asking it every minute costs nothing.
    const HOME_POLL_MS = 60000;
    const AT_HOME_M = 300;
    const home = { place: null, eta: null, fetchedAt: 0, loading: false };

    function onNavPage() { return (typeof currentPage === 'undefined' || currentPage === 3) && !document.hidden; }

    function homeChip() {
        let chip = document.getElementById('navmap-home');
        if (chip) return chip;
        const panel = document.querySelector('[data-page="3"] .nav-panel');
        if (!panel) return null;
        chip = document.createElement('button');
        chip.id = 'navmap-home';
        chip.className = 'nav-event-card navmap-home hidden';
        chip.setAttribute('aria-label', 'Route home');
        chip.innerHTML = '<i class="fas fa-house"></i><span><strong id="navmap-home-min"></strong><small id="navmap-home-sub"></small></span>';
        chip.addEventListener('click', routeHome);
        panel.append(chip);
        return chip;
    }

    async function loadHome() {
        try {
            const r = await fetch('/api/nav/home');
            home.place = (await r.json()).home || null;
        } catch (e) { /* keep what we had */ }
    }

    async function setHome(p) {
        try {
            const r = await fetch('/api/nav/home', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(p) });
            if (!r.ok) throw new Error(r.status);
            home.place = (await r.json()).home || null;
            home.eta = null;
            home.fetchedAt = 0;
            window.RasCar?.media?.toast?.(`Home set: ${p.label}`);
            pollHome();
            if (!document.getElementById('navmap-search')?.classList.contains('hidden')) typed();
        } catch (e) {
            window.RasCar?.media?.toast?.('Home not saved');
        }
    }

    async function pollHome(force) {
        if (home.loading || !lastFix || !onNavPage()) return;
        if (!force && Date.now() - home.fetchedAt < HOME_POLL_MS) return;
        home.loading = true;
        try {
            if (!home.place) await loadHome();
            if (!home.place) return;
            const r = await fetch(`/api/nav/home-eta?lat=${lastFix.lat}&lon=${lastFix.lon}`);
            if (r.ok) {
                const j = await r.json();
                home.eta = { ...j, receivedAt: Date.now() };
                if (j.home) home.place = j.home;
            }
            home.fetchedAt = Date.now();
        } catch (e) { /* offline: keep the last value a while */ }
        finally { home.loading = false; renderHome(); }
    }

    function renderHome() {
        const chip = homeChip();
        if (!chip) return;
        const e = home.eta;
        const fresh = e && Date.now() - e.receivedAt < 15 * 60000;
        // Guiding home already: the route card says it all.
        const guidingHome = route && home.place && visible()
            && metres([route.dest.lon, route.dest.lat], [home.place.lon, home.place.lat]) < AT_HOME_M;
        const show = !!(fresh && !e.at_home && home.place && !guidingHome);
        chip.classList.toggle('hidden', !show);
        if (!show) return;
        const eta = new Date(Date.now() + e.seconds * 1000);
        const delay = e.free_seconds != null ? Math.round((e.seconds - e.free_seconds) / 60) : 0;
        setText('navmap-home-min', `${Math.round(e.seconds / 60)} min`);
        setText('navmap-home-sub', [
            `${home.place.label || 'Home'} ${eta.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' })}`,
            delay >= 2 ? `+${delay} trafic` : '',
        ].filter(Boolean).join(' · '));
        chip.classList.toggle('delayed', delay >= 5);
    }

    function routeHome() {
        const p = home.place;
        if (!p) return;
        const offline = typeof navSource === 'undefined' || navSource === 'offline';
        if (offline) { startRoute({ name: p.label || 'Home', detail: 'Home', lat: p.lat, lon: p.lon }); return; }
        if (typeof navigateTo === 'function') navigateTo(`${p.lat.toFixed(6)},${p.lon.toFixed(6)}`);
    }

    setInterval(() => { pollHome(); renderHome(); }, 15000);

    window.RasCar = window.RasCar || {};
    // While the map is on screen, one line every 15 s: was it fed, did it draw?
    setInterval(() => {
        if (!map || !visible()) return;
        const c = map.getCenter();
        const age = (t) => t ? `${((performance.now() - t) / 1000).toFixed(0)}s` : 'never';
        klog(`fixes=${diag.fixes} ignored=${JSON.stringify(diag.ignored)} placed=${diag.placed} ` +
            `lastFix=${age(lastFix?.at)} lastPlaced=${age(diag.lastPlacedAt)} ready=${ready} following=${following} ` +
            `center=${c.lat.toFixed(5)},${c.lng.toFixed(5)} zoom=${map.getZoom().toFixed(1)} ` +
            `canvas=${map.getCanvas().width}x${map.getCanvas().height} route=${!!route} ` +
            `traffic=${trafficOn} 3d=${buildingsOn} trafficErrors=${diag.trafficErrors || 0}`);
    }, 15000);

    window.RasCar.navmap = {
        show, hide, onGps, recenter, isAvailable: checkAvailable, openSearch, closeSearch, cancelRoute, startRoute, toggleVoice,
        toggleTraffic, toggleBuildings, setHome, routeHome, openFavourites,
    };
})();

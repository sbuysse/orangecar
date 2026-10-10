// Applies the last theme before first paint. The server copy (/api/settings) is
// authoritative and app.js re-applies it; this is only a per-browser cache so a
// reload doesn't flash the default theme. Storage can be unavailable: ignore.
(function () {
    try {
        const t = localStorage.getItem('rascar-theme');
        if (t) document.documentElement.dataset.theme = t;
    } catch (e) { /* no storage: keep the default theme */ }
})();

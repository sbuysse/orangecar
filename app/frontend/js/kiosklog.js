/**
 * Kiosk diagnostics: short lines posted to rascar-app, which writes them to its
 * journal (kept on disk by the debug capture). The car kiosk's console is not
 * recorded anywhere else — the offline map that never moved on 2026-10-01 left
 * no trace. Loaded first, so every other script can use it.
 */
(function () {
    'use strict';
    function kioskLog(msg) {
        try {
            fetch('/api/debug/log', { method: 'POST', body: String(msg).slice(0, 500), keepalive: true })
                .catch(() => { /* app down: nothing to report to */ });
        } catch (e) { /* never let logging break the page */ }
    }
    window.RasCar = window.RasCar || {};
    window.RasCar.kioskLog = kioskLog;

    window.addEventListener('error', (e) => {
        kioskLog(`js error: ${e.message} @ ${(e.filename || '').split('/').pop()}:${e.lineno}:${e.colno}`);
    });
    window.addEventListener('unhandledrejection', (e) => {
        kioskLog(`unhandled rejection: ${e.reason && (e.reason.stack || e.reason.message) || e.reason}`);
    });
    kioskLog(`page loaded ${location.pathname} ${innerWidth}x${innerHeight} hidden=${document.hidden}`);
})();

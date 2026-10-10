// Google Maps / Waze frames: the car's position comes from rascar-app through
// Emulation.setGeolocationOverride. Chromium 146 reports POSITION_UNAVAILABLE
// to every watcher each time that override is replaced, in the same
// millisecond as the new position (measured on the car browser, 1 Hz updates:
// err 2, position, err 2, position, ...). A map that takes the error at face
// value drops the "following my location" mode. Hold that error back briefly
// and drop it when a position follows; a real outage still gets through.
(() => {
  const geo = navigator.geolocation;
  if (!geo) return;
  const HOLD_MS = 500;
  const UNAVAILABLE = 2;
  const steady = (success, error) => {
    let pending = null;
    const ok = pos => {
      if (pending) { clearTimeout(pending); pending = null; }
      success(pos);
    };
    const fail = err => {
      if (!error) return;
      if (err.code !== UNAVAILABLE) { error(err); return; }
      if (pending) return;
      pending = setTimeout(() => { pending = null; error(err); }, HOLD_MS);
    };
    return [ok, fail];
  };
  const watch = geo.watchPosition.bind(geo);
  const current = geo.getCurrentPosition.bind(geo);
  geo.watchPosition = (success, error, options) => watch(...steady(success, error), options);
  geo.getCurrentPosition = (success, error, options) => current(...steady(success, error), options);
})();

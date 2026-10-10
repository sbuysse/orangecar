# Chromium managed policies for the car-screen browser

Install into `/etc/chromium/policies/managed/` (read at browser start, i.e. when
`rascar-carplay` launches the kiosk).

- `rascar-embedded-apps.json`: the streaming/meeting apps run as cross-site
  iframes inside the dashboard (`http://localhost:8080`), where Chromium blocks
  their cookies as third-party, so no sign-in or cookie choice can persist.
  Each entry is an `"<embedded site>,<top-level site>"` pair: cookies are
  allowed for that site only while it is embedded in the dashboard. Third-party
  blocking stays on everywhere else. Add a pair when adding an app.

All of `[*.]google.com` is allowed (add your country domain, e.g. `[*.]google.fr`, the same way) (Maps, its `ogs.google.com` account frame, sign-in and Meet), so
a sign-in done through the map page's Sign in button (top-level tab) is seen in
the frame. It used to be left out because its consent answer (`SOCS`, Lax)
could not be stored in a frame and the banner looped; the extension now
re-saves it as SameSite=None.

The policy only covers SameSite=None cookies; Chromium never sends Lax/Strict
ones to a cross-site frame. The frame-unlock extension
(`app/chromium-ext/frame-unlock/background.js`) re-saves these services'
cookies as SameSite=None, so sessions such as Netflix's `NetflixId` work.

`DefaultGeolocationSetting: 1` (allow): the Pi has no location provider;
rascar-app feeds the car's GPS through `Emulation.setGeolocationOverride`.
The `Browser.grantPermissions` it also sends only lasts while that DevTools
connection is open, so the permission fell back to "prompt". A prompt nobody
can answer in the kiosk then blocked every later position request of the tab,
and Maps/Waze only got the position that the dashboard puts in the map URL
(i.e. on reload).

`BrowserSignin: 0` + `SyncDisabled` (2026-10-05): Debian's Chromium ships Google
API keys (`/etc/chromium.d/apikeys`), so a web sign-in to Google also made the
browser try to sign itself in (DICE). That failed (`google.services.signin.
REFRESH_TOKEN_RECEIVED` = "Missing authorization code due to OAuth outage in
Dice"), and from then on, 3 s after every browser start, a browser-internal
request (invisible to extensions) got Google's answer expiring every
`.google.com` session cookie: YouTube Music, Maps and every Google frame were
signed out by each restart, even a clean one 17 s after signing in. With
browser sign-in off, the web session is left alone.

Keep only this file in `/etc/chromium/policies/managed/`: Chromium reads every
file there whatever its extension, and a stale `*.bak` copy of this policy won
the conflict for a day (none of the later changes applied). Backups go to
`/var/tmp/`.

// Session cookies for the embedded apps.
//
// The apps run as cross-site iframes of the dashboard (http://localhost:8080).
// The Chromium policy `rascar-embedded-apps.json` allows their cookies there,
// but that only covers SameSite=None cookies: Chromium never sends a Lax or
// Strict cookie to a cross-site frame, whatever the policy. Netflix keeps its
// session in NetflixId (Lax) / SecureNetflixId (Strict), so the frame looked
// signed out even after a successful sign-in. So, for these services only,
// every stored cookie is re-saved as SameSite=None (which requires Secure).
// Elsewhere third-party cookies stay blocked by the browser, so these cookies
// still only go cross-site to the dashboard.
//
// google.com covers Maps too: its consent answer SOCS (Lax) is re-saved as
// None, so the consent banner no longer loops in the frame.
const DOMAINS = [
  'youtube.com', 'netflix.com', 'disneyplus.com', 'disney.com',
  'microsoft.com', 'microsoftonline.com', 'live.com', 'office.com', 'microsoft365.com', 'cloud.microsoft', 'skype.com',
  'free.fr', 'oqee.tv', 'oqee.net',
  // Waze Live Map: its session and settings cookies were never stored in the
  // frame (0 waze.com cookies in the jar on 2026-09-29).
  'waze.com',
  // Google sign-in cookies (for Meet and the YouTube account); the policy only
  // lets accounts.google.com and meet.google.com receive them in a frame.
  'google.com',
];

const inScope = domain => {
  const d = domain.replace(/^\./, '');
  return DOMAINS.some(x => d === x || d.endsWith('.' + x));
};

// Google's account cookies are NEVER rewritten (2026-10-05). Two reasons:
// - Google already ships a SameSite=None twin of every session cookie for
//   embedded contexts (__Secure-3PSID, -3PAPISID, -3PSIDTS, -3PSIDCC), so the
//   frames are signed in without touching the first-party ones.
// - Several of them rotate on almost every response (SIDCC, *PSIDCC, *PSIDTS).
//   A rewrite is a get() then a set(): a rotation landing in between is
//   overwritten by the OLDER value. Measured in Chromium with this worker on
//   one CPU core: 5 of 20 bursts of 6 rotations ended with a stale value in
//   the jar (0 of 20 without the worker). A browser presenting a session
//   cookie older than the one Google last issued is what a stolen copy looks
//   like, and the car's Google session was revoked again and again.
// Consent cookies (SOCS, CONSENT…) are still relaxed so banners don't loop.
const GOOGLE_SITE = /(^|\.)(google\.[a-z.]+|youtube\.com)$/;
const GOOGLE_ACCOUNT_COOKIE = /^(SID|HSID|SSID|APISID|SAPISID|SIDCC|LSID|OSID|NID|LOGIN_INFO|ACCOUNT_CHOOSER|SMSV|OTZ|__Secure-.+|__Host-.+)$/;
const isGoogleAccountCookie = (domain, name) =>
  GOOGLE_SITE.test(domain.replace(/^\./, '')) && GOOGLE_ACCOUNT_COOKIE.test(name);

// Every write goes through one queue, in arrival order. Sites rotate session
// cookies several times during a login; concurrent writes finishing out of
// order put a stale (signed-out) value back over the new session — that is
// why Netflix never remembered the user.
let queue = Promise.resolve();
const serial = job => { queue = queue.then(job, job); return queue; };

function relax(c) { return serial(() => relaxNow(c)); }

async function relaxNow(c) {
  if (!inScope(c.domain) || c.sameSite === 'no_restriction' || c.partitionKey) return;
  if (isGoogleAccountCookie(c.domain, c.name)) return;
  const host = c.domain.replace(/^\./, '');
  // Only rewrite what is still stored: if the site changed the cookie since
  // this event, that newer value wins (its own event re-queues it).
  const current = await chrome.cookies.get({ url: `https://${host}${c.path}`, name: c.name, storeId: c.storeId }).catch(() => null);
  if (!current || current.value !== c.value || current.domain !== c.domain || current.sameSite === 'no_restriction') return;
  const details = {
    url: `https://${host}${c.path}`,
    name: c.name,
    value: c.value,
    path: c.path,
    secure: true,
    httpOnly: c.httpOnly,
    sameSite: 'no_restriction',
    storeId: c.storeId,
  };
  if (!c.hostOnly) details.domain = c.domain;
  if (!c.session) details.expirationDate = c.expirationDate;
  try { await chrome.cookies.set(details); } catch (e) { /* e.g. a cookie the API may not rewrite */ }
}

// Re-saving fires onChanged again, but then sameSite is already None: no loop.
chrome.cookies.onChanged.addListener(({ removed, cookie }) => { if (!removed) relax(cookie); });

// A Lax/Strict (or SameSite-less, i.e. Lax) cookie SET by a response inside a
// cross-site frame is rejected before it is stored, so onChanged never sees it.
// That is Google's consent answer (SOCS: the consent banner looped) and
// Netflix's sign-in inside the frame ("Something went wrong"). Read those
// Set-Cookie headers as they arrive and store the cookie ourselves, as None.
function parseSetCookie(header, responseUrl) {
  const [pair, ...attrs] = header.split(';');
  const eq = pair.indexOf('=');
  if (eq <= 0) return null;
  const url = new URL(responseUrl);
  const c = { name: pair.slice(0, eq).trim(), value: pair.slice(eq + 1).trim(), path: '/', hostOnly: true, domain: url.hostname };
  let sameSite = 'lax';
  for (const a of attrs) {
    const i = a.indexOf('=');
    const k = (i < 0 ? a : a.slice(0, i)).trim().toLowerCase();
    const v = i < 0 ? '' : a.slice(i + 1).trim();
    if (k === 'domain' && v) { c.domain = v.startsWith('.') ? v : '.' + v; c.hostOnly = false; }
    else if (k === 'path' && v.startsWith('/')) c.path = v;
    else if (k === 'max-age') c.maxAge = Number(v);
    else if (k === 'expires') c.expires = Date.parse(v);
    else if (k === 'httponly') c.httpOnly = true;
    else if (k === 'samesite') sameSite = v.toLowerCase();
    else if (k === 'partitioned') c.partitioned = true;
  }
  if (sameSite === 'none' || c.partitioned) return null; // the browser handles these
  const host = url.hostname;
  // A server may only set cookies for its own host or a parent domain of it.
  const bare = c.domain.replace(/^\./, '');
  if (host !== bare && !host.endsWith('.' + bare)) return null;
  return c;
}

function rescue(c, hadCookies) { return serial(() => rescueNow(c, hadCookies)); }

// A response to a request that carried NO cookies speaks to an anonymous
// visitor: its deletions and new identities must never touch the stored
// session. On 2026-09-29 Maps frames that could not send cookies got Google's
// "clear SID/HSID/…" answer and the rescue signed the user out of google.com.
// Such a response may only add cookies that do not exist yet (a first consent
// answer, a first visitor id).
async function rescueNow(c, hadCookies) {
  if (!inScope(c.domain) || isGoogleAccountCookie(c.domain, c.name)) return;
  const details = {
    url: `https://${c.domain.replace(/^\./, '')}${c.path}`,
    name: c.name, value: c.value, path: c.path,
    secure: true, httpOnly: !!c.httpOnly, sameSite: 'no_restriction',
  };
  if (!c.hostOnly) details.domain = c.domain;
  const now = Date.now() / 1000;
  if (Number.isFinite(c.maxAge)) details.expirationDate = now + c.maxAge;
  else if (Number.isFinite(c.expires)) details.expirationDate = c.expires / 1000;
  if (!hadCookies) {
    if (details.expirationDate !== undefined && details.expirationDate <= now) return;
    try { if (await chrome.cookies.get({ url: details.url, name: c.name })) return; } catch (e) { return; }
  }
  if (details.expirationDate !== undefined && details.expirationDate <= now) {
    // An expired cookie is a deletion.
    try { await chrome.cookies.remove({ url: details.url, name: c.name }); } catch (e) {}
    return;
  }
  try { await chrome.cookies.set(details); } catch (e) {}
}

// Which in-flight frame requests carried a Cookie header (see rescueNow).
const sentCookies = new Set();
const FILTER = { urls: DOMAINS.map(d => `https://*.${d}/*`) };
chrome.webRequest.onSendHeaders.addListener(details => {
  if ((details.requestHeaders || []).some(h => h.name.toLowerCase() === 'cookie' && h.value)) sentCookies.add(details.requestId);
}, FILTER, ['requestHeaders', 'extraHeaders']);
const forget = details => sentCookies.delete(details.requestId);
chrome.webRequest.onCompleted.addListener(forget, FILTER);
chrome.webRequest.onErrorOccurred.addListener(forget, FILTER);

chrome.webRequest.onHeadersReceived.addListener(details => {
  // Only a frame of a real tab (the dashboard's app frame) needs rescuing:
  // top-level pages store their own cookies, and requests outside any tab
  // (tabId -1: a site's service worker updating its script) are sent WITHOUT
  // cookies, so the site answers with a fresh anonymous session. Rescuing
  // that one overwrote the signed-in Netflix session two seconds after every
  // login (service-worker.js, 2026-09-28).
  if (details.tabId < 0 || details.frameType !== 'sub_frame') return;
  for (const h of details.responseHeaders || []) {
    if (h.name.toLowerCase() !== 'set-cookie' || !h.value) continue;
    for (const line of h.value.split('\n')) {
      const c = parseSetCookie(line, details.url);
      if (c) rescue(c, sentCookies.has(details.requestId));
    }
  }
}, FILTER, ['responseHeaders', 'extraHeaders']);

// Cookies stored before this worker ran (browser start, extension update).
chrome.cookies.getAll({}).then(all => all.forEach(relax));

// --- Google session trace ---
// The YouTube session vanished a minute after a good sign-in (2026-10-01):
// youtube.com kept only its third-party SID copies. Record who sets and who
// deletes Google's session cookies: everything to the console (a DevTools
// recorder can follow it), deletions and removals also to the rascar-app
// journal (POST /api/debug/log), which the debug capture keeps on disk.
const SESSION_COOKIE = /^(SID|HSID|SSID|APISID|SAPISID|SIDCC|LOGIN_INFO|__Secure-[13]P(SID|APISID|SIDTS|SIDCC))$/;
const GOOGLE = /(^|\.)(google\.[a-z.]+|youtube\.com)$/;
function trace(line, toJournal) {
  console.log('[gtrace] ' + line);
  if (toJournal) fetch('http://localhost:8080/api/debug/log', { method: 'POST', mode: 'no-cors', body: 'gtrace ' + line }).catch(() => {});
}
chrome.cookies.onChanged.addListener(({ removed, cookie: c, cause }) => {
  if (!SESSION_COOKIE.test(c.name) || !GOOGLE.test(c.domain.replace(/^\./, ''))) return;
  // Every relax() rewrite shows up as removed+overwrite then added: not news.
  if (removed && cause === 'overwrite') return;
  trace(`cookie ${removed ? 'REMOVED' : 'set'} ${c.domain} ${c.name} cause=${cause} ss=${c.sameSite}`, removed);
});
chrome.webRequest.onHeadersReceived.addListener(details => {
  const host = new URL(details.url).hostname;
  for (const h of details.responseHeaders || []) {
    if (h.name.toLowerCase() !== 'set-cookie' || !h.value) continue;
    for (const line of h.value.split('\n')) {
      const name = line.slice(0, line.indexOf('=')).trim();
      if (!SESSION_COOKIE.test(name)) continue;
      const lower = line.toLowerCase();
      const del = /max-age=0\b/.test(lower) || /expires=[^;]*19[0-9]{2}/.test(lower) || /^[^=]+=\s*(;|$)/.test(line);
      const where = `${host}${new URL(details.url).pathname.slice(0, 60)} type=${details.type} frame=${details.frameType} tab=${details.tabId} cookies=${sentCookies.has(details.requestId) ? 'y' : 'n'}`;
      trace(`header ${del ? 'DELETE' : 'set'} ${name} from ${where}`, del);
    }
  }
}, FILTER, ['responseHeaders', 'extraHeaders']);

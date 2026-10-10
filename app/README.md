# rascar-app — the in-car dashboard

`rascar-app` is the web dashboard the car displays. It is a Rust (axum) server
that serves a vanilla-JS single-page app (`frontend/`) and a JSON/WebSocket API
over GPS, OBD-II, media, navigation, the 4G modem and a few online services.
On the board, a Chromium kiosk opens `http://localhost:8080/` and the CarPlay
transmitter (`carplay/`) streams that kiosk to the car's screen, forwarding
touches back to it.

Everything optional is **off until you configure it**: without keys or config
files the dashboard runs fully offline and makes no request to third parties
beyond the ones listed under [External services](#external-services).

## Screens

The pages sit side by side in a horizontal swipe track; the dock on the left
jumps to the main ones, the "layers" button in the status row opens a grid of
all of them.

| # | Screen | What it shows / does |
|---|--------|----------------------|
| 0 | **Trips** | Recorded drives (SQLite), calendar of driving days, per-trip map, speed/altitude/consumption charts, trip cost. |
| 1 | **Vehicle (OBD-II)** | Live gauges from an ELM327 Bluetooth adapter (speed, rpm, coolant, fuel level, load, voltage…), history charts, trouble codes (read / clear, generic SAE J2012 descriptions), readiness monitors, optional TPMS. |
| 2 | **Home** *(optional)* | The house through a self-hosted home server: rooms, lights, blinds, outlets (KNX group addresses), garage door, front door, arrival popup. Hidden unless a home server is configured, see [Home server](#home-server-optional). |
| 3 | **Navigation** | Offline MapLibre map (local PMTiles), offline search (local geocoder), Valhalla routing with French voice guidance (Piper TTS), speed limits, favourites, "time to home" chip; optionally Google Maps / Waze in a frame and Google live traffic. |
| 4 | **Media** (start page) | Now playing (YouTube Music, radio, phone/CarPlay audio), audio visualizer, lyrics (lrclib), volume, fuel/range strip. |
| 5 | **Calendar** | Agenda from iCalendar (ICS) feeds; meeting links open Meet/Teams in a frame. |
| 6 | **Apps** | Tiles for every screen plus the embedded web apps (YouTube, Netflix, Disney+, Free TV, Android Auto panel). |
| 7 | **TV** | HLS channels from your own `tv-channels.json`, with picture-in-picture. |
| 8 | **Radio** | Internet radio from your own `radio-stations.json`, favourites. |
| 9 | **Crypto** | Prices, 24 h change and 7-day sparklines (Kraken, CoinGecko fallback). |
| 10 | **GPS info** | Fix, satellites, speed/altitude history (car GPS over iAP2, else gpsd, else a mock track). |
| 11 | **Weather** | Current conditions, 24 h and 7-day forecast (Open-Meteo). |
| 12 | **YouTube Music** | Built-in player (mpv on the board) driven through the kiosk browser's own YouTube Music session: home feed, search, playlists, liked songs, radio, queue. |
| 13 | **Embedded app** | The web app opened from the Apps page, full size or as a floating PiP. |

Overlays: **Settings** (gear: theme, units, speed alarm, screensaver, fuel,
Wi-Fi, Bluetooth / OBD adapter pairing, 4G data allowance), **phone** (incoming
call, in-call screen, dialer, call history), **arrival popup** (home server).

## Backend services (`src/services/`)

| Service | Role |
|---------|------|
| `gps` | Car GPS (NMEA written by `rascar-carplay` from iAP2) → gpsd → mock track fallback; speed/altitude history, gear from `$PASCD`. |
| `obd/` | ELM327 driver, PID decoding, DTC table, readiness/health, raw log, TPMS (opt-in). |
| `trip` | Trip detection and recording (SQLite, retention sweep). |
| `fuel` | Refill detection from the OBD fuel level, fuel "wallet", range and trip cost. |
| `park_sync` | On shift into Park: checkpoint SQLite, `sync`, flush journald; powers the board off when the engine is confirmed off in Park (the car cuts power without warning). |
| `navigation` | Offline geocoder search, Valhalla routes, speed limits, voice guidance (Piper), optional Google Routes traffic, time-to-home. |
| `weather` | Open-Meteo forecast for the current GPS position (or `--lat/--lon`). |
| `calendar` | ICS feed fetch + cache, per-source ids and visibility. |
| `media`, `player`, `music`, `ytbridge`, `lyrics`, `resume`, `visualizer`, `volume` | Now playing (MPRIS), mpv player, YouTube Music helper + browser-session bridge, lyrics, resume after a power cut, audio spectrum from PulseAudio, ALSA volume. |
| `apps`, `signin` | Embedded web apps through Chromium DevTools (PiP, meeting frames, sign-in overlay). |
| `headunit` | Head-unit info written by the CarPlay transmitter. |
| `cell`, `datausage` | SIM7600 4G status, data counters, monthly allowance and optional cut at the limit. |
| `connections` | Wi-Fi (wpa_supplicant) and Bluetooth (bluetoothctl) from the Settings panel. |
| `signal` | Wi-Fi signal strength. |
| `phone/` | Phone calls through the SIM7600 modem, audio over the car's CarPlay telephony stream (opt-in). |
| `home_server` | Optional home-server client (see below). |

## Running it on a PC for development

Requirements: a Rust toolchain (edition 2024). Optional tools the backend uses
when present: `pactl` (visualizer), `playerctl` (MPRIS), `mpv` + Python 3 with
`ytmusicapi` (YouTube Music), `gpsd`.

```sh
cargo run --bin rascar-app -- --bind 127.0.0.1
# then open http://localhost:8080
```

What to expect without the car:

- **GPS**: with no car feed and no gpsd, after a few failed attempts the GPS
  service switches to a **mock track** (a small circle around the Royal
  Observatory, Greenwich). It is flagged `mock: true` and never counts as a
  fix, so no trips are recorded. To feed real NMEA, run gpsd, or point
  `RASCAR_CAR_GPS_PATH` at a file you append NMEA sentences to.
- **Weather** needs a fix; for a fixed place pass `--lat 48.85 --lon 2.35`.
- **OBD** retries `/dev/rfcomm0` and shows no data; pass `--obd-port` with a
  USB ELM327 (`/dev/ttyUSB0`) to test with real hardware.
- **Map / search / routing** need the offline data in `--maps-dir` (PMTiles
  archive `region.pmtiles`, `fonts/`, `sprites/`, `geocoder.sqlite`)
  and Valhalla listening on `127.0.0.1:8002`; see `setup/maps/`.
- To keep your own `~/.config` clean, run with a throwaway home:
  `HOME=/tmp/rascar-home cargo run --bin rascar-app -- --bind 127.0.0.1`.
- Set `RASCAR_PARK_POWEROFF=0` if you ever feed real OBD + GPS gear data on a
  machine that must not power itself off.

`cargo test -p rascar-app` runs the unit tests (no hardware needed).

## Command-line options

| Option | Default | Meaning |
|--------|---------|---------|
| `--port` | `8080` | HTTP port. |
| `--bind` | `0.0.0.0` | Listen address. The API has **no authentication** (the occupants' phones can reach it on the car hotspot); use `127.0.0.1` on any untrusted network. Home and sign-in routes only answer loopback anyway. |
| `--trip-retention-days` | `365` | Delete trips older than this at startup (0 = keep all). |
| `--obd-port` | `/dev/rfcomm0` | ELM327 serial device. |
| `--ics-uris` | *(empty)* | Comma-separated ICS feed URLs, merged with `calendar_ics_uris` from settings. |
| `--maps-dir` | `/var/lib/rascar/maps` | Offline map data (PMTiles, fonts, sprites, geocoder). |
| `--lat`, `--lon` | *(none)* | Weather fallback location used until the GPS has a fix. Without them the forecast waits for the first fix. |

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_FRONTEND_DIR` | auto | Directory served as the dashboard (else `./frontend`, next to the binary, `/opt/orangecar/app/frontend`, or the source tree). |
| `RASCAR_CAR_GPS_PATH` | `/var/lib/rascar-debug/car-gps.nmea` | NMEA file the CarPlay transmitter writes with the car's GPS. |
| `RASCAR_HEADUNIT_PATH` | `/var/lib/rascar/headunit.json` | Head-unit record written by the CarPlay transmitter. |
| `RASCAR_OBD_RAWLOG_DIR` | `/var/lib/rascar-debug/obd` | Raw ELM327 traffic log; `off` (or empty) disables it. |
| `RASCAR_OBD_TPMS` | off | `1`/`true`/`yes`/`on` enables the (Jaguar Land Rover specific) tyre-pressure reads. |
| `RASCAR_PARK_POWEROFF` | on | `0` disables the clean power-off when the engine is off in Park. |
| `RASCAR_PHONE` | off | Enables the phone-call service on the SIM7600 modem (emergency numbers still start it on demand). |
| `RASCAR_MIXER_CONTROL` | `Master` | ALSA mixer control used for the volume. |
| `RASCAR_VIZ_SOURCE` | default sink's `.monitor` | PulseAudio source the visualizer listens to. |
| `RASCAR_YTMUSIC_HELPER` | auto | Path of `scripts/ytmusic_helper.py`. |
| `RASCAR_COINGECKO_KEY` | *(none)* | CoinGecko demo API key (also `coingecko_api_key` in settings.json). |
| `RASCAR_YTM_BRIDGE`, `RASCAR_YTM_BRIDGE_TOKEN` | set by the app | Internal: passed to the YouTube Music helper, not meant to be set by hand. |
| `XDG_RUNTIME_DIR` | | Used for the mpv IPC socket and the phone's PulseAudio access. |

## Configuration files

All under `~/.config/rascar/` of the user running the app (keep them `0600`
when they hold secrets).

| File | Content |
|------|---------|
| `settings.json` | Written by the Settings panel; some keys are hand-edited only (below). |
| `radio-stations.json` | `[{"name", "genre", "url"}]` — the radio list, also the allowlist of the stream proxy. |
| `tv-channels.json` | `[{"name", "url"}]` — HLS channels, also the allowlist of the HLS proxy. |
| `contacts.json` | `[{"name", "phone"}]` — phone dialer contacts and caller-id names. |
| `home-server.json` | Optional home server (see below). Absent = feature off. |
| `trips.db`, `datausage.db`, `calendar-cache.json`, `last_media.json` | State written by the app. |

Other files: `/etc/rascar/google-routes.key` (optional Google Routes key, one
line), `/var/lib/rascar/voice/fr_FR-siwis-medium.onnx` + `/opt/piper/piper`
(voice guidance).

### `settings.json`

| Key | Default | Meaning |
|-----|---------|---------|
| `speed_alarm_kmh` | `0` | Speed alarm threshold, 0 = off. |
| `units` | `"metric"` | `"metric"` or `"imperial"`. |
| `screensaver_timeout` | `300` | Seconds of inactivity before the screensaver, 0 = never. |
| `theme` | `"night"` | `night`, `glass`, `amber` or `day`. |
| `calendar_ics_uris` | `[]` | ICS feed URLs (hand-edited; never sent back to the page — they often embed a token). |
| `calendar_hidden` | `[]` | Feed ids hidden on the Calendar page. |
| `radio_favorites`, `tv_favorites` | `[]` | Favourite stations / channels by name. |
| `obd_device`, `obd_device_name`, `obd_pin` | none | Bluetooth OBD adapter chosen in Settings (MAC, name, PIN if not 1234/0000); read by the `rascar-obd-link` root service. |
| `home` | none | Home for "time to home", set from the nav search ("Set as home"); falls back to the home server's home area. |
| `nav_favorites` | `[]` | Hand-edited favourite destinations: `[{"name", "address", "icon", "lat", "lon"}]` (`icon` = Font Awesome class such as `fa-house`; `address` is handed to Google Maps / Waze, the coordinates to the offline router). |
| `fuel_tank_l` | `63` | Usable tank capacity (litres), turns the OBD fuel % into litres. |
| `fuel_price_eur_l` | `1.80` | Price per litre until a refill with a price is entered. |
| `fuel_l100` | `9.5` | Consumption used when the car reports neither fuel rate nor air flow. |
| `ytmusic_enabled` | none | Turn YouTube Music off with `false`. |
| `ytmusic_auth_path` | none | Obsolete, ignored (kept for old files). |
| `cell_data` | see below | 4G data settings, written by the 4G panel. |
| `coingecko_api_key` | none | Optional CoinGecko key (hand-edited, never sent to the page). |

`cell_data`: `enabled` (true), `allowance_gb` (0 = no allowance), `cycle_start_day`
(1), `cut_at_limit` (false — turn 4G off when the allowance is used up),
`ota_over_4g` (true), `logs_over_4g` (true), `cap_override_cycle` (internal).

Unknown keys are preserved on save.

## External services

All optional; none needs an account unless noted.

| Service | Used for | Key |
|---------|----------|-----|
| Open-Meteo | Weather | none |
| Kraken public API, CoinGecko, alternative.me | Crypto page | CoinGecko key optional |
| lrclib.net | Lyrics | none |
| YouTube Music | Music page | uses the kiosk browser's own sign-in; no key stored by the app |
| Google Routes API | Live traffic duration / jams on the offline route | your own key in `/etc/rascar/google-routes.key`; no file = no request |
| Google Maps traffic tiles | Traffic layer on the offline map | none; **off by default** (toggle on the map). Check Google's terms before using Google data over a non-Google map. |
| Google Maps / Waze, streaming sites, Meet/Teams | Embedded web apps | your own accounts, signed in inside the kiosk browser |
| Your ICS feeds | Calendar | — |
| Your home server | Home page | see below |

Local services expected on the board: Valhalla (`127.0.0.1:8002`), gpsd,
PulseAudio, wpa_supplicant, BlueZ, the 4G stick's QMI link (see `setup/`), Chromium with DevTools on `127.0.0.1:9222`.

## Home server (optional)

The Home page, the arrival popup ("open the front door / the garage?") and the
periodic car-position upload talk to a self-hosted home-automation server. The
feature is **off** unless `~/.config/rascar/home-server.json` exists; then
only `/api/home/status` answers (`configured: false`), the page and its tiles
are hidden and nothing goes over the network.

```json
{
  "lanUrl": "http://homeserver.local",
  "remoteUrl": "https://example.org",
  "certSha256": "<sha-256 hex of the remote certificate>",
  "username": "car",
  "password": "…",
  "frontDoor": { "address": "1.2.3", "pulseSecs": 5 },
  "home": { "latitude": 0.0, "longitude": 0.0, "radiusMeters": 100 }
}
```

`lanUrl` is tried first; `remoteUrl` (optional, self-signed certificate pinned
by SHA-256) is the fallback when the LAN is unreachable. `frontDoor` and `home`
are optional (no door tile / no arrival popup without them). All `/api/home/*`
routes answer only loopback clients (the kiosk), since they open doors.

HTTP contract the server must implement (JSON, `Authorization: Bearer <token>`
on everything but the login; a 401 triggers one re-login):

| Request | Answer |
|---------|--------|
| `POST /api/auth/login` `{"username","password"}` | `{"token": "<JWT or opaque token>"}` |
| `GET /api/topology` | `[{"kind":"floor","name",…,"elements":[{"kind":"room","name","elements":[…]}]}]`. Room elements: `{"kind":"lights","name","groupaddress","groupaddress_dim_status"?}`, `{"kind":"store","name","groupaddress_updown","groupaddress_stop"?}`, `{"kind":"prises","outlets":[{"name","groupaddress"}]}`, any element may carry `"temperature": "<group address>"`. |
| `GET /api/events?action=knx_value&limit=1000` | Newest first: `[{"payload":{"destination":"1/2/3","value_float":21.5}}]` (latest value per address is kept). |
| `POST /api/knx` `{"address":"1.2.3","value":"1"}` | 2xx when queued. Used for lights, blinds (0 = up, 1 = down, 1 on the stop address), outlets and the front-door pulse (0 then 1 after `pulseSecs`). |
| `GET /api/garage` | `[{"id","available":bool,"state":one of open, closed, opening, closing, stopped, intermediate_open, intermediate_closed, or null,"light":bool}]` |
| `GET /api/garage/{id}/state?fresh=true` | One door snapshot, read from the device. |
| `POST /api/garage/{id}/{open\|close\|stop\|ventilate}` | Door snapshot; 409 with a plain-text reason when refused (interlock). |
| `POST /api/garage/{id}/light` `{"on":bool}` | Door snapshot. |
| `GET /api/persistent` | Free key/value map; `weather.temp` is shown as the outside temperature. |
| `GET /api/car` / `POST /api/car` `{"latitude","longitude"}` | Last car position / store it (`{"success":true}`). Pushed every 25 m of movement or 30 min. |

## API overview

`GET /ws` is the live WebSocket (GPS, OBD, media, phone, cell, arrival, …).
The REST routes live in `src/main.rs` (`/api/gps`, `/api/obd/*`, `/api/trips/*`,
`/api/fuel/*`, `/api/calendar/*`, `/api/weather`, `/api/media/*`,
`/api/music/*`, `/api/radio/*`, `/api/tv/*`, `/api/apps/*`, `/api/geo/search`,
`/api/route`, `/api/traffic`, `/api/nav/*`, `/api/settings`, `/api/wifi/*`,
`/api/bluetooth/*`, `/api/cell`, `/api/calls`, `/api/contacts`, `/api/home/*`).

## Other directories

- `scripts/ytmusic_helper.py` — `ytmusicapi` wrapper the backend runs for
  YouTube Music; signed-in calls go through the kiosk browser's session via a
  loopback bridge, so no cookie or token is stored on disk by the app.
- `chromium-ext/frame-unlock/` — kiosk Chromium extension that lets the
  dashboard frame Maps, Waze, the streaming apps and meetings (strips
  `X-Frame-Options`/CSP on their sub-frames, `SameSite=None` cookies).

# Third-party code and credits

OrangeCar is licensed under the **GNU GPL v3** (see [LICENSE](LICENSE)). Parts of it are derived from,
or were checked against, other open-source projects. Thank you to their authors: without them this
project would not exist.

## Code derived from other projects

| Project | License | Used for | Where |
|---|---|---|---|
| [usenocturne/iap2-rs](https://github.com/usenocturne/iap2-rs) | GPL-3.0 | iAP2 link layer, control session and auth logic, adapted to the *device* (phone) side | `carplay/src/iap2/` |
| [omarroth/doubletake](https://github.com/omarroth/doubletake) | LGPL-3.0 | AirPlay sender building blocks: RTSP client, binary plist, HomeKit pairing, FairPlay | `carplay/src/airplay/{rtsp,pairing,fairplay,fairplay_data}.rs` |
| Linux kernel `drivers/net/usb/cdc_ncm.c` | GPL-2.0 | Patched CDC-NCM host driver (the patch only, applied to your kernel's own source) | `kernel/cdc_ncm/` |

Because `carplay/` includes GPL-3.0 code, the whole repository is distributed under GPL-3.0.

`carplay/src/airplay/fairplay_data.rs` reproduces FairPlay tables published by doubletake (and, before it,
by other open AirPlay implementations). The CarPlay path used with the tested car does **not** run
FairPlay setup at all (legacy receiver, see [docs/protocol.md](docs/protocol.md)); the code is kept for
receivers that ask for it.

## Projects used as references (no code copied)

| Project | What we checked against it |
|---|---|
| [catplay-labs/catplay](https://github.com/catplay-labs/catplay) | Wired CarPlay session order, screen frame format, stream descriptions, modes/resources, the `_carplay-ctrl` mDNS packet layout. catplay publishes no license, so none of its code is included; only wire-format facts were checked against it. |
| [opencardev/aasdk](https://github.com/opencardev/aasdk) | Android Auto protocol (experimental head unit only) |
| [lvalen91/carplayd](https://github.com/lvalen91/carplayd) (`iap2-core/src/spec.rs`) | iAP2 message and parameter identifiers |
| [shilapi/xcertplay](https://github.com/shilapi/xcertplay) | iAP2 message directions |
| Carlinkit wireless CarPlay dongle firmware | How a commercial "phone side" brings up the CDC-NCM link |

## Vendored front-end assets (`app/frontend/vendor/`)

| Asset | Version | License |
|---|---|---|
| Font Awesome Free | 6.5.1 | Icons CC BY 4.0, fonts SIL OFL 1.1, code MIT |
| Leaflet | 1.9.4 | BSD-2-Clause |
| MapLibre GL JS | 5.24.0 | BSD-3-Clause |
| PMTiles | 4.5.0 | BSD-3-Clause |
| @protomaps/basemaps | 5.7.2 | BSD-3-Clause (map data © OpenStreetMap contributors, ODbL) |
| hls.js | minified build | Apache-2.0 |
| Manrope, Sora (variable fonts) | latin subsets | SIL OFL 1.1 |

## Not included, fetched by you

- The Android Auto head-unit certificate used by `experimental/headunit` is **not** in this repository.
  `experimental/androidauto-setup/aa-headunit-cert.sh` downloads it from aasdk at a pinned commit, for
  personal, non-commercial use only.
- Offline map tiles and routing graphs are built on your machine from OpenStreetMap data
  (`setup/maps/`).

## Trademarks

CarPlay, iPhone, AirPlay and iAP are trademarks of Apple Inc. Android Auto is a trademark of Google LLC.
Jaguar and InControl are trademarks of Jaguar Land Rover. This project is not affiliated with,
endorsed by or certified by any of them. It is an independent interoperability project.

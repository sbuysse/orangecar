# Optional: self-hosted OTA updates and log upload

A car unit is hard to reach. It only has power while the car runs, it sits behind 4G, and the log of a
failed drive is the one you need. `rascar-agent` covers both problems. When an update server is
configured, it:

- **uploads logs**: the journal of the rascar units and the kernel, plus the CarPlay run logs, the
  early-boot traces and the raw OBD logs under `/var/lib/rascar-debug`. Uploads resume across power cuts
  and stay inside a daily budget on 4G;
- **installs signed releases**: it downloads a release, checks its ed25519 signature against a key the
  car holds, stages it next to the running one, switches at the next boot, and rolls back on its own if
  the new release is unhealthy.

Everything here is optional. **Without `/etc/rascar/update-server.json` the agent stays idle.** It then
only finishes judging a trial release, if one is pending. There is no default server; you run your own.

Contents:

- [How a release reaches the car](#how-a-release-reaches-the-car)
- [Setting it up](#setting-it-up)
- [Making a release](#making-a-release)
- [The HTTP contract a server must implement](#the-http-contract-a-server-must-implement)
- [Release format](#release-format)
- [Safety rules](#safety-rules)

## How a release reaches the car

```text
PC: tools/release.sh ──build, sign──► update server ◄──── car: rascar-agent (every hour)
                                         │   offer + bundle
                                         ▼
   stage: /opt/orangecar-releases/<version>   (hard links of the current tree + bundle files)
   next boot: rascar-ota-boot ─► rascar-agent apply-staged ─► /opt/orangecar symlink moved, "trial"
   services start ─► agent judges health after settle_secs ─► commit, or roll back + mark version bad
   trial still pending after 3 boots ─► rascar-ota-boot rolls back in shell (agent not trusted)
```

1. **Stage** (background, any time). The agent receives an offer, checks the signature and the
   manifest, downloads the bundle (resumable, with `Range`), and checks the bundle's SHA-256. It then
   builds a complete new tree in `/opt/orangecar-releases/<version>`. The current tree is hard-linked,
   so files only the board has (such as Chromium's extension `_metadata`) carry over, and every bundle
   file is written over it and checked against its own SHA-256. Nothing running is touched.
2. **Apply** (next boot). `rascar-ota-boot.service` runs before every rascar unit. It calls the
   *current* agent's `apply-staged`, which installs the release's system files (backing up what they
   replace), points `/opt/orangecar` at the new tree, and writes `/var/lib/rascar-agent/trial`. This
   takes milliseconds and needs no network.
3. **Judge.** Once the services have run for the manifest's `settle_secs`, the agent checks the health
   rules in the manifest: units active or not failed, URLs answering 2xx. On success the trial is
   committed. On failure the agent rolls back live (symlink back, system files restored, units
   restarted) and marks the version bad, so it is never offered again.
4. **Boot guard.** If the new agent crashes before it can judge, the trial is still pending after
   `max_trial_boots` (3) boots. `rascar-ota-boot` then rolls back by itself, in shell, without running
   any code from the release under trial. The guard script and its unit are never part of a release.

The dashboard's 4G panel can forbid "OTA over 4G" and "logs over 4G". The agent reads those switches
from the dashboard user's `settings.json` (`cell_data.ota_over_4g`, `cell_data.logs_over_4g`) on every
round.

## Setting it up

### On the PC that signs releases (once)

```sh
cargo run --release -p rascar-agent --bin rascar-release -- keygen --key ~/.config/rascar/ota-signing.key
chmod 600 ~/.config/rascar/ota-signing.key
# prints the PUBLIC key (64 hex digits); print it again with:
cargo run --release -p rascar-agent --bin rascar-release -- pubkey --key ~/.config/rascar/ota-signing.key
```

The signing key is a 32-byte ed25519 seed in hex. Keep it off the car and off the server: whoever holds
it can push code that runs as root on the car.

### On the car

```sh
sudo setup/install.sh --with-ota          # or: sudo setup/ota/install-ota.sh
sudo setup/ota/install-ota.sh --pubkey <64-hex public key>     # writes /etc/rascar/ota.pub
sudo cp /etc/rascar/update-server.json.example /etc/rascar/update-server.json
sudo chmod 600 /etc/rascar/update-server.json && sudoedit /etc/rascar/update-server.json
sudo systemctl restart rascar-agent
```

`/etc/rascar/update-server.json`:

```json
{
  "lanUrl": "http://updates.lan",
  "remoteUrl": "https://updates.example.org",
  "certSha256": null,
  "username": "car",
  "password": "change-me"
}
```

| Key | Meaning |
|---|---|
| `lanUrl` | optional. Tried first with a 1.5 s connect timeout: a server on your home network, reached when the car is parked within Wi-Fi range |
| `remoteUrl` | optional. Used when `lanUrl` is unset or unreachable (e.g. over 4G) |
| `certSha256` | optional. Hex SHA-256 of the remote server's certificate (DER) for a self-signed server. Unset = normal verification against the public CAs |
| `username`, `password` | the car's account on the server |

At least one URL is required. `/opt/orangecar` must be a symlink to a release tree. `setup/install.sh`
sets it up that way. An older install with a plain directory needs `sudo setup/ota/install-ota.sh --migrate`
once.

On the car:

```sh
journalctl -fu rascar-agent
sudo /opt/orangecar/target/release/rascar-agent status      # local state (JSON)
sudo /opt/orangecar/target/release/rascar-agent check-now   # ask the server now, stage if newer
sudo /opt/orangecar/target/release/rascar-agent apply-now   # bench: switch now, restart units, judge
```

## Making a release

```sh
export RASCAR_OTA_URL=https://updates.example.org RASCAR_OTA_USER=admin
tools/release.sh                       # tests, cross-build, assemble, sign, upload, publish
tools/release.sh --no-upload           # build + sign only (output in ~/.local/share/orangecar/releases/<version>)
```

The script requires a clean git tree (or `--allow-dirty`), `cargo-zigbuild`, `jq` and `rsync`. It
cross-builds `rascar-app`, `rascar-carplay` and `rascar-agent` with `RASCAR_BUILD_ID=<version>`, and
ships `app/frontend`, `app/scripts`, `app/chromium-ext` and `setup/` with them. It also ships the system
files listed in [`tools/release-system-files.txt`](../tools/release-system-files.txt) (scripts in
`/usr/local/sbin/rascar-*`, rascar units and drop-ins). It then signs the manifest, verifies the result
with the public key, and uploads it. The version is `YYYYMMDD.HHMM-<git sha>`, and the serial is the
build's unix time.

**Before the first release**, diff the board's installed system files against the repository. A release
replaces them at the next boot (backups are kept in the release for rollback).

## The HTTP contract a server must implement

The agent is the client. It never accepts incoming connections. Every request except the login carries
`Authorization: Bearer <token>`. On `401` or `403` the agent logs in once more and retries once. Paths are
relative to `lanUrl` / `remoteUrl`.

### `POST /api/auth/login`

Request: `{"username": "...", "password": "..."}` (JSON). Response `200`: `{"token": "<opaque bearer token>"}`.
Any other status means the login failed. `tools/release.sh` uses the same endpoint with an admin account.

### `GET /api/carops/update?version=<installed version>&serial=<installed serial>`

The car asks whether a newer release exists. An install that OTA did not create reports
`version=base&serial=0`.

- `204`: nothing newer.
- `200` with an **offer**:

```json
{
  "version": "20261006.1412-a056fb1",
  "serial": 1791288720,
  "manifest": "<base64 of the exact manifest.json bytes that were signed>",
  "signature": "<content of manifest.sig: base64 ed25519 signature>",
  "bundle_size": 14123456,
  "bundle_url": "/api/carops/releases/20261006.1412-a056fb1/bundle.tar.gz"
}
```

The car refuses the offer if `version`/`serial` differ from the signed manifest, if the serial is not
greater than the installed one (no downgrades), if the version was rolled back before, or if
`bundle_size` differs from the manifest. Only offer releases whose `serial` is higher than the one in the
query.

### `GET <bundle_url>`

`bundle_url` is a path on the same server, fetched with the bearer token. The car resumes interrupted
downloads with `Range: bytes=<offset>-`, so answer `206` with the remaining bytes. A plain `200` with
the whole file also works: the car then starts over. `416` makes the car restart from zero. After the
download the car checks the size and SHA-256 against the signed manifest, so the server needs no
integrity guarantees of its own.

### `POST /api/carops/logs`

Appends a chunk to a named log stream:

| Header | Value |
|---|---|
| `X-Car-Stream` | stream name: `/`-separated components of `[A-Za-z0-9._@~+-]`, at most 6 components. Examples: `boot-20261006-0912-1a2b3c4d/journal.log`, `files/boot-…/runs/001-091530-carplay.log~123456` |
| `X-Car-Offset` | byte offset of this chunk in the stream |
| `Content-Encoding` | `gzip` (the body is the gzip of the raw bytes) |
| `Content-Type` | `application/octet-stream` |

Body: up to `upload.batchBytes` (512 KiB) of raw data before compression. The journal is one entry per
line.

Response JSON `{"have": <bytes the server now holds for this stream>}`, with:

- `200`: the chunk was appended, or it was already there. The server must be idempotent: a chunk whose
  offset+length is at or below `have` is acknowledged without being appended again. The car may resend
  after a power cut.
- `409`: the offset does not match what the server holds, for example a gap. `have` tells the car where
  to continue. If `have` is larger than the car's file, the car skips that stream.
- any other status: the car stops this round and retries later.

The agent moves its local cursor only to the `have` the server acknowledges, which makes uploads
exactly-once across power cuts.

### `POST /api/carops/status`

JSON body, small (keep the server's limit at 64 KiB or more), sent every `statusIntervalSecs` and after
each update result:

```json
{
  "agent_version": "0.1.0+20261006.1412-a056fb1",
  "version": "20261006.1412-a056fb1", "serial": 1791288720,
  "state": "ok | staged | trial | rolled_back | failed",
  "staged": null, "trial": null,
  "last_update": { "version": "…", "result": "staged|applied|ok|rolled_back|failed", "at": "<RFC 3339>", "detail": "…" },
  "bad_versions": [], "boot_id": "1a2b3c4d", "uptime_secs": 812,
  "metered": true, "log_backlog_bytes": 0, "detail": "…",
  "headunit": { "…": "what the car reported over iAP2 (/var/lib/rascar/headunit.json)" }
}
```

Answer any `2xx`. The car ignores the body.

### Admin endpoints used by `tools/release.sh`

These are not used by the car. Implement them however suits you, or upload releases another way.

- `PUT /api/carops/admin/releases/<version>/manifest.json`, `.../manifest.sig`, `.../bundle.tar.gz`:
  raw bodies, admin bearer token.
- `POST /api/carops/admin/releases/<version>/publish`: makes the version the one offered by
  `/api/carops/update`.

A minimal server is a static directory plus a small handler for login, update, logs and status. The
release files need no protection beyond access control, because the car trusts only the signature.

## Release format

`bundle.tar.gz` (deterministic tar, gzip):

```text
tree/<path>     goes into the release tree (/opt/orangecar-releases/<version>/<path>)
system/<path>   installed at /<path>, allowlisted
```

`manifest.json`, signed byte for byte:

| Field | Meaning |
|---|---|
| `format` | 1 |
| `version`, `serial` | release name, and a number that must strictly increase |
| `git_sha`, `created` | provenance |
| `target` | device family (`orangecar`). The car refuses other targets (`ota.target`) |
| `bundle` | `{ "size", "sha256" }` of `bundle.tar.gz` |
| `files[]` | `{ "path", "sha256", "size", "mode" }` for every bundle entry |
| `mirror[]` | tree directories the bundle owns completely: files there that are not in the bundle are removed |
| `preserve[]` | tree paths never replaced or removed (board-only files) |
| `restart[]` | units restarted by a live apply / rollback (a boot-time apply restarts nothing) |
| `health` | `{ "services": [{ "unit", "check": "active" or "not-failed" }], "http": [urls], "settle_secs" }` |

`rascar-release` builds, signs and verifies all of this (`bundle`, `manifest`, `sign`, `verify`, `keygen`,
`pubkey`). Run it with `--help`.

## Safety rules

- **The signature is the only trust anchor.** A compromised server or network can withhold updates. It
  cannot install anything the signing key did not sign, cannot replay an older signed release (the
  serial must grow), and cannot re-offer a version that was rolled back.
- **System files are allowlisted** (`agent/src/manifest.rs`, `system_path_allowed`): only
  `/usr/local/sbin/rascar-*`, `/etc/systemd/system/rascar-*.{service,target,timer,conf}`,
  `/etc/udev/rules.d/*rascar*.rules` and `/etc/modules-load.d/rascar*.conf`. A release can never touch
  `/etc/rascar` (credentials, keys, configuration), and can never replace the boot guard that judges it.
- Releases do not enable or disable units. Do that by hand or with `setup/install.sh`.
- The agent runs with `Nice=10`, idle I/O priority, `CPUQuota=30%` and `MemoryMax=300M`. It is never on
  the CarPlay start path: nothing waits for it.

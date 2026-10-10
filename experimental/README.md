# Experimental: Android Auto

These crates are **not needed for CarPlay**. They are kept because they work, partly, and may help
someone else.

| Path | What it is | State |
|---|---|---|
| `bridge/` (`rascar-bridge`) | The board pretends to be an **Android phone** (AOA2 accessory gadget) and projects the dashboard to an Android Auto head unit. | **Dead end.** The head unit authenticates the phone with a Google-signed certificate. Without one, the session stops at `AuthComplete -3`. This is why the project switched to CarPlay, which needs no certificate and no MFi chip on the phone side. |
| `headunit/` (`rascar-headunit`, `rascar-aa`) | The reverse: the board is an Android Auto **head unit** for a real Android phone plugged into one of its USB host ports. The phone's projected UI is decoded and shown in a dashboard panel (`app/frontend/aa.html`), which then reaches the car through CarPlay. | Works on the bench with an Android phone. Needs the publicly available aasdk head-unit certificate (see below). Personal, non-commercial use only. |
| `androidauto-setup/` | udev rule, systemd unit and certificate installer for `rascar-aa`. | Optional. |

## Installing the head-unit identity

A phone only talks to a head unit whose TLS client certificate chains to Google's "Google Automotive
Link" CA. Every open-source head unit uses the JVC Kenwood certificate published by
[opencardev/aasdk](https://github.com/opencardev/aasdk). It is **not** in this repository:

```bash
sudo experimental/androidauto-setup/aa-headunit-cert.sh   # downloads at a pinned commit, checks SHA-256
```

Then install the unit (replace `@RASCAR_USER@` / `@RASCAR_UID@` with the kiosk user and its uid):

```bash
sudo cp experimental/androidauto-setup/rascar-aa.service /etc/systemd/system/
sudo cp experimental/androidauto-setup/70-rascar-aa.rules /etc/udev/rules.d/
sudo systemctl daemon-reload && sudo systemctl start rascar-aa
```

Using this certificate is not licensed by Google. Do not ship it in a product.

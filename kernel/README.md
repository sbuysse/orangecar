# Patched `cdc_ncm` kernel module

After the USB role switch, the board is the USB **host** and the car's head unit
is a USB **device** (an Apple "gadget"). The CarPlay control and media traffic
runs over IPv6 link-local on a CDC-NCM network function that this gadget
exposes. Linux drives that function with `drivers/net/usb/cdc_ncm.c`. The stock
driver does not work with the tested head unit (Jaguar F-PACE InControl, a
Harman/QNX gadget that enumerates as `8086:9999`, "Gadget MFi v0.1"). It fails
in three ways, and the patch adds one optional switch.

`cdc_ncm/cdc_ncm-rascar.patch` is a plain diff against Linux **6.18.20**
`drivers/net/usb/cdc_ncm.c`. `cdc_ncm/build.sh` builds it for the running
kernel. It also applies cleanly to nearby 6.x kernels, because these parts of
the file rarely change.

## What the patch changes

1. **The MAC address string is not valid hex: use a random MAC instead of giving up.**
   The NCM functional descriptor's `iMACAddress` points at a string descriptor
   that should contain 12 hex digits. The car's string is not a MAC. Stock
   `cdc_ncm_bind_common()` treats this as fatal ("failed to get mac address"), so
   no network interface appears, there is no IPv6 link, and AirPlay never
   starts. The patched driver calls `eth_hw_addr_random()` and continues. A
   locally administered address is fine here because the link only carries
   IPv6 link-local traffic.

2. **Wait 220 ms before the alt-setting switch.**
   After `SET_CONFIGURATION`, the stock driver waits 10-20 ms
   (`usleep_range(10000, 20000)`, a value tuned on a Sierra Wireless modem)
   before it selects the data interface's alternate setting. usbmon captures
   showed that this head unit faults when the request comes that soon. The next
   control transfer fails with `-71` (EPROTO), the device drops off the bus, and
   it then cannot be configured again ("can't set config #1, error -71"). The
   patch waits 220 ms, the delay a shipping CarPlay dongle's own patched driver
   uses.

3. **Use iPhone-sized NTB buffers.**
   The initial NTB sizes are capped at 12288 bytes (receive) and 15224 bytes
   (transmit). These are the values a shipping CarPlay dongle negotiates with
   Apple gadgets. They replace upstream's larger generic defaults, which this
   class of device does not honour.

4. **`hold_apple_gadget` module parameter (off by default).**
   While `/sys/module/cdc_ncm/parameters/hold_apple_gadget` is `1`,
   `cdc_ncm_bind()` returns `-ENODEV` before it sends anything to the device.
   The driver therefore leaves the car's gadget alone while userspace runs the
   iAP2 identification and authentication on the same device. Afterwards,
   userspace clears the parameter and binds the interface explicitly through
   `/sys/bus/usb/drivers/cdc_ncm/bind`. `rascar-carplay` handles both steps when
   `RASCAR_NCM_HOLD=1` (see `carplay/src/host/mod.rs`: `engage_ncm_hold`,
   `release_ncm_hold_and_bind`).

   Why the parameter exists: in early attempts, the driver's first class
   requests sometimes reached the gadget while it was still initialising. The
   bind then failed, or the hub disabled the port ("disabled by hub (EMI?)")
   1-12 ms later. A module parameter was used because the other options did not
   work on this board. `drivers_autoprobe=0` is global, so the car's device
   would never get configured. Unloading `cdc_ncm` is not reliable once it is
   resident.

   Current status: **off by default.** In car tests the port was still disabled
   with `cdc_ncm` completely untouched, so the driver was not the cause, and
   attempts with the hold engaged authenticated less often. The parameter stays
   in the patch as an A/B switch.

The patch also adds `MODULE_VERSION("rascar-3")` and a few `dev_info` lines
(`RasCar: ...`) to the kernel log. The runner reads
`/sys/module/cdc_ncm/version` to tell which module is resident. An older
patched module carries the same "O" (out-of-tree) taint, so the taint alone
cannot tell them apart.

## Building and installing

On the board, with the headers for the running kernel installed
(Armbian: `linux-headers-current-rockchip64`):

```sh
sudo kernel/cdc_ncm/build.sh
```

The script fetches the stock `cdc_ncm.c` for `uname -r`. It takes it from a
matching `/usr/src/linux-source-*` tarball if there is one, and otherwise from
kernel.org. It then applies the patch, builds the module and installs it to:

- `/usr/local/lib/rascar/modules/$(uname -r)/cdc_ncm.ko`, which
  `rascar-carplay-run` loads before each attempt (override the path with
  `RASCAR_CDC_NCM_KO` in `/etc/rascar/carplay.env`);
- `/lib/modules/$(uname -r)/updates/cdc_ncm.ko` plus `depmod`, so the module the
  kernel autoloads is the patched one too (`--no-autoload` skips this).

Armbian's header packages are built on x86 and can lack the arm64 host tools
(`scripts/basic/fixdep`, `scripts/mod/modpost`). When they are missing, the
script runs `make modules_prepare` in the headers tree. It writes a
`localversion` file first: without it the regenerated `kernel.release` loses
the `-current-rockchip64` suffix, the module's vermagic no longer matches the
kernel, and insmod fails with "Invalid module format".

**Rebuild after every kernel upgrade.** The runner logs `cdc_ncm WANTED ... but
... is resident` when it cannot load the wanted module. That happens when the
resident module cannot be unloaded, and a reboot fixes it.

Check what is loaded:

```sh
cat /sys/module/cdc_ncm/version                      # rascar-3
cat /sys/module/cdc_ncm/parameters/hold_apple_gadget # N
dmesg | grep -i 'rascar\|cdc_ncm'
```

#!/usr/bin/env python3
"""Bench-only touch bridge: click the receiver window, drive the dashboard.

    ############################################################
    #  THIS IS NOT CARPLAY INPUT. IT BYPASSES THE PROTOCOL.    #
    ############################################################

Why this exists
---------------
On a laptop the CarPlay sender is exercised against an AirPlay receiver such as
UxPlay (see `--airplay-endpoint` on `rascar-carplay`). AirPlay *mirroring* is a
one-way protocol: it carries video to the receiver and has no reverse-touch
channel. Touch-back is a CarPlay extension, so a plain AirPlay receiver never
sends input and `carplay/src/stream/input.rs` never receives any.

The result is a live picture you cannot click. This tool closes that loop
*outside* the protocol: it watches the pointer on the desktop, and when a click
lands inside the receiver window it injects the equivalent click into the X
display the dashboard is actually rendered on. The dashboard reacts, and the
change flows back to the receiver through the real capture/encode/encrypt/RTSP
pipeline.

    click on :0 (receiver window)
        -> [this shim] map coords
        -> XTest click on :99 (dashboard)
        -> dashboard repaints
        -> x11grab -> H.264 -> AirPlay packetise -> encrypt -> RTSP
        -> receiver window updates

What it proves and does NOT prove
---------------------------------
PROVES:      the dashboard is interactive, and the whole video path is live and
             low-latency enough to feel like a head unit.
DOES NOT     anything about the real CarPlay reverse-touch wire format, which
PROVE:       is still unverified (`TODO(hw)` in stream/input.rs) and still
             gated off behind RASCAR_TOUCH_PARSER=experimental. Do not let a
             working demo here be recorded as touch-input working.

Requirements: xinput, xdotool, xwininfo (all X11 command-line tools).

Usage
-----
    tools/bench_touch_bridge.py                       # defaults below
    tools/bench_touch_bridge.py --window-name RasCarTest \
        --dst-display :99 --dst-size 800x480 --src-display :0
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import time

BANNER = r"""
+----------------------------------------------------------------+
|  BENCH TOUCH BRIDGE - protocol BYPASS, not CarPlay input.       |
|  Clicks are read from the desktop and injected into the         |
|  dashboard's X display directly. The CarPlay reverse-touch      |
|  path is NOT exercised and remains unverified.                  |
+----------------------------------------------------------------+
"""


def xenv(display):
    """Inherit the caller's environment, overriding only DISPLAY.

    A stripped env would drop XAUTHORITY, which mutter's Xwayland (:0 on a
    Wayland desktop) requires — every X call would fail with
    "Authorization required, but no authorization protocol specified".
    """
    env = dict(os.environ)
    env["DISPLAY"] = display
    return env


def need(tool):
    if shutil.which(tool) is None:
        sys.exit(f"error: {tool!r} not found on PATH; this tool needs it")


def find_window(src_display, name):
    """Return the X window id (int) of the receiver window, or None."""
    try:
        out = subprocess.run(
            ["wmctrl", "-l"],
            env=xenv(src_display),
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return None
    for line in out.splitlines():
        if name.lower() in line.lower():
            return int(line.split()[0], 16)
    return None


def geometry(src_display, win_id):
    """Absolute (x, y, w, h) of a window, or None if it has gone away."""
    try:
        out = subprocess.run(
            ["xwininfo", "-id", str(win_id)],
            env=xenv(src_display),
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return None
    g = {}
    for key, pat in (
        ("x", r"Absolute upper-left X:\s+(-?\d+)"),
        ("y", r"Absolute upper-left Y:\s+(-?\d+)"),
        ("w", r"Width:\s+(\d+)"),
        ("h", r"Height:\s+(\d+)"),
    ):
        m = re.search(pat, out)
        if not m:
            return None
        g[key] = int(m.group(1))
    return g["x"], g["y"], g["w"], g["h"]


def pointer(src_display):
    """Root-relative pointer position as (x, y)."""
    out = subprocess.run(
        ["xdotool", "getmouselocation", "--shell"],
        env=xenv(src_display),
        capture_output=True, text=True, timeout=5,
    ).stdout
    vals = dict(
        line.split("=", 1) for line in out.splitlines() if "=" in line
    )
    return int(vals["X"]), int(vals["Y"])


def map_click(px, py, win, dst_w, dst_h):
    """Map a root-relative click to dashboard coords, honouring letterboxing.

    The receiver scales the video to fit its window while preserving aspect, so
    unless the window happens to match the source aspect there are bars on two
    sides. Ignoring them puts every click at the wrong place, increasingly so
    towards the edges.
    """
    wx, wy, ww, wh = win
    if ww <= 0 or wh <= 0:
        return None
    a_src = dst_w / dst_h
    a_win = ww / wh
    if a_win > a_src:            # pillarbox: bars left and right
        vh = wh
        vw = wh * a_src
        ox = (ww - vw) / 2.0
        oy = 0.0
    else:                        # letterbox: bars top and bottom
        vw = ww
        vh = ww / a_src
        ox = 0.0
        oy = (wh - vh) / 2.0
    rx = px - wx - ox
    ry = py - wy - oy
    if not (0 <= rx < vw and 0 <= ry < vh):
        return None              # in the window but on a bar, or outside it
    return int(rx / vw * dst_w), int(ry / vh * dst_h)


def inject(dst_display, x, y):
    subprocess.run(
        ["xdotool", "mousemove", str(x), str(y), "click", "1"],
        env=xenv(dst_display),
        timeout=5,
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--window-name", default="RasCarTest",
                    help="substring of the receiver window title (default: RasCarTest)")
    ap.add_argument("--src-display", default=":0",
                    help="display the receiver window is on (default: :0)")
    ap.add_argument("--dst-display", default=":99",
                    help="display the dashboard is rendered on (default: :99)")
    ap.add_argument("--dst-size", default="800x480",
                    help="dashboard resolution (default: 800x480)")
    ap.add_argument("--button", type=int, default=1,
                    help="pointer button to mirror (default: 1, left)")
    args = ap.parse_args()

    for tool in ("xinput", "xdotool", "xwininfo", "wmctrl"):
        need(tool)

    dst_w, dst_h = (int(v) for v in args.dst_size.lower().split("x"))

    print(BANNER)
    win_id = find_window(args.src_display, args.window_name)
    if win_id is None:
        sys.exit(f"error: no window matching {args.window_name!r} on {args.src_display}. "
                 f"Is the receiver running?")
    win = geometry(args.src_display, win_id)
    if win is None:
        sys.exit("error: could not read window geometry")
    print(f"receiver window 0x{win_id:08x} at {win[0]},{win[1]} {win[2]}x{win[3]} "
          f"on {args.src_display}")
    print(f"dashboard {dst_w}x{dst_h} on {args.dst_display}")
    print("click inside the receiver window; Ctrl-C to stop\n")

    # Raw events tell us a button went down but not where the pointer is, so we
    # query the position on each press. Re-read the geometry periodically too:
    # the window can be moved or resized underneath us.
    proc = subprocess.Popen(
        ["xinput", "test-xi2", "--root"],
        env=xenv(args.src_display),
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
    )
    last_geom = time.monotonic()
    pending_press = False
    n = 0
    try:
        for line in proc.stdout:
            line = line.strip()
            if line.startswith("EVENT type") and "RawButtonPress" in line:
                pending_press = True
                continue
            # The button number arrives on the "detail:" line of that event.
            if pending_press and line.startswith("detail:"):
                pending_press = False
                try:
                    button = int(line.split(":", 1)[1].strip())
                except ValueError:
                    continue
                if button != args.button:
                    continue
                if time.monotonic() - last_geom > 2.0:
                    g = geometry(args.src_display, win_id)
                    if g:
                        win = g
                    last_geom = time.monotonic()
                try:
                    px, py = pointer(args.src_display)
                except Exception:
                    continue
                hit = map_click(px, py, win, dst_w, dst_h)
                if hit is None:
                    continue
                x, y = hit
                n += 1
                print(f"[{n:4d}] desktop ({px},{py}) -> dashboard ({x},{y})")
                inject(args.dst_display, x, y)
    except KeyboardInterrupt:
        pass
    finally:
        proc.terminate()
        print(f"\nstopped after {n} forwarded click(s)")


if __name__ == "__main__":
    main()

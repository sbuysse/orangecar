# Contributing

Thanks for helping. The most useful contributions right now are:

1. **Reports from other cars.** Did the board enumerate? Did the car send the role switch? How far did the
   session get? Fill in the vehicle template in [docs/tested-vehicles.md](docs/tested-vehicles.md) and open
   an issue with the "New vehicle report" template. Attach the run log from `/var/lib/rascar-debug/`
   **after removing anything personal** (the car's VIN can show up in iAP2 identification and in OBD logs).
2. **Other boards.** Anything with a USB-C port that can switch between device and host mode (dwc3 with
   `usb-role-switch`) should work in principle. Porting notes are welcome.
3. **Protocol knowledge.** Corrections or additions to [docs/protocol.md](docs/protocol.md), with the capture
   that proves them.

## Ground rules

- A capture from a real car beats any reference implementation. Say which car and head unit it came from.
- Mark anything you have not verified as unverified.
- Code comments, commit messages and docs are in English.
- Keep `cargo test --workspace` green, run `tools/bench/session_survival_test.sh` after any change in `carplay/`, run `node --check` on changed JavaScript and `bash -n` on shell scripts.
- Do not commit certificates, keys, captures containing VINs, or anything tied to a person.

## Building

See [docs/install.md](docs/install.md). Cross-compiling for the board from a PC:

```bash
cargo install cargo-zigbuild   # and install zig
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36
```

## License

By contributing you agree that your contribution is licensed under the GPL-3.0, like the rest of the project.

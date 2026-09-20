# asus-lamp

Small Rust CLI for ASUS firmware that exposes the standard HID LampArray interface through Windows Dynamic Lighting. It discovers the LampArray report descriptor instead of relying on a changing `hidrawN` number, sets a solid RGB color, then exits.

## Build and use

```sh
cargo build --release
cargo run -- list
cargo run -- -v info --lamps
cargo run -- -v set ff0000 --dry-run
cargo run -- set ff0000
cargo run -- off
```

`info --lamps` only queries feature reports. `set` reads the array and lamp attributes first, disables autonomous mode, waits for the device's required update interval, and sends one range update. `--dry-run` prints the reports without sending either write.

## Permissions

For desktop-session access, install this narrow udev rule as `/etc/udev/rules.d/99-asus-lamp.rules`:

```udev
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0b05", ATTRS{idProduct}=="18f3", TAG+="uaccess"
```

Reload it with `sudo udevadm control --reload-rules`, then reconnect the device or reboot. The program validates the HID report descriptor before opening it, so it rejects the other, proprietary ASUS interface on the same controller.

## Verified device

On the ASUS `0b05:18f3` controller used during development, the LampArray reports one programmable chassis/accent lamp with 255 RGB levels and on/off intensity. It controls the motherboard and attached/synchronized GPU lighting together.

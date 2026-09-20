# lightset

Small Rust CLI for the ASUS HID LampArray interface and ENE DRAM RGB controllers. It sets one solid RGB color, then exits; no daemon, OpenRGB process, or proprietary ASUS HID protocol is used.

## Build and use

```sh
cargo build --release
./target/release/lightset set ff0000
./target/release/lightset off
```

These commands update every configured backend, then exit. The DRAM bus is conservatively discovered by looking only at configured SMBus adapter names and configured ENE addresses.

## Hardware profiles

The verified hardware is defined in [src/profile.rs](src/profile.rs). To support another machine, add a profile there rather than changing protocol code. A profile specifies:

- optional LampArray VID/PID constraints (or `None` for any descriptor-validated LampArray);
- permitted SMBus adapter-name patterns and I²C addresses;
- each accepted ENE version, LED count, direct-color register/layout, and Direct-mode write sequence.

The program will not probe unlisted SMBus addresses or write to a controller whose identifier and LED count do not exactly match a profile.

## Backends and safety

The HID backend validates the Lighting and Illumination/LampArray usage in the report descriptor, reads the array and lamp attributes, disables autonomous mode, respects the reported update interval, and sends one range update. This controls the motherboard, synchronized header lighting, and GPU lighting on the verified system.

The DRAM backend is deliberately limited to ENE `AUDA0-E6K5-0101` controllers reporting exactly eight LEDs. It follows the Linux SMBus framing used by OpenRGB: select the 16-bit register through SMBus command `0x00` (with the register byte-swapped for the Linux SMBus word ABI), then read through command `0x81`. Direct colors are stored at `0x8100` in `R,B,G` order.

The Direct write sequence is:

1. Write `01` to `0x8020` and `0x80a0` to enable and apply Direct mode.
2. Write eight 3-byte `R,B,G` blocks from `0x8100` through `0x8115`.
3. Do not send a trailing apply or any save-to-flash command.

Keep OpenRGB stopped while using `lightset`. The tool never scans arbitrary SMBus addresses, touches SPD/EEPROM ranges, implements effects, or writes persistent controller state.

## Privilege model

Run `lightset` as root. Do not grant users access to the HID or I²C devices.

## Verified device

On the ASUS `0b05:18f3` controller used during development, the LampArray reports one programmable chassis/accent lamp with 255 RGB levels and on/off intensity. It controls the motherboard and attached/synchronized GPU lighting together. Four `AUDA0-E6K5-0101` ENE DRAM controllers at `0x70`–`0x73` each expose eight LEDs; ENE direct-color memory uses `R,B,G` byte order.

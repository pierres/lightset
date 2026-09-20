# lightset

Set a solid RGB color on supported ASUS Aura lighting and ENE RGB DRAM from Linux.

## Build and use

```sh
cargo build --release
./target/release/lightset set ff0000
./target/release/lightset off
```

Run `lightset` as root. Do not grant users access to the HID or I²C devices.

## Requirements

On a compatible ASUS motherboard, enable **Windows Dynamic Lighting** in the BIOS. This exposes the Aura controller through the standard HID LampArray interface used by `lightset`.

The current profile supports the ASUS Aura controller and ENE RGB DRAM controllers verified during development. It updates the motherboard, synchronized headers and GPU lighting, plus all configured DRAM modules. The profile only probes configured SMBus adapters and addresses, and validates each controller before writing.

To support different hardware, add a profile in [src/profile.rs](src/profile.rs).

## Scope

This is a personal tool for the author's hardware. It is published as open source, but is not intended for general use, compatibility requests, feature requests, or support. Use it only at your own risk.

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

The current profile supports the ASUS Aura controller and ENE RGB DRAM controllers verified during development. It updates the motherboard, synchronized headers and GPU lighting, plus supported DRAM modules found at the configured addresses. Empty DRAM addresses are normal. The profile only probes configured SMBus adapters and addresses, and validates each controller before writing. DRAM address mapping at `0x77` is attempted only when that device reports a supported controller identity.

To support different hardware, add a profile in [src/profile.rs](src/profile.rs).

## Hardware check

After a protocol change, use a debug build to inspect the configured DRAM addresses and mapper without changing lighting:

```sh
cargo build
sudo ./target/debug/lightset probe
```

Then check `set` and `off` with the release build. Confirm that every installed supported DIMM appears in the output and that both commands exit successfully. The `probe` command selects ENE registers to read them, so it still needs I²C write access. It does not change colors or address mappings.

## Scope

This is a personal tool for the author's hardware. It is published as open source, but is not intended for general use, compatibility requests, feature requests, or support. Use it only at your own risk.

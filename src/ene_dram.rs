//! ENE DRAM SMBus protocol, faithfully following OpenRGB's Linux
//! `ENESMBusInterface_i2c_smbus` transport. A 16-bit ENE register is selected
//! with an SMBus word write to command 0x00; the Linux SMBus ABI serializes a
//! word least-significant byte first, so the register value is byte-swapped.
//! The selected byte is then read through command 0x81.
use crate::profile::{EneControllerProfile, EneDramProfile};
use transport::SmbusTransport;

mod transport;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

const DEVICE_NAME_REGISTER: u16 = 0x1000;
const CONFIG_TABLE_REGISTER: u16 = 0x1c00;
const CONFIG_LED_COUNT: usize = 0x02;
const DRAM_MAPPER_ADDRESS: u16 = 0x77;
const DRAM_MAPPER_SLOT_REGISTER: u16 = 0x80f8;
const DRAM_MAPPER_ADDRESS_REGISTER: u16 = 0x80f9;

#[derive(Debug, Clone, PartialEq, Eq)]
struct EneDramDevice {
    address: u16,
    controller: EneControllerProfile,
}

struct ProbeReport {
    devices: Vec<EneDramDevice>,
    failures: Vec<(u16, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegisterWrite {
    Byte { register: u16, value: u8 },
    Block { register: u16, data: Vec<u8> },
}

#[derive(Debug)]
pub struct DeviceWriteResult {
    pub address: u16,
    pub result: Result<(), String>,
}

/// ENE devices validated on a single SMBus and ready for a color write.
#[derive(Debug)]
pub struct PreparedDram {
    bus: PathBuf,
    devices: Vec<EneDramDevice>,
    probe_failures: Vec<(u16, String)>,
}

impl PreparedDram {
    pub fn probe_failures(&self) -> &[(u16, String)] {
        &self.probe_failures
    }
}

/// A non-mutating report of what each configured ENE address returned.
#[cfg(debug_assertions)]
#[derive(Debug)]
pub struct BusDiagnostic {
    pub bus: PathBuf,
    pub adapter_name: String,
    pub open_error: Option<String>,
    pub addresses: Vec<AddressDiagnostic>,
}

#[cfg(debug_assertions)]
#[derive(Debug)]
pub struct AddressDiagnostic {
    pub address: u16,
    pub present: bool,
    pub version: Option<String>,
    pub led_count: Option<u8>,
    pub error: Option<String>,
    pub supported: bool,
}

fn probe(bus: &Path, profile: &EneDramProfile) -> Result<ProbeReport> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bus)
        .with_context(|| {
            format!(
                "open {} (SMBus reads require write access to select an ENE register)",
                bus.display()
            )
        })?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    Ok(probe_on_transport(&transport, profile))
}

fn probe_on_transport(transport: &impl EneTransport, profile: &EneDramProfile) -> ProbeReport {
    let mut devices = Vec::new();
    let mut failures = Vec::new();
    for &address in profile.addresses {
        match probe_address(transport, address, profile) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(error) => failures.push((address, format!("{error:#}"))),
        }
    }
    ProbeReport { devices, failures }
}

/// Prepare all validated ENE DRAM devices before changing lighting.
/// Discovery is conservative: only configured PIIX4 adapters and addresses are
/// probed.
pub fn prepare(profile: &EneDramProfile) -> Result<PreparedDram> {
    validate_profile(profile)?;
    let class = Path::new("/sys/class/i2c-dev");
    let mut probe_failures = Vec::new();
    for entry in adapter_entries(class)? {
        let name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if !profile
            .adapter_name_patterns
            .iter()
            .any(|pattern| name.contains(pattern))
        {
            continue;
        }
        let bus = Path::new("/dev").join(entry.file_name());
        remap_dram_devices(&bus, profile)
            .with_context(|| format!("check ENE mapping on {}", bus.display()))?;
        let report = probe(&bus, profile)?;
        if !report.devices.is_empty() {
            return Ok(PreparedDram {
                bus,
                devices: report.devices,
                probe_failures: report.failures,
            });
        }
        probe_failures.extend(
            report
                .failures
                .into_iter()
                .map(|(address, error)| format!("{} 0x{address:02x}: {error}", bus.display())),
        );
    }
    ensure!(
        probe_failures.is_empty(),
        "no supported ENE DRAM controllers found; probe failures: {}",
        probe_failures.join("; ")
    );
    bail!("no supported ENE DRAM controllers found on a configured SMBus adapter")
}

fn adapter_entries(class: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<_> = fs::read_dir(class)
        .context("read /sys/class/i2c-dev")?
        .collect::<io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

/// Discover a supported ENE DRAM bus without retaining the validated devices.
pub fn discover_bus(profile: &EneDramProfile) -> Result<PathBuf> {
    Ok(prepare(profile)?.bus)
}

/// ENE DRAM controllers can boot behind a mapper at 0x77 instead of having
/// individual addresses. Assign each configured slot its stable address before
/// probing. This follows OpenRGB's ENE SMBus DRAM discovery sequence.
fn remap_dram_devices(bus: &Path, profile: &EneDramProfile) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(bus)?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    remap_on_transport(&transport, profile)
}

fn remap_on_transport(transport: &impl EneTransport, profile: &EneDramProfile) -> Result<()> {
    if probe_address(transport, DRAM_MAPPER_ADDRESS, profile)?.is_none() {
        return Ok(());
    }
    for (slot, address) in mapper_assignments(profile.addresses, |address| {
        address_responds(transport, address)
    })? {
        if !address_responds(transport, DRAM_MAPPER_ADDRESS)? {
            break;
        }
        transport.write_register(DRAM_MAPPER_ADDRESS, DRAM_MAPPER_SLOT_REGISTER, slot as u8)?;
        transport.write_register(
            DRAM_MAPPER_ADDRESS,
            DRAM_MAPPER_ADDRESS_REGISTER,
            (address << 1) as u8,
        )?;
    }
    Ok(())
}

fn mapper_assignments(
    addresses: &[u16],
    mut address_is_mapped: impl FnMut(u16) -> Result<bool>,
) -> Result<Vec<(usize, u16)>> {
    addresses
        .iter()
        .enumerate()
        .filter_map(|(slot, &address)| match address_is_mapped(address) {
            Ok(false) => Some(Ok((slot, address))),
            Ok(true) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn address_responds(transport: &impl EneTransport, address: u16) -> Result<bool> {
    match transport.read_address_byte(address) {
        Ok(_) => Ok(true),
        Err(error) if is_absent(&error) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("probe address 0x{address:02x}")),
    }
}

fn is_absent(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::raw_os_error)
        .is_some_and(|code| matches!(code, libc::ENXIO | libc::EREMOTEIO | libc::ENODEV))
}

/// Read controller identity without changing colors or address mappings.
/// Register selection still requires I²C write access.
#[cfg(debug_assertions)]
pub fn diagnose(profile: &EneDramProfile) -> Result<Vec<BusDiagnostic>> {
    let class = Path::new("/sys/class/i2c-dev");
    let mut buses = Vec::new();
    for entry in adapter_entries(class)? {
        let adapter_name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if !profile
            .adapter_name_patterns
            .iter()
            .any(|pattern| adapter_name.contains(pattern))
        {
            continue;
        }
        let bus = Path::new("/dev").join(entry.file_name());
        let file = match OpenOptions::new().read(true).write(true).open(&bus) {
            Ok(file) => file,
            Err(error) => {
                buses.push(BusDiagnostic {
                    bus,
                    adapter_name: adapter_name.trim_end().to_owned(),
                    open_error: Some(format!("{error:#}")),
                    addresses: Vec::new(),
                });
                continue;
            }
        };
        let transport = SmbusTransport {
            fd: file.as_raw_fd(),
        };
        let mut addresses: Vec<_> = profile
            .addresses
            .iter()
            .map(|&address| diagnose_address(&transport, address, profile))
            .collect();
        addresses.push(diagnose_address(&transport, DRAM_MAPPER_ADDRESS, profile));
        buses.push(BusDiagnostic {
            bus,
            adapter_name: adapter_name.trim_end().to_owned(),
            open_error: None,
            addresses,
        });
    }
    Ok(buses)
}

#[cfg(debug_assertions)]
fn diagnose_address(
    transport: &impl EneTransport,
    address: u16,
    profile: &EneDramProfile,
) -> AddressDiagnostic {
    match address_responds(transport, address) {
        Ok(true) => {}
        Ok(false) => {
            return AddressDiagnostic {
                address,
                present: false,
                version: None,
                led_count: None,
                error: None,
                supported: false,
            };
        }
        Err(error) => {
            return AddressDiagnostic {
                address,
                present: false,
                version: None,
                led_count: None,
                error: Some(format!("check address: {error:#}")),
                supported: false,
            };
        }
    }
    let name = match read_bytes(transport, address, DEVICE_NAME_REGISTER, 16) {
        Ok(name) => String::from_utf8_lossy(&name)
            .trim_end_matches('\0')
            .to_owned(),
        Err(error) => {
            return AddressDiagnostic {
                address,
                present: true,
                version: None,
                led_count: None,
                error: Some(format!("read device name: {error:#}")),
                supported: false,
            };
        }
    };
    let config = match read_bytes(transport, address, CONFIG_TABLE_REGISTER, 64) {
        Ok(config) => config,
        Err(error) => {
            return AddressDiagnostic {
                address,
                present: true,
                version: Some(name),
                led_count: None,
                error: Some(format!("read configuration table: {error:#}")),
                supported: false,
            };
        }
    };
    let led_count = config[CONFIG_LED_COUNT];
    let supported = profile
        .controllers
        .iter()
        .any(|controller| controller.version == name && controller.led_count == led_count);
    AddressDiagnostic {
        address,
        present: true,
        version: Some(name),
        led_count: Some(led_count),
        error: None,
        supported,
    }
}

fn set_color(transport: &impl EneTransport, device: &EneDramDevice, rgb: [u8; 3]) -> Result<()> {
    let writes = direct_color_writes(device.controller, rgb);
    for write in &writes {
        match write {
            RegisterWrite::Byte { register, value } => {
                transport.write_register(device.address, *register, *value)?
            }
            RegisterWrite::Block { register, data } => {
                transport.write_register_block(device.address, *register, data)?
            }
        }
    }
    Ok(())
}

/// Attempt every prepared controller; one DIMM failing does not stop the rest.
pub fn set_prepared_colors(
    prepared: &PreparedDram,
    rgb: [u8; 3],
) -> Result<Vec<DeviceWriteResult>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&prepared.bus)
        .with_context(|| format!("open {}", prepared.bus.display()))?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    Ok(write_devices(&transport, &prepared.devices, rgb))
}

fn write_devices(
    transport: &impl EneTransport,
    devices: &[EneDramDevice],
    rgb: [u8; 3],
) -> Vec<DeviceWriteResult> {
    devices
        .iter()
        .map(|device| DeviceWriteResult {
            address: device.address,
            result: set_color(transport, device, rgb).map_err(|error| format!("{error:#}")),
        })
        .collect()
}

/// Discover devices on one bus and write them. Prefer [`prepare`] followed by
/// [`set_prepared_colors`] when the visible write should happen without a
/// second discovery pass.
pub fn set_all_colors(
    bus: &Path,
    profile: &EneDramProfile,
    rgb: [u8; 3],
) -> Result<Vec<DeviceWriteResult>> {
    validate_profile(profile)?;
    let report = probe(bus, profile)?;
    ensure!(
        !report.devices.is_empty(),
        "no supported ENE DRAM controllers found on {}",
        bus.display()
    );
    ensure!(
        report.failures.is_empty(),
        "failed to probe ENE addresses: {:?}",
        report.failures
    );
    set_prepared_colors(
        &PreparedDram {
            bus: bus.to_owned(),
            devices: report.devices,
            probe_failures: report.failures,
        },
        rgb,
    )
}

fn validate_profile(profile: &EneDramProfile) -> Result<()> {
    ensure!(
        !profile.adapter_name_patterns.is_empty()
            && profile
                .adapter_name_patterns
                .iter()
                .all(|name| !name.is_empty()),
        "ENE adapter names must be specified"
    );
    ensure!(!profile.addresses.is_empty(), "ENE address list is empty");
    ensure!(
        !profile.controllers.is_empty(),
        "ENE controller list is empty"
    );
    ensure!(
        profile.addresses.len() <= usize::from(u8::MAX) + 1,
        "too many ENE mapper slots"
    );
    for (index, &address) in profile.addresses.iter().enumerate() {
        ensure!(
            (0x03..DRAM_MAPPER_ADDRESS).contains(&address),
            "invalid ENE DRAM address 0x{address:02x}"
        );
        ensure!(
            !profile.addresses[..index].contains(&address),
            "duplicate ENE DRAM address 0x{address:02x}"
        );
    }
    for controller in profile.controllers {
        ensure!(!controller.version.is_empty(), "ENE version is empty");
        ensure!(controller.led_count > 0, "ENE controller has zero LEDs");
        ensure!(
            (1..=32).contains(&controller.block_size),
            "ENE block size must be 1..=32"
        );
        let mut order = controller.direct_color_order;
        order.sort_unstable();
        ensure!(order == [0, 1, 2], "ENE color order must be a permutation");
        ensure!(
            u32::from(controller.direct_color_register) + u32::from(controller.led_count) * 3
                <= u32::from(u16::MAX) + 1,
            "ENE color registers overflow"
        );
    }
    Ok(())
}

/// OpenRGB's GUI Direct path calls SetDirect(true), which writes Direct then
/// Apply, followed by SetAllColorsDirect. The latter writes 3-byte blocks and
/// deliberately does not issue a further Apply write.
fn direct_color_writes(controller: EneControllerProfile, rgb: [u8; 3]) -> Vec<RegisterWrite> {
    let mut direct_bytes = Vec::with_capacity(usize::from(controller.led_count) * 3);
    for _ in 0..controller.led_count {
        let mut bytes = [0; 3];
        bytes[controller.direct_color_order[0]] = rgb[0];
        bytes[controller.direct_color_order[1]] = rgb[1];
        bytes[controller.direct_color_order[2]] = rgb[2];
        direct_bytes.extend(bytes);
    }
    let mut writes = vec![
        RegisterWrite::Byte {
            register: controller.direct_register,
            value: controller.direct_value,
        },
        RegisterWrite::Byte {
            register: controller.apply_register,
            value: controller.apply_value,
        },
    ];
    for (index, block) in direct_bytes.chunks(controller.block_size).enumerate() {
        writes.push(RegisterWrite::Block {
            register: controller.direct_color_register + (index * controller.block_size) as u16,
            data: block.to_vec(),
        });
    }
    writes
}

fn probe_address(
    transport: &impl EneTransport,
    address: u16,
    profile: &EneDramProfile,
) -> Result<Option<EneDramDevice>> {
    if !address_responds(transport, address)? {
        return Ok(None);
    }
    let name = read_bytes(transport, address, DEVICE_NAME_REGISTER, 16)?;
    let version = String::from_utf8_lossy(&name)
        .trim_end_matches('\0')
        .to_owned();
    let config = read_bytes(transport, address, CONFIG_TABLE_REGISTER, 64)?;
    let led_count = config[CONFIG_LED_COUNT];
    let Some(controller) = profile
        .controllers
        .iter()
        .find(|controller| controller.version == version && controller.led_count == led_count)
    else {
        return Ok(None);
    };
    Ok(Some(EneDramDevice {
        address,
        controller: *controller,
    }))
}

fn read_bytes(
    transport: &impl EneTransport,
    address: u16,
    register: u16,
    count: usize,
) -> Result<Vec<u8>> {
    (0..count)
        .map(|offset| transport.read_register(address, register + offset as u16))
        .collect()
}

/// ENE direct colors are stored in R, B, G order, not ordinary RGB order.
#[cfg(test)]
fn decode_direct_colors(bytes: &[u8], controller: EneControllerProfile) -> Result<Vec<[u8; 3]>> {
    ensure!(
        bytes.len().is_multiple_of(3),
        "ENE direct-color data must be a multiple of three bytes"
    );
    let (chunks, _) = bytes.as_chunks::<3>();
    Ok(chunks
        .iter()
        .map(|chunk| {
            [
                chunk[controller.direct_color_order[0]],
                chunk[controller.direct_color_order[1]],
                chunk[controller.direct_color_order[2]],
            ]
        })
        .collect())
}

trait EneTransport {
    fn read_address_byte(&self, address: u16) -> Result<u8>;
    fn read_register(&self, address: u16, register: u16) -> Result<u8>;
    fn write_register(&self, address: u16, register: u16, value: u8) -> Result<()>;
    fn write_register_block(&self, address: u16, register: u16, data: &[u8]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::transport::{is_block_unsupported, write_block_with_fallback};
    use super::*;
    use crate::profile::DEFAULT_HARDWARE;
    use std::cell::{Cell, RefCell};

    struct FakeTransport {
        supported: Vec<u16>,
        mapper_remaining: Cell<usize>,
        mapper_supported: bool,
        bad_identity: Option<u16>,
        failed_write: Option<u16>,
        writes: RefCell<Vec<(u16, u16, u8)>>,
    }

    impl FakeTransport {
        fn new(supported: Vec<u16>) -> Self {
            Self {
                supported,
                mapper_remaining: Cell::new(0),
                mapper_supported: false,
                bad_identity: None,
                failed_write: None,
                writes: RefCell::new(Vec::new()),
            }
        }

        fn present(&self, address: u16) -> bool {
            self.supported.contains(&address)
                || (address == DRAM_MAPPER_ADDRESS && self.mapper_remaining.get() > 0)
        }
    }

    impl EneTransport for FakeTransport {
        fn read_address_byte(&self, address: u16) -> Result<u8> {
            if self.present(address) {
                Ok(0)
            } else {
                Err(io::Error::from_raw_os_error(libc::ENXIO).into())
            }
        }

        fn read_register(&self, address: u16, register: u16) -> Result<u8> {
            if self.bad_identity == Some(address) {
                return Err(io::Error::from_raw_os_error(libc::EIO).into());
            }
            let version = if address == DRAM_MAPPER_ADDRESS && !self.mapper_supported {
                b"UNKNOWN\0\0\0\0\0\0\0\0\0"
            } else {
                b"AUDA0-E6K5-0101\0"
            };
            if (DEVICE_NAME_REGISTER..DEVICE_NAME_REGISTER + 16).contains(&register) {
                return Ok(version[usize::from(register - DEVICE_NAME_REGISTER)]);
            }
            if register == CONFIG_TABLE_REGISTER + CONFIG_LED_COUNT as u16 {
                return Ok(8);
            }
            Ok(0)
        }

        fn write_register(&self, address: u16, register: u16, value: u8) -> Result<()> {
            self.writes.borrow_mut().push((address, register, value));
            if self.failed_write == Some(address) {
                return Err(io::Error::from_raw_os_error(libc::EIO).into());
            }
            if address == DRAM_MAPPER_ADDRESS && register == DRAM_MAPPER_ADDRESS_REGISTER {
                self.mapper_remaining.set(self.mapper_remaining.get() - 1);
            }
            Ok(())
        }

        fn write_register_block(&self, address: u16, _register: u16, _data: &[u8]) -> Result<()> {
            if self.failed_write == Some(address) {
                return Err(io::Error::from_raw_os_error(libc::EIO).into());
            }
            Ok(())
        }
    }

    #[test]
    fn ene_register_word_is_byte_swapped_for_linux_smbus() {
        assert_eq!(0x8100_u16.swap_bytes(), 0x0081);
    }
    #[test]
    fn known_controller_id_is_exact() {
        assert_eq!(
            DEFAULT_HARDWARE.ene_dram.controllers[0].version,
            "AUDA0-E6K5-0101"
        );
    }

    #[test]
    fn mapper_assigns_each_unmapped_dimm_its_configured_address() {
        let addresses = DEFAULT_HARDWARE.ene_dram.addresses;
        assert_eq!(
            mapper_assignments(addresses, |_| Ok(false)).unwrap(),
            vec![(0, 0x70), (1, 0x71), (2, 0x72), (3, 0x73)]
        );
        assert_eq!(
            mapper_assignments(addresses, |address| Ok(address == 0x71)).unwrap(),
            vec![(0, 0x70), (2, 0x72), (3, 0x73)]
        );
    }
    #[test]
    fn unknown_mapper_is_never_written() {
        let transport = FakeTransport {
            mapper_remaining: Cell::new(2),
            ..FakeTransport::new(vec![])
        };
        remap_on_transport(&transport, &DEFAULT_HARDWARE.ene_dram).unwrap();
        assert!(transport.writes.borrow().is_empty());
    }
    #[test]
    fn mapper_stops_when_no_more_modules_respond() {
        let transport = FakeTransport {
            mapper_remaining: Cell::new(2),
            mapper_supported: true,
            ..FakeTransport::new(vec![])
        };
        remap_on_transport(&transport, &DEFAULT_HARDWARE.ene_dram).unwrap();
        assert_eq!(
            *transport.writes.borrow(),
            vec![
                (0x77, 0x80f8, 0),
                (0x77, 0x80f9, 0xe0),
                (0x77, 0x80f8, 1),
                (0x77, 0x80f9, 0xe2),
            ]
        );
    }
    #[test]
    fn empty_addresses_are_normal_but_identity_errors_are_reported() {
        let transport = FakeTransport {
            bad_identity: Some(0x71),
            ..FakeTransport::new(vec![0x70, 0x71])
        };
        let report = probe_on_transport(&transport, &DEFAULT_HARDWARE.ene_dram);
        assert_eq!(report.devices.len(), 1);
        assert_eq!(report.devices[0].address, 0x70);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].0, 0x71);
    }
    #[test]
    fn diagnostics_distinguish_absent_addresses_from_failures() {
        let transport = FakeTransport {
            bad_identity: Some(0x71),
            ..FakeTransport::new(vec![0x70, 0x71])
        };
        let profile = &DEFAULT_HARDWARE.ene_dram;
        let known = diagnose_address(&transport, 0x70, profile);
        assert!(known.present && known.supported);
        let broken = diagnose_address(&transport, 0x71, profile);
        assert!(broken.present && broken.error.is_some());
        let empty = diagnose_address(&transport, 0x72, profile);
        assert!(!empty.present && empty.error.is_none());
    }
    #[test]
    fn a_failed_dimm_does_not_stop_other_writes() {
        let transport = FakeTransport {
            failed_write: Some(0x70),
            ..FakeTransport::new(vec![0x70, 0x71])
        };
        let report = probe_on_transport(&transport, &DEFAULT_HARDWARE.ene_dram);
        let results = write_devices(&transport, &report.devices, [1, 2, 3]);
        assert!(results[0].result.is_err());
        assert!(results[1].result.is_ok());
        assert!(
            transport
                .writes
                .borrow()
                .iter()
                .any(|write| write.0 == 0x71)
        );
    }
    #[test]
    fn invalid_profiles_are_rejected_before_io() {
        let mut profile = DEFAULT_HARDWARE.ene_dram;
        let mut controller = profile.controllers[0];
        controller.block_size = 0;
        profile.controllers = Box::leak(vec![controller].into_boxed_slice());
        assert!(validate_profile(&profile).is_err());
    }
    #[test]
    fn block_fallback_only_accepts_unsupported_operations() {
        assert!(is_block_unsupported(
            &io::Error::from_raw_os_error(libc::EOPNOTSUPP).into()
        ));
        assert!(!is_block_unsupported(
            &io::Error::from_raw_os_error(libc::EIO).into()
        ));
    }
    #[test]
    fn unsupported_block_write_reselects_register_before_bytes() {
        let calls = RefCell::new(Vec::new());
        write_block_with_fallback(
            0x8100,
            &[1, 2],
            |_| {
                calls.borrow_mut().push("block".to_owned());
                Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP).into())
            },
            |register| {
                calls.borrow_mut().push(format!("select {register:04x}"));
                Ok(())
            },
            |byte| {
                calls.borrow_mut().push(format!("byte {byte}"));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            ["block", "select 8100", "byte 1", "byte 2"]
        );
    }
    #[test]
    fn io_failure_does_not_retry_as_bytes() {
        let calls = RefCell::new(Vec::new());
        let result = write_block_with_fallback(
            0x8100,
            &[1],
            |_| Err(io::Error::from_raw_os_error(libc::EIO).into()),
            |_| {
                calls.borrow_mut().push("select");
                Ok(())
            },
            |_| {
                calls.borrow_mut().push("byte");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(calls.borrow().is_empty());
    }
    #[test]
    fn direct_color_bytes_decode_from_rbg() {
        assert_eq!(
            decode_direct_colors(
                &[0xff, 0x00, 0x00, 0x00, 0xff, 0x00],
                DEFAULT_HARDWARE.ene_dram.controllers[0],
            )
            .unwrap(),
            vec![[0xff, 0x00, 0x00], [0x00, 0x00, 0xff]]
        );
    }
    #[test]
    fn direct_writes_match_openrgb_gui_sequence() {
        let writes = direct_color_writes(DEFAULT_HARDWARE.ene_dram.controllers[0], [0xff, 0, 0]);
        assert_eq!(writes.len(), 10);
        assert_eq!(
            writes[0],
            RegisterWrite::Byte {
                register: 0x8020,
                value: 1
            }
        );
        assert_eq!(
            writes[1],
            RegisterWrite::Byte {
                register: 0x80a0,
                value: 1
            }
        );
        assert_eq!(
            writes[2],
            RegisterWrite::Block {
                register: 0x8100,
                data: vec![0xff, 0, 0]
            }
        );
        assert_eq!(
            writes[9],
            RegisterWrite::Block {
                register: 0x8115,
                data: vec![0xff, 0, 0]
            }
        );
    }
}

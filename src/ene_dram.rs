//! ENE DRAM SMBus protocol, faithfully following OpenRGB's Linux
//! `ENESMBusInterface_i2c_smbus` transport. A 16-bit ENE register is selected
//! with an SMBus word write to command 0x00; the Linux SMBus ABI serializes a
//! word least-significant byte first, so the register value is byte-swapped.
//! The selected byte is then read through command 0x81.
use crate::profile::{EneControllerProfile, EneDramProfile};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
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

/// A non-mutating report of what each configured ENE address returned.
#[derive(Debug)]
pub struct BusDiagnostic {
    pub bus: PathBuf,
    pub adapter_name: String,
    pub open_error: Option<String>,
    pub addresses: Vec<AddressDiagnostic>,
}

#[derive(Debug)]
pub struct AddressDiagnostic {
    pub address: u16,
    pub version: Option<String>,
    pub led_count: Option<u8>,
    pub error: Option<String>,
    pub supported: bool,
}

fn probe(bus: &Path, profile: &EneDramProfile) -> Result<Vec<EneDramDevice>> {
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
    let mut devices = Vec::new();
    for &address in profile.addresses {
        match probe_address(&transport, address, profile) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(_) => {}
        }
    }
    Ok(devices)
}

/// Discover conservatively: only AMD PIIX4-style SMBus adapters are considered,
/// and only the four ENE addresses used by this platform are probed.
pub fn discover_bus(profile: &EneDramProfile) -> Result<PathBuf> {
    let class = Path::new("/sys/class/i2c-dev");
    for entry in fs::read_dir(class).context("read /sys/class/i2c-dev")? {
        let entry = entry?;
        let name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if !profile
            .adapter_name_patterns
            .iter()
            .any(|pattern| name.contains(pattern))
        {
            continue;
        }
        let bus = Path::new("/dev").join(entry.file_name());
        remap_dram_devices(&bus, profile)?;
        if !probe(&bus, profile)?.is_empty() {
            return Ok(bus);
        }
    }
    bail!("no supported ENE DRAM controllers found on a configured SMBus adapter")
}

/// ENE DRAM controllers can boot behind a mapper at 0x77 instead of having
/// individual addresses. Assign each configured slot its stable address before
/// probing. This follows OpenRGB's ENE SMBus DRAM discovery sequence.
fn remap_dram_devices(bus: &Path, profile: &EneDramProfile) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(bus)?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    if transport.read_address_byte(DRAM_MAPPER_ADDRESS).is_err() {
        return Ok(());
    }
    for (slot, address) in mapper_assignments(profile.addresses, |address| {
        transport.read_address_byte(address).is_ok()
    }) {
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
    mut address_is_mapped: impl FnMut(u16) -> bool,
) -> Vec<(usize, u16)> {
    addresses
        .iter()
        .enumerate()
        .filter_map(|(slot, &address)| (!address_is_mapped(address)).then_some((slot, address)))
        .collect()
}

/// Read, but never write, all configured controller identity fields. This is
/// intended for diagnosing a controller that no longer matches its profile.
pub fn diagnose(profile: &EneDramProfile) -> Result<Vec<BusDiagnostic>> {
    let class = Path::new("/sys/class/i2c-dev");
    let mut buses = Vec::new();
    for entry in fs::read_dir(class).context("read /sys/class/i2c-dev")? {
        let entry = entry?;
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
        let addresses = profile
            .addresses
            .iter()
            .map(|&address| diagnose_address(&transport, address, profile))
            .collect();
        buses.push(BusDiagnostic {
            bus,
            adapter_name: adapter_name.trim_end().to_owned(),
            open_error: None,
            addresses,
        });
    }
    Ok(buses)
}

fn diagnose_address(
    transport: &SmbusTransport,
    address: u16,
    profile: &EneDramProfile,
) -> AddressDiagnostic {
    let name = match read_bytes(transport, address, DEVICE_NAME_REGISTER, 16) {
        Ok(name) => String::from_utf8_lossy(&name)
            .trim_end_matches('\0')
            .to_owned(),
        Err(error) => {
            return AddressDiagnostic {
                address,
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
        version: Some(name),
        led_count: Some(led_count),
        error: None,
        supported,
    }
}

fn set_color(bus: &Path, profile: &EneDramProfile, address: u16, rgb: [u8; 3]) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bus)
        .with_context(|| format!("open {}", bus.display()))?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    let device = probe_address(&transport, address, profile)?
        .with_context(|| format!("0x{address:02x} is not a supported ENE DRAM controller"))?;
    let writes = direct_color_writes(device.controller, rgb);
    for write in &writes {
        match write {
            RegisterWrite::Byte { register, value } => {
                transport.write_register(address, *register, *value)?
            }
            RegisterWrite::Block { register, data } => {
                transport.write_register_block(address, *register, data)?
            }
        }
    }
    Ok(())
}

/// Attempt every validated controller; one DIMM failing does not stop the rest.
pub fn set_all_colors(
    bus: &Path,
    profile: &EneDramProfile,
    rgb: [u8; 3],
) -> Result<Vec<DeviceWriteResult>> {
    let devices = probe(bus, profile)?;
    ensure!(
        !devices.is_empty(),
        "no supported ENE DRAM controllers found on {}",
        bus.display()
    );
    Ok(devices
        .into_iter()
        .map(|device| DeviceWriteResult {
            address: device.address,
            result: set_color(bus, profile, device.address, rgb)
                .map_err(|error| format!("{error:#}")),
        })
        .collect())
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
    transport: &SmbusTransport,
    address: u16,
    profile: &EneDramProfile,
) -> Result<Option<EneDramDevice>> {
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
    transport: &SmbusTransport,
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

struct SmbusTransport {
    fd: std::os::fd::RawFd,
}

impl SmbusTransport {
    fn read_address_byte(&self, address: u16) -> Result<u8> {
        self.set_address(address)?;
        let mut data = SmbusData { byte: 0 };
        self.access_ptr(I2C_SMBUS_READ, 0, I2C_SMBUS_BYTE, &mut data)?;
        // SAFETY: `access_ptr` initialized the byte field for an SMBus byte read.
        Ok(unsafe { data.byte })
    }
    fn read_register(&self, address: u16, register: u16) -> Result<u8> {
        self.set_address(address)?;
        self.write_word(0x00, register.swap_bytes())?;
        self.read_byte(0x81)
    }
    fn write_register(&self, address: u16, register: u16, value: u8) -> Result<()> {
        self.set_address(address)?;
        self.write_word(0x00, register.swap_bytes())?;
        self.write_byte(0x01, value)
    }
    fn write_register_block(&self, address: u16, register: u16, data: &[u8]) -> Result<()> {
        ensure!(
            !data.is_empty() && data.len() <= 32,
            "ENE block must contain 1..=32 bytes"
        );
        self.set_address(address)?;
        self.write_word(0x00, register.swap_bytes())?;
        if self.write_block(0x03, data).is_err() {
            for byte in data {
                self.write_byte(0x01, *byte)?;
            }
        }
        Ok(())
    }
    fn set_address(&self, address: u16) -> Result<()> {
        ensure!(
            (0x03..=0x77).contains(&address),
            "invalid 7-bit I²C address 0x{address:02x}"
        );
        ioctl(
            self.fd,
            I2C_SLAVE,
            address as libc::c_ulong,
            "select I²C slave",
        )
    }
    fn write_word(&self, command: u8, value: u16) -> Result<()> {
        self.access(
            I2C_SMBUS_WRITE,
            command,
            I2C_SMBUS_WORD_DATA,
            SmbusData { word: value },
        )
    }
    fn read_byte(&self, command: u8) -> Result<u8> {
        let mut data = SmbusData { byte: 0 };
        self.access_ptr(I2C_SMBUS_READ, command, I2C_SMBUS_BYTE_DATA, &mut data)?;
        Ok(unsafe { data.byte })
    }
    fn write_byte(&self, command: u8, value: u8) -> Result<()> {
        self.access(
            I2C_SMBUS_WRITE,
            command,
            I2C_SMBUS_BYTE_DATA,
            SmbusData { byte: value },
        )
    }
    fn write_block(&self, command: u8, values: &[u8]) -> Result<()> {
        ensure!(values.len() <= 32, "SMBus block exceeds 32 bytes");
        let mut data = SmbusData { block: [0; 34] };
        unsafe {
            data.block[0] = values.len() as u8;
            data.block[1..=values.len()].copy_from_slice(values);
        }
        self.access(I2C_SMBUS_WRITE, command, I2C_SMBUS_BLOCK_DATA, data)
    }
    fn access(&self, read_write: u8, command: u8, size: u32, mut data: SmbusData) -> Result<()> {
        self.access_ptr(read_write, command, size, &mut data)
    }
    fn access_ptr(
        &self,
        read_write: u8,
        command: u8,
        size: u32,
        data: &mut SmbusData,
    ) -> Result<()> {
        let mut request = SmbusIoctlData {
            read_write,
            command,
            size,
            data,
        };
        ioctl(
            self.fd,
            I2C_SMBUS,
            (&mut request as *mut SmbusIoctlData) as libc::c_ulong,
            "SMBus transfer",
        )
    }
}

fn ioctl(
    fd: std::os::fd::RawFd,
    request: libc::c_ulong,
    argument: libc::c_ulong,
    operation: &str,
) -> Result<()> {
    let result = unsafe { libc::ioctl(fd, request, argument) };
    if result == -1 {
        bail!("{operation}: {}", std::io::Error::last_os_error())
    }
    Ok(())
}

#[repr(C)]
union SmbusData {
    byte: u8,
    word: u16,
    block: [u8; 34],
}
#[repr(C)]
struct SmbusIoctlData {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut SmbusData,
}

const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_WRITE: u8 = 0;
const I2C_SMBUS_BYTE: u32 = 1;
const I2C_SMBUS_BYTE_DATA: u32 = 2;
const I2C_SMBUS_WORD_DATA: u32 = 3;
const I2C_SMBUS_BLOCK_DATA: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::DEFAULT_HARDWARE;
    #[test]
    fn ene_register_word_is_byte_swapped_for_linux_smbus() {
        assert_eq!(0x8100_u16.swap_bytes(), 0x0081);
    }
    #[test]
    fn known_controller_id_is_exact() {
        assert_eq!(DEFAULT_HARDWARE.ene_dram.controllers[0].version.len(), 15);
    }

    #[test]
    fn mapper_assigns_each_unmapped_dimm_its_configured_address() {
        let addresses = DEFAULT_HARDWARE.ene_dram.addresses;
        assert_eq!(
            mapper_assignments(addresses, |_| false),
            vec![(0, 0x70), (1, 0x71), (2, 0x72), (3, 0x73)]
        );
        assert_eq!(
            mapper_assignments(addresses, |address| address == 0x71),
            vec![(0, 0x70), (2, 0x72), (3, 0x73)]
        );
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

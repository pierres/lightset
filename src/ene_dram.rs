//! ENE DRAM SMBus protocol, faithfully following OpenRGB's Linux
//! `ENESMBusInterface_i2c_smbus` transport. A 16-bit ENE register is selected
//! with an SMBus word write to command 0x00; the Linux SMBus ABI serializes a
//! word least-significant byte first, so the register value is byte-swapped.
//! The selected byte is then read through command 0x81.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub const SUPPORTED_VERSION: &str = "AUDA0-E6K5-0101";
const DEVICE_NAME_REGISTER: u16 = 0x1000;
const CONFIG_TABLE_REGISTER: u16 = 0x1c00;
const DIRECT_COLORS_V2_REGISTER: u16 = 0x8100;
const DIRECT_REGISTER: u16 = 0x8020;
const APPLY_REGISTER: u16 = 0x80a0;
const APPLY_VALUE: u8 = 0x01;
const MAX_BLOCK: usize = 3;
const CONFIG_LED_COUNT: usize = 0x02;
const EXPECTED_LED_COUNT: u8 = 8;
const ENE_ADDRESSES: std::ops::RangeInclusive<u16> = 0x70..=0x73;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EneDramDevice {
    pub bus: PathBuf,
    pub address: u16,
    pub version: String,
    pub led_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterWrite {
    Byte { register: u16, value: u8 },
    Block { register: u16, data: Vec<u8> },
}

#[derive(Debug)]
pub struct DeviceWriteResult {
    pub address: u16,
    pub result: Result<Vec<RegisterWrite>, String>,
}

pub fn probe(bus: &Path, verbose: bool) -> Result<Vec<EneDramDevice>> {
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
    for address in ENE_ADDRESSES {
        match probe_address(&transport, bus, address) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) if verbose => {
                eprintln!("0x{address:02x}: not a supported ENE DRAM controller")
            }
            Ok(None) => {}
            Err(error) if verbose => eprintln!("0x{address:02x}: {error:#}"),
            Err(_) => {}
        }
    }
    Ok(devices)
}

/// Discover conservatively: only AMD PIIX4-style SMBus adapters are considered,
/// and only the four ENE addresses used by this platform are probed.
pub fn discover_bus() -> Result<PathBuf> {
    let class = Path::new("/sys/class/i2c-dev");
    for entry in fs::read_dir(class).context("read /sys/class/i2c-dev")? {
        let entry = entry?;
        let name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if !name.contains("SMBus PIIX4 adapter") {
            continue;
        }
        let bus = Path::new("/dev").join(entry.file_name());
        if !probe(&bus, false)?.is_empty() {
            return Ok(bus);
        }
    }
    bail!("no supported ENE DRAM controllers found on an AMD PIIX4 SMBus adapter")
}

pub fn dump(bus: &Path, address: u16) -> Result<Vec<[u8; 3]>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bus)
        .with_context(|| format!("open {}", bus.display()))?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    let device = probe_address(&transport, bus, address)?
        .with_context(|| format!("0x{address:02x} is not a supported ENE DRAM controller"))?;
    let bytes = read_bytes(
        &transport,
        address,
        DIRECT_COLORS_V2_REGISTER,
        usize::from(device.led_count) * 3,
    )?;
    decode_direct_colors(&bytes)
}

pub fn set_color(
    bus: &Path,
    address: u16,
    rgb: [u8; 3],
    dry_run: bool,
) -> Result<Vec<RegisterWrite>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bus)
        .with_context(|| format!("open {}", bus.display()))?;
    let transport = SmbusTransport {
        fd: file.as_raw_fd(),
    };
    let device = probe_address(&transport, bus, address)?
        .with_context(|| format!("0x{address:02x} is not a supported ENE DRAM controller"))?;
    let writes = direct_color_writes(device.led_count, rgb)?;
    if !dry_run {
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
    }
    Ok(writes)
}

/// Attempt every validated controller; one DIMM failing does not stop the rest.
pub fn set_all_colors(bus: &Path, rgb: [u8; 3], dry_run: bool) -> Result<Vec<DeviceWriteResult>> {
    let devices = probe(bus, false)?;
    ensure!(
        !devices.is_empty(),
        "no supported ENE DRAM controllers found on {}",
        bus.display()
    );
    Ok(devices
        .into_iter()
        .map(|device| DeviceWriteResult {
            address: device.address,
            result: set_color(bus, device.address, rgb, dry_run)
                .map_err(|error| format!("{error:#}")),
        })
        .collect())
}

/// OpenRGB's GUI Direct path calls SetDirect(true), which writes Direct then
/// Apply, followed by SetAllColorsDirect. The latter writes 3-byte blocks and
/// deliberately does not issue a further Apply write.
pub fn direct_color_writes(led_count: u8, rgb: [u8; 3]) -> Result<Vec<RegisterWrite>> {
    ensure!(
        led_count == EXPECTED_LED_COUNT,
        "unsupported ENE LED count {led_count}; expected {EXPECTED_LED_COUNT}"
    );
    let mut direct_bytes = Vec::with_capacity(usize::from(led_count) * 3);
    for _ in 0..led_count {
        direct_bytes.extend([rgb[0], rgb[2], rgb[1]]);
    }
    let mut writes = vec![
        RegisterWrite::Byte {
            register: DIRECT_REGISTER,
            value: 1,
        },
        RegisterWrite::Byte {
            register: APPLY_REGISTER,
            value: APPLY_VALUE,
        },
    ];
    for (index, block) in direct_bytes.chunks(MAX_BLOCK).enumerate() {
        writes.push(RegisterWrite::Block {
            register: DIRECT_COLORS_V2_REGISTER + (index * MAX_BLOCK) as u16,
            data: block.to_vec(),
        });
    }
    Ok(writes)
}

fn probe_address(
    transport: &SmbusTransport,
    bus: &Path,
    address: u16,
) -> Result<Option<EneDramDevice>> {
    let name = read_bytes(transport, address, DEVICE_NAME_REGISTER, 16)?;
    let version = String::from_utf8_lossy(&name)
        .trim_end_matches('\0')
        .to_owned();
    if version != SUPPORTED_VERSION {
        return Ok(None);
    }
    let config = read_bytes(transport, address, CONFIG_TABLE_REGISTER, 64)?;
    let led_count = config[CONFIG_LED_COUNT];
    ensure!(
        led_count == EXPECTED_LED_COUNT,
        "unsupported ENE LED count {led_count}; expected {EXPECTED_LED_COUNT}"
    );
    Ok(Some(EneDramDevice {
        bus: bus.to_owned(),
        address,
        version,
        led_count,
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
pub fn decode_direct_colors(bytes: &[u8]) -> Result<Vec<[u8; 3]>> {
    ensure!(
        bytes.len().is_multiple_of(3),
        "ENE direct-color data must be a multiple of three bytes"
    );
    let (chunks, _) = bytes.as_chunks::<3>();
    Ok(chunks
        .iter()
        .map(|chunk| [chunk[0], chunk[2], chunk[1]])
        .collect())
}

struct SmbusTransport {
    fd: std::os::fd::RawFd,
}

impl SmbusTransport {
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
            !data.is_empty() && data.len() <= MAX_BLOCK,
            "ENE block must contain 1..={MAX_BLOCK} bytes"
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
const I2C_SMBUS_BYTE_DATA: u32 = 2;
const I2C_SMBUS_WORD_DATA: u32 = 3;
const I2C_SMBUS_BLOCK_DATA: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ene_register_word_is_byte_swapped_for_linux_smbus() {
        assert_eq!(0x8100_u16.swap_bytes(), 0x0081);
    }
    #[test]
    fn known_controller_id_is_exact() {
        assert_eq!(SUPPORTED_VERSION.as_bytes().len(), 15);
    }
    #[test]
    fn direct_color_bytes_decode_from_rbg() {
        assert_eq!(
            decode_direct_colors(&[0xff, 0x00, 0x00, 0x00, 0xff, 0x00]).unwrap(),
            vec![[0xff, 0x00, 0x00], [0x00, 0x00, 0xff]]
        );
    }
    #[test]
    fn direct_writes_match_openrgb_gui_sequence() {
        let writes = direct_color_writes(8, [0xff, 0, 0]).unwrap();
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

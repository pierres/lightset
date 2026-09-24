use super::EneTransport;
use anyhow::{Context, Result, ensure};
use std::io;

pub(super) struct SmbusTransport {
    pub(super) fd: std::os::fd::RawFd,
}

impl EneTransport for SmbusTransport {
    fn read_address_byte(&self, address: u16) -> Result<u8> {
        SmbusTransport::read_address_byte(self, address)
    }
    fn read_register(&self, address: u16, register: u16) -> Result<u8> {
        SmbusTransport::read_register(self, address, register)
    }
    fn write_register(&self, address: u16, register: u16, value: u8) -> Result<()> {
        SmbusTransport::write_register(self, address, register, value)
    }
    fn write_register_block(&self, address: u16, register: u16, data: &[u8]) -> Result<()> {
        SmbusTransport::write_register_block(self, address, register, data)
    }
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
        write_block_with_fallback(
            register,
            data,
            |data| self.write_block(0x03, data),
            |register| self.write_word(0x00, register.swap_bytes()),
            |byte| self.write_byte(0x01, byte),
        )
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

pub(super) fn is_block_unsupported(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::raw_os_error)
        .is_some_and(|code| matches!(code, libc::EOPNOTSUPP | libc::ENOTTY))
}

pub(super) fn write_block_with_fallback(
    register: u16,
    data: &[u8],
    mut write_block: impl FnMut(&[u8]) -> Result<()>,
    mut select_register: impl FnMut(u16) -> Result<()>,
    mut write_byte: impl FnMut(u8) -> Result<()>,
) -> Result<()> {
    if let Err(error) = write_block(data) {
        if !is_block_unsupported(&error) {
            return Err(error);
        }
        select_register(register)?;
        for &byte in data {
            write_byte(byte)?;
        }
    }
    Ok(())
}

fn ioctl(
    fd: std::os::fd::RawFd,
    request: libc::c_ulong,
    argument: libc::c_ulong,
    operation: &str,
) -> Result<()> {
    let result = unsafe { libc::ioctl(fd, request, argument) };
    if result == -1 {
        return Err(io::Error::last_os_error()).with_context(|| operation.to_owned());
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

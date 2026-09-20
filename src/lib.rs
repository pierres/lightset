use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const ASUS_VID: u16 = 0x0b05;
pub const ASUS_PID: u16 = 0x18f3;
pub const ATTRIBUTES_REPORT: u8 = 1;
pub const LAMP_ATTRIBUTES_REQUEST_REPORT: u8 = 2;
pub const LAMP_ATTRIBUTES_RESPONSE_REPORT: u8 = 3;
pub const RANGE_UPDATE_REPORT: u8 = 5;
pub const ARRAY_CONTROL_REPORT: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCandidate {
    pub path: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub lamp_array: bool,
}

/// A LampArray interface begins with Usage Page Lighting and Illumination (0x59),
/// Usage LampArray (0x01). The short-item encoding is `05 59 09 01`.
pub fn is_lamp_array_descriptor(descriptor: &[u8]) -> bool {
    descriptor
        .windows(4)
        .take(16)
        .any(|bytes| bytes == [0x05, 0x59, 0x09, 0x01])
}

pub fn hidraw_descriptor(path: &Path) -> Result<Vec<u8>> {
    let name = path.file_name().context("hidraw path has no file name")?;
    let descriptor = Path::new("/sys/class/hidraw")
        .join(name)
        .join("device/report_descriptor");
    fs::read(&descriptor).with_context(|| format!("read {}", descriptor.display()))
}

pub fn discover_hidraw() -> Result<Vec<DeviceCandidate>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/class/hidraw").context("read /sys/class/hidraw")? {
        let entry = entry?;
        let name = entry.file_name();
        let path = Path::new("/dev").join(&name);
        let uevent = fs::read_to_string(entry.path().join("device/uevent")).unwrap_or_default();
        let Some((vid, pid)) = parse_hid_id(&uevent) else {
            continue;
        };
        let descriptor =
            fs::read(entry.path().join("device/report_descriptor")).unwrap_or_default();
        devices.push(DeviceCandidate {
            path,
            vid,
            pid,
            lamp_array: is_lamp_array_descriptor(&descriptor),
        });
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

fn parse_hid_id(uevent: &str) -> Option<(u16, u16)> {
    let value = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))?;
    let mut parts = value.split(':');
    parts.next()?; // bus
    let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
    let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
    Some((vid, pid))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayAttributes {
    pub lamp_count: u16,
    pub bounding_box_width_um: u32,
    pub bounding_box_height_um: u32,
    pub bounding_box_depth_um: u32,
    pub lamp_array_kind: u32,
    pub min_update_interval_us: u32,
}

impl ArrayAttributes {
    pub const REPORT_LEN: usize = 23;

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= Self::REPORT_LEN,
            "report 1 is {} bytes, expected at least {}",
            bytes.len(),
            Self::REPORT_LEN
        );
        ensure!(
            bytes[0] == ATTRIBUTES_REPORT,
            "expected report ID 1, got {}",
            bytes[0]
        );
        Ok(Self {
            lamp_count: u16::from_le_bytes([bytes[1], bytes[2]]),
            bounding_box_width_um: le_u32(bytes, 3),
            bounding_box_height_um: le_u32(bytes, 7),
            bounding_box_depth_um: le_u32(bytes, 11),
            lamp_array_kind: le_u32(bytes, 15),
            min_update_interval_us: le_u32(bytes, 19),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LampAttributes {
    pub lamp_id: u16,
    pub position_x_um: u32,
    pub position_y_um: u32,
    pub position_z_um: u32,
    pub purposes: u32,
    pub update_latency_us: u32,
    pub red_level_count: u8,
    pub green_level_count: u8,
    pub blue_level_count: u8,
    pub intensity_level_count: u8,
    pub is_programmable: bool,
    pub input_binding: u8,
}

impl LampAttributes {
    pub const REPORT_LEN: usize = 29;

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= Self::REPORT_LEN,
            "report 3 is {} bytes, expected at least {}",
            bytes.len(),
            Self::REPORT_LEN
        );
        ensure!(
            bytes[0] == LAMP_ATTRIBUTES_RESPONSE_REPORT,
            "expected report ID 3, got {}",
            bytes[0]
        );
        Ok(Self {
            lamp_id: u16::from_le_bytes([bytes[1], bytes[2]]),
            position_x_um: le_u32(bytes, 3),
            position_y_um: le_u32(bytes, 7),
            position_z_um: le_u32(bytes, 11),
            // The descriptor declares Usage 0x27 (latency) before 0x26
            // (purposes), so the wire order differs from the struct's field order.
            update_latency_us: le_u32(bytes, 15),
            purposes: le_u32(bytes, 19),
            red_level_count: bytes[23],
            green_level_count: bytes[24],
            blue_level_count: bytes[25],
            intensity_level_count: bytes[26],
            is_programmable: bytes[27] != 0,
            input_binding: bytes[28],
        })
    }
}

pub fn lamp_request(lamp_id: u16) -> [u8; 3] {
    [
        LAMP_ATTRIBUTES_REQUEST_REPORT,
        lamp_id as u8,
        (lamp_id >> 8) as u8,
    ]
}
pub fn disable_autonomous_mode() -> [u8; 2] {
    [ARRAY_CONTROL_REPORT, 0]
}
pub fn range_update(lamp_count: u16, rgb: [u8; 3]) -> Result<[u8; 10]> {
    if lamp_count == 0 {
        bail!("device reports zero lamps")
    }
    Ok([
        RANGE_UPDATE_REPORT,
        0x01,
        0,
        0,
        (lamp_count - 1) as u8,
        ((lamp_count - 1) >> 8) as u8,
        rgb[0],
        rgb[1],
        rgb[2],
        0xff,
    ])
}

/// Select a single RGBI tuple that is legal for all lamps in a range.
pub fn range_channels(lamps: &[LampAttributes], rgb: [u8; 3]) -> Result<[u8; 4]> {
    ensure!(!lamps.is_empty(), "device reports zero lamps");
    let mut limits = [u8::MAX; 3];
    let mut programmable = false;
    for lamp in lamps.iter().filter(|lamp| lamp.is_programmable) {
        programmable = true;
        limits[0] = limits[0].min(lamp.red_level_count);
        limits[1] = limits[1].min(lamp.green_level_count);
        limits[2] = limits[2].min(lamp.blue_level_count);
    }
    if !programmable {
        limits = [0; 3];
    }
    let intensity = lamps
        .iter()
        .map(|lamp| lamp.intensity_level_count)
        .min()
        .expect("non-empty lamps");
    ensure!(
        intensity > 0,
        "at least one lamp has no usable intensity channel"
    );
    Ok([
        scale_channel(rgb[0], limits[0]),
        scale_channel(rgb[1], limits[1]),
        scale_channel(rgb[2], limits[2]),
        intensity,
    ])
}

fn scale_channel(input: u8, maximum: u8) -> u8 {
    ((u16::from(input) * u16::from(maximum) + 127) / 255) as u8
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated report length"),
    )
}

pub fn format_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_rgb(value: &str) -> Result<[u8; 3]> {
    let value = value.strip_prefix('#').unwrap_or(value);
    ensure!(
        value.len() == 6 && value.bytes().all(|c| c.is_ascii_hexdigit()),
        "color must be six hexadecimal digits (RRGGBB)"
    );
    Ok([
        u8::from_str_radix(&value[0..2], 16)?,
        u8::from_str_radix(&value[2..4], 16)?,
        u8::from_str_radix(&value[4..6], 16)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_detection_requires_lighting_lamparray_prefix() {
        assert!(is_lamp_array_descriptor(&[0x05, 0x59, 0x09, 0x01, 0xa1]));
        assert!(!is_lamp_array_descriptor(&[0x06, 0x72, 0xff, 0x09, 0x01]));
    }
    #[test]
    fn attributes_decode_descriptor_sized_report() {
        let data = [
            1, 3, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0,
        ];
        let parsed = ArrayAttributes::decode(&data).unwrap();
        assert_eq!(parsed.lamp_count, 3);
        assert_eq!(parsed.min_update_interval_us, 5);
    }
    #[test]
    fn lamp_attributes_decode() {
        let data = [
            3, 2, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 6, 7, 8, 9, 1, 10,
        ];
        let parsed = LampAttributes::decode(&data).unwrap();
        assert_eq!(parsed.lamp_id, 2);
        assert_eq!(parsed.position_z_um, 3);
        assert_eq!(parsed.update_latency_us, 4);
        assert_eq!(parsed.purposes, 5);
        assert!(parsed.is_programmable);
        assert_eq!(parsed.input_binding, 10);
    }
    #[test]
    fn range_update_is_little_endian() {
        assert_eq!(
            range_update(0x1235, [0x20, 0, 0]).unwrap(),
            [5, 1, 0, 0, 0x34, 0x12, 0x20, 0, 0, 0xff]
        );
    }
    #[test]
    fn color_parses() {
        assert_eq!(parse_rgb("#40ff80").unwrap(), [0x40, 0xff, 0x80]);
    }
    #[test]
    fn range_channels_stays_within_every_lamp_limit() {
        let lamp = |id: u16, levels: [u8; 3], intensity: u8| LampAttributes {
            lamp_id: id,
            position_x_um: 0,
            position_y_um: 0,
            position_z_um: 0,
            purposes: 0,
            update_latency_us: 0,
            red_level_count: levels[0],
            green_level_count: levels[1],
            blue_level_count: levels[2],
            intensity_level_count: intensity,
            is_programmable: true,
            input_binding: 0,
        };
        assert_eq!(
            range_channels(
                &[lamp(0, [255, 255, 255], 255), lamp(1, [15, 31, 63], 31)],
                [255, 128, 0]
            )
            .unwrap(),
            [15, 16, 0, 31]
        );
    }
}

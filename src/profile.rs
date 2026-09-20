//! In-code hardware profiles. Add a profile here before enabling a new
//! controller; discovery deliberately never treats an unknown SMBus responder
//! as writable.

#[derive(Debug, Clone, Copy)]
pub struct HardwareProfile {
    pub lamp_array: LampArrayProfile,
    pub ene_dram: EneDramProfile,
}

#[derive(Debug, Clone, Copy)]
pub struct LampArrayProfile {
    /// `None` accepts any descriptor-validated LampArray interface.
    pub vid: Option<u16>,
    pub pid: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub struct EneDramProfile {
    /// Only adapters whose sysfs name contains one of these strings are probed.
    pub adapter_name_patterns: &'static [&'static str],
    /// The only 7-bit I²C addresses that will be read or written.
    pub addresses: &'static [u16],
    pub controllers: &'static [EneControllerProfile],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EneControllerProfile {
    pub version: &'static str,
    pub led_count: u8,
    pub direct_color_register: u16,
    /// Storage indices for logical red, green, and blue respectively.
    pub direct_color_order: [usize; 3],
    pub direct_register: u16,
    pub direct_value: u8,
    pub apply_register: u16,
    pub apply_value: u8,
    pub block_size: usize,
}

const PIIX4_ADAPTERS: &[&str] = &["SMBus PIIX4 adapter"];
const ENE_DRAM_ADDRESSES: &[u16] = &[0x70, 0x71, 0x72, 0x73];
const AUDA0_E6K5_0101: EneControllerProfile = EneControllerProfile {
    version: "AUDA0-E6K5-0101",
    led_count: 8,
    direct_color_register: 0x8100,
    direct_color_order: [0, 2, 1], // R, B, G on the wire
    direct_register: 0x8020,
    direct_value: 0x01,
    apply_register: 0x80a0,
    apply_value: 0x01,
    block_size: 3,
};

/// The profile proven on the development machine. Extend the controller or
/// adapter lists to support new hardware; do not loosen validation globally.
pub const DEFAULT_HARDWARE: HardwareProfile = HardwareProfile {
    lamp_array: LampArrayProfile {
        vid: Some(0x0b05),
        pid: Some(0x18f3),
    },
    ene_dram: EneDramProfile {
        adapter_name_patterns: PIIX4_ADAPTERS,
        addresses: ENE_DRAM_ADDRESSES,
        controllers: &[AUDA0_E6K5_0101],
    },
};

use anyhow::{Context, Result, bail};
use asus_lamp::{
    ArrayAttributes, DeviceCandidate, LampAttributes, disable_autonomous_mode, discover_hidraw,
    format_bytes, hidraw_descriptor, is_lamp_array_descriptor, lamp_request, parse_rgb,
    range_channels, range_update,
};
use clap::{Parser, Subcommand};
use hidapi::HidApi;
use std::{ffi::CString, path::PathBuf, thread, time::Duration};

#[derive(Parser)]
#[command(about = "Set a solid color on an ASUS standard HID LampArray")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    device: Option<PathBuf>,
    #[arg(long, default_value = "0b05", value_parser = parse_hex_u16)]
    vid: u16,
    #[arg(long, default_value = "18f3", value_parser = parse_hex_u16)]
    pid: u16,
    #[arg(short, long)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    List,
    Info {
        #[arg(long)]
        lamps: bool,
    },
    Set {
        color: String,
        #[arg(long)]
        dry_run: bool,
    },
    Off {
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::List => list(),
        Command::Info { lamps } => info(&cli, *lamps),
        Command::Set { color, dry_run } => set(&cli, parse_rgb(color)?, *dry_run),
        Command::Off { dry_run } => set(&cli, [0, 0, 0], *dry_run),
    }
}
fn list() -> Result<()> {
    for d in discover_hidraw()? {
        println!(
            "{} {:04x}:{:04x} LampArray: {}",
            d.path.display(),
            d.vid,
            d.pid,
            if d.lamp_array { "yes" } else { "no" }
        );
    }
    Ok(())
}
fn selected(cli: &Cli) -> Result<DeviceCandidate> {
    if let Some(path) = &cli.device {
        let descriptor = hidraw_descriptor(path)?;
        if !is_lamp_array_descriptor(&descriptor) {
            bail!(
                "{} is not a Lighting and Illumination LampArray interface",
                path.display()
            )
        };
        return Ok(DeviceCandidate {
            path: path.clone(),
            vid: cli.vid,
            pid: cli.pid,
            lamp_array: true,
        });
    }
    discover_hidraw()?
        .into_iter()
        .find(|d| d.vid == cli.vid && d.pid == cli.pid && d.lamp_array)
        .with_context(|| {
            format!(
                "no LampArray hidraw interface found for {:04x}:{:04x}",
                cli.vid, cli.pid
            )
        })
}
fn open(cli: &Cli) -> Result<(hidapi::HidDevice, DeviceCandidate)> {
    let candidate = selected(cli)?;
    let api = HidApi::new().context("initialize hidapi")?;
    let path = CString::new(candidate.path.as_os_str().as_encoded_bytes())
        .context("device path contains a NUL byte")?;
    let device = api
        .open_path(&path)
        .with_context(|| format!("open {}", candidate.path.display()))?;
    Ok((device, candidate))
}
fn attributes(device: &hidapi::HidDevice, verbose: bool) -> Result<ArrayAttributes> {
    let mut bytes = [1u8; ArrayAttributes::REPORT_LEN];
    let len = device
        .get_feature_report(&mut bytes)
        .context("GET feature report 1")?;
    if verbose {
        eprintln!(
            "GET report 1 ({} bytes): {}",
            len,
            format_bytes(&bytes[..len])
        );
    }
    ArrayAttributes::decode(&bytes[..len])
}
fn info(cli: &Cli, lamps: bool) -> Result<()> {
    let (device, candidate) = open(cli)?;
    let attributes = attributes(&device, cli.verbose)?;
    println!(
        "Device: {}\nVID:PID: {:04x}:{:04x}\nLampArray: yes\nLamp count: {}\nBounding box: {} × {} × {} um\nLamp array kind: {}\nMin update interval: {} us",
        candidate.path.display(),
        candidate.vid,
        candidate.pid,
        attributes.lamp_count,
        attributes.bounding_box_width_um,
        attributes.bounding_box_height_um,
        attributes.bounding_box_depth_um,
        attributes.lamp_array_kind,
        attributes.min_update_interval_us
    );
    if lamps {
        for lamp in read_lamps(&device, attributes.lamp_count, cli.verbose)? {
            println!(
                "Lamp {}: pos=({}, {}, {}) um purposes=0x{:08x} latency={} us RGBI levels={}/{}/{}/{} programmable={} input_binding={}",
                lamp.lamp_id,
                lamp.position_x_um,
                lamp.position_y_um,
                lamp.position_z_um,
                lamp.purposes,
                lamp.update_latency_us,
                lamp.red_level_count,
                lamp.green_level_count,
                lamp.blue_level_count,
                lamp.intensity_level_count,
                lamp.is_programmable,
                lamp.input_binding
            );
        }
    }
    Ok(())
}
fn set(cli: &Cli, rgb: [u8; 3], dry_run: bool) -> Result<()> {
    let (device, candidate) = open(cli)?;
    let attrs = attributes(&device, cli.verbose)?;
    let lamps = read_lamps(&device, attrs.lamp_count, cli.verbose)?;
    let channels = range_channels(&lamps, rgb)?;
    let control = disable_autonomous_mode();
    let mut update = range_update(attrs.lamp_count, [channels[0], channels[1], channels[2]])?;
    update[9] = channels[3];
    println!(
        "Device: {}\nReport 6: {}\nReport 5: {}",
        candidate.path.display(),
        format_bytes(&control),
        format_bytes(&update)
    );
    if dry_run {
        return Ok(());
    }
    device
        .send_feature_report(&control)
        .context("SET feature report 6 (disable autonomous mode)")?;
    thread::sleep(Duration::from_micros(u64::from(
        attrs.min_update_interval_us,
    )));
    device
        .send_feature_report(&update)
        .context("SET feature report 5 (range update)")?;
    Ok(())
}
fn read_lamps(
    device: &hidapi::HidDevice,
    lamp_count: u16,
    verbose: bool,
) -> Result<Vec<LampAttributes>> {
    let mut lamps = Vec::with_capacity(usize::from(lamp_count));
    for id in 0..lamp_count {
        let request = lamp_request(id);
        if verbose {
            eprintln!("SET report 2: {}", format_bytes(&request));
        }
        device
            .send_feature_report(&request)
            .with_context(|| format!("SET feature report 2 for lamp {id}"))?;
        let mut response = [3u8; LampAttributes::REPORT_LEN];
        let len = device
            .get_feature_report(&mut response)
            .with_context(|| format!("GET feature report 3 for lamp {id}"))?;
        if verbose {
            eprintln!(
                "GET report 3 ({} bytes): {}",
                len,
                format_bytes(&response[..len])
            );
        }
        let lamp = LampAttributes::decode(&response[..len])?;
        if lamp.lamp_id != id {
            bail!("requested lamp {id}, device returned lamp {}", lamp.lamp_id)
        }
        lamps.push(lamp);
    }
    Ok(lamps)
}
fn parse_hex_u16(value: &str) -> Result<u16, String> {
    u16::from_str_radix(value.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

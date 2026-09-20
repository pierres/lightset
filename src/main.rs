use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hidapi::HidApi;
use lightset::profile::DEFAULT_HARDWARE;
use lightset::{
    ArrayAttributes, DeviceCandidate, LampAttributes, disable_autonomous_mode, discover_hidraw,
    format_bytes, hidraw_descriptor, is_lamp_array_descriptor, lamp_request, parse_rgb,
    range_channels, range_update,
};
use std::{ffi::CString, path::PathBuf, thread, time::Duration};

#[derive(Parser)]
#[command(about = "Set a solid color on ASUS LampArray and ENE DRAM lighting")]
struct Cli {
    #[arg(long, value_name = "PATH", hide = true)]
    device: Option<PathBuf>,
    #[arg(long, value_parser = parse_hex_u16, hide = true)]
    vid: Option<u16>,
    #[arg(long, value_parser = parse_hex_u16, hide = true)]
    pid: Option<u16>,
    #[arg(short, long, hide = true)]
    verbose: bool,
    #[arg(long, value_name = "PATH", global = true, hide = true)]
    i2c_bus: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    List,
    #[command(hide = true)]
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
    #[command(hide = true)]
    Lamp {
        #[command(subcommand)]
        command: LampCommand,
    },
    #[command(hide = true)]
    Dram {
        #[command(subcommand)]
        command: DramCommand,
    },
}
#[derive(Subcommand)]
enum LampCommand {
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
#[derive(Subcommand)]
enum DramCommand {
    Probe {
        #[arg(long, value_name = "PATH")]
        bus: PathBuf,
    },
    Dump {
        #[arg(long, value_name = "PATH")]
        bus: PathBuf,
        #[arg(long, value_parser = parse_hex_u16)]
        address: u16,
    },
    Set {
        color: String,
        #[arg(long, value_name = "PATH")]
        bus: PathBuf,
        #[arg(long, value_parser = parse_hex_u16)]
        address: Option<u16>,
        #[arg(long)]
        dry_run: bool,
    },
    Off {
        #[arg(long, value_name = "PATH")]
        bus: PathBuf,
        #[arg(long, value_parser = parse_hex_u16)]
        address: Option<u16>,
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::List => list(),
        Command::Info { lamps } => info(&cli, *lamps),
        Command::Set { color, dry_run } => set_all(&cli, parse_rgb(color)?, *dry_run),
        Command::Off { dry_run } => set_all(&cli, [0, 0, 0], *dry_run),
        Command::Lamp { command } => match command {
            LampCommand::List => list(),
            LampCommand::Info { lamps } => info(&cli, *lamps),
            LampCommand::Set { color, dry_run } => lamp_set(&cli, parse_rgb(color)?, *dry_run),
            LampCommand::Off { dry_run } => lamp_set(&cli, [0, 0, 0], *dry_run),
        },
        Command::Dram { command } => match command {
            DramCommand::Probe { bus } => dram_probe(bus, cli.verbose),
            DramCommand::Dump { bus, address } => dram_dump(bus, *address),
            DramCommand::Set {
                color,
                bus,
                address,
                dry_run,
            } => match address {
                Some(address) => dram_set(bus, *address, parse_rgb(color)?, *dry_run),
                None => dram_set_all(bus, parse_rgb(color)?, *dry_run),
            },
            DramCommand::Off {
                bus,
                address,
                dry_run,
            } => match address {
                Some(address) => dram_set(bus, *address, [0, 0, 0], *dry_run),
                None => dram_set_all(bus, [0, 0, 0], *dry_run),
            },
        },
    }
}
fn dram_probe(bus: &std::path::Path, verbose: bool) -> Result<()> {
    let devices = lightset::ene_dram::probe(bus, &DEFAULT_HARDWARE.ene_dram, verbose)?;
    println!("Bus: {}", bus.display());
    for device in devices {
        println!(
            "0x{:02x}  {}  LEDs: {}  supported",
            device.address, device.version, device.led_count
        );
    }
    Ok(())
}
fn dram_dump(bus: &std::path::Path, address: u16) -> Result<()> {
    let colors = lightset::ene_dram::dump(bus, &DEFAULT_HARDWARE.ene_dram, address)?;
    println!("Bus: {}\nAddress: 0x{address:02x}", bus.display());
    for (index, [red, green, blue]) in colors.into_iter().enumerate() {
        println!("LED {index}: {red:02x}{green:02x}{blue:02x}");
    }
    Ok(())
}
fn dram_set(bus: &std::path::Path, address: u16, rgb: [u8; 3], dry_run: bool) -> Result<()> {
    let writes =
        lightset::ene_dram::set_color(bus, &DEFAULT_HARDWARE.ene_dram, address, rgb, dry_run)?;
    println!("Bus: {}\nAddress: 0x{address:02x}", bus.display());
    for write in writes {
        match write {
            lightset::ene_dram::RegisterWrite::Byte { register, value } => {
                println!("Write 0x{register:04x}: {value:02x}")
            }
            lightset::ene_dram::RegisterWrite::Block { register, data } => {
                println!("Write 0x{register:04x}: {}", format_bytes(&data))
            }
        }
    }
    if dry_run {
        println!("Dry run: no DRAM color writes sent");
    }
    Ok(())
}
fn dram_set_all(bus: &std::path::Path, rgb: [u8; 3], dry_run: bool) -> Result<()> {
    let results =
        lightset::ene_dram::set_all_colors(bus, &DEFAULT_HARDWARE.ene_dram, rgb, dry_run)?;
    let mut failures = 0;
    for result in results {
        match result.result {
            Ok(writes) => {
                println!("ENE DRAM 0x{:02x}: OK", result.address);
                if dry_run {
                    for write in writes {
                        match write {
                            lightset::ene_dram::RegisterWrite::Byte { register, value } => {
                                println!("  Write 0x{register:04x}: {value:02x}")
                            }
                            lightset::ene_dram::RegisterWrite::Block { register, data } => {
                                println!("  Write 0x{register:04x}: {}", format_bytes(&data))
                            }
                        }
                    }
                }
            }
            Err(error) => {
                failures += 1;
                println!("ENE DRAM 0x{:02x}: FAILED: {error}", result.address);
            }
        }
    }
    if failures > 0 {
        bail!("{failures} ENE DRAM controller(s) failed")
    }
    if dry_run {
        println!("Dry run: no DRAM color writes sent");
    }
    Ok(())
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
    let vid = cli.vid.or(DEFAULT_HARDWARE.lamp_array.vid);
    let pid = cli.pid.or(DEFAULT_HARDWARE.lamp_array.pid);
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
            vid: vid.unwrap_or_default(),
            pid: pid.unwrap_or_default(),
            lamp_array: true,
        });
    }
    discover_hidraw()?
        .into_iter()
        .find(|d| {
            d.lamp_array
                && vid.is_none_or(|wanted| d.vid == wanted)
                && pid.is_none_or(|wanted| d.pid == wanted)
        })
        .with_context(|| {
            format!(
                "no matching LampArray hidraw interface found{}{}",
                vid.map(|value| format!(" for VID {value:04x}"))
                    .unwrap_or_default(),
                pid.map(|value| format!(" PID {value:04x}"))
                    .unwrap_or_default(),
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
fn lamp_set(cli: &Cli, rgb: [u8; 3], dry_run: bool) -> Result<()> {
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
fn set_all(cli: &Cli, rgb: [u8; 3], dry_run: bool) -> Result<()> {
    let mut failures = Vec::new();
    match lamp_set(cli, rgb, dry_run) {
        Ok(()) => println!("LampArray: OK"),
        Err(error) => {
            println!("LampArray: FAILED: {error:#}");
            failures.push("LampArray");
        }
    }
    let bus = match &cli.i2c_bus {
        Some(bus) => Ok(bus.clone()),
        None => lightset::ene_dram::discover_bus(&DEFAULT_HARDWARE.ene_dram),
    };
    if let Err(error) = bus.and_then(|bus| dram_set_all(&bus, rgb, dry_run)) {
        println!("ENE DRAM: FAILED: {error:#}");
        failures.push("ENE DRAM");
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "{} backend(s) failed: {}",
            failures.len(),
            failures.join(", ")
        )
    }
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

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hidapi::HidApi;
use lightset::profile::DEFAULT_HARDWARE;
use lightset::{
    ArrayAttributes, DeviceCandidate, LampAttributes, disable_autonomous_mode, discover_hidraw,
    lamp_request, parse_rgb, range_channels, range_update,
};
use std::{ffi::CString, thread, time::Duration};

#[derive(Parser)]
#[command(about = "Set a solid color on ASUS LampArray and ENE DRAM lighting")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Set {
        color: String,
    },
    Off,
    #[cfg(debug_assertions)]
    /// Report every configured ENE DRAM address without changing lighting.
    Probe,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Set { color } => set_all(parse_rgb(color)?),
        Command::Off => set_all([0, 0, 0]),
        #[cfg(debug_assertions)]
        Command::Probe => probe(),
    }
}
#[cfg(debug_assertions)]
fn probe() -> Result<()> {
    for bus in lightset::ene_dram::diagnose(&DEFAULT_HARDWARE.ene_dram)? {
        println!("{} ({})", bus.bus.display(), bus.adapter_name);
        if let Some(error) = bus.open_error {
            println!("  open: FAILED: {error}");
            continue;
        }
        for address in bus.addresses {
            match (address.version, address.led_count, address.error) {
                (_, _, Some(error)) => println!("  0x{:02x}: FAILED: {error}", address.address),
                (Some(version), Some(led_count), None) => println!(
                    "  0x{:02x}: version={version:?}, leds={led_count}, supported={}",
                    address.address, address.supported
                ),
                _ => unreachable!("a diagnostic has a result or an error"),
            }
        }
    }
    Ok(())
}
fn dram_set_all(prepared: &lightset::ene_dram::PreparedDram, rgb: [u8; 3]) -> Result<()> {
    let results = lightset::ene_dram::set_prepared_colors(prepared, rgb)?;
    let mut failures = 0;
    for result in results {
        match result.result {
            Ok(_) => {
                println!("ENE DRAM 0x{:02x}: OK", result.address);
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
    Ok(())
}
fn selected() -> Result<DeviceCandidate> {
    let vid = DEFAULT_HARDWARE.lamp_array.vid;
    let pid = DEFAULT_HARDWARE.lamp_array.pid;
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
fn open() -> Result<(hidapi::HidDevice, DeviceCandidate)> {
    let candidate = selected()?;
    let api = HidApi::new().context("initialize hidapi")?;
    let path = CString::new(candidate.path.as_os_str().as_encoded_bytes())
        .context("device path contains a NUL byte")?;
    let device = api
        .open_path(&path)
        .with_context(|| format!("open {}", candidate.path.display()))?;
    Ok((device, candidate))
}
fn attributes(device: &hidapi::HidDevice) -> Result<ArrayAttributes> {
    let mut bytes = [1u8; ArrayAttributes::REPORT_LEN];
    let len = device
        .get_feature_report(&mut bytes)
        .context("GET feature report 1")?;
    ArrayAttributes::decode(&bytes[..len])
}
fn lamp_set(rgb: [u8; 3]) -> Result<()> {
    let (device, _) = open()?;
    let attrs = attributes(&device)?;
    let lamps = read_lamps(&device, attrs.lamp_count)?;
    let channels = range_channels(&lamps, rgb)?;
    let control = disable_autonomous_mode();
    let mut update = range_update(attrs.lamp_count, [channels[0], channels[1], channels[2]])?;
    update[9] = channels[3];
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
fn set_all(rgb: [u8; 3]) -> Result<()> {
    let mut failures = Vec::new();
    let dram = lightset::ene_dram::prepare(&DEFAULT_HARDWARE.ene_dram);
    match lamp_set(rgb) {
        Ok(()) => println!("LampArray: OK"),
        Err(error) => {
            println!("LampArray: FAILED: {error:#}");
            failures.push("LampArray");
        }
    }
    if let Err(error) = dram.and_then(|prepared| dram_set_all(&prepared, rgb)) {
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
fn read_lamps(device: &hidapi::HidDevice, lamp_count: u16) -> Result<Vec<LampAttributes>> {
    let mut lamps = Vec::with_capacity(usize::from(lamp_count));
    for id in 0..lamp_count {
        let request = lamp_request(id);
        device
            .send_feature_report(&request)
            .with_context(|| format!("SET feature report 2 for lamp {id}"))?;
        let mut response = [3u8; LampAttributes::REPORT_LEN];
        let len = device
            .get_feature_report(&mut response)
            .with_context(|| format!("GET feature report 3 for lamp {id}"))?;
        let lamp = LampAttributes::decode(&response[..len])?;
        if lamp.lamp_id != id {
            bail!("requested lamp {id}, device returned lamp {}", lamp.lamp_id)
        }
        lamps.push(lamp);
    }
    Ok(lamps)
}

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use lightset::profile::DEFAULT_HARDWARE;
use lightset::{lamp_array, parse_rgb};

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
            if !address.present && address.error.is_none() {
                println!("  0x{:02x}: absent", address.address);
                continue;
            }
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
    let supported = results.len();
    let mut failures = 0;
    let mut updated = 0;
    for (address, error) in prepared.probe_failures() {
        failures += 1;
        println!("ENE DRAM 0x{address:02x}: PROBE FAILED: {error}");
    }
    for result in results {
        match result.result {
            Ok(_) => {
                updated += 1;
                println!("ENE DRAM 0x{:02x}: OK", result.address);
            }
            Err(error) => {
                failures += 1;
                println!("ENE DRAM 0x{:02x}: FAILED: {error}", result.address);
            }
        }
    }
    println!("ENE DRAM: {updated}/{supported} supported controller(s) updated");
    if failures > 0 {
        bail!("{failures} ENE DRAM controller(s) failed")
    }
    Ok(())
}
fn set_all(rgb: [u8; 3]) -> Result<()> {
    let mut failures = Vec::new();
    let dram = lightset::ene_dram::prepare(&DEFAULT_HARDWARE.ene_dram);
    match lamp_array::set_color(&DEFAULT_HARDWARE.lamp_array, rgb) {
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

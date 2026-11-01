// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! Stream bounded, original-Liberty pin data for file-based ECO planners.

use rusteda_core::liberty::Direction;
use rusteda_formats::liberty::read_file_diags;

fn number(value: Option<f64>) -> String {
    value.map_or_else(|| "none".into(), |v| v.to_string())
}

#[allow(clippy::print_stdout)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.len() > 8 {
        return Err("expected one to eight Liberty paths".into());
    }
    for arg in args {
        let path = std::path::Path::new(&arg);
        if path.metadata()?.len() > 64 * 1024 * 1024 {
            return Err("input exceeds 64 MiB".into());
        }
        let (lib, diags) = read_file_diags(path)?;
        for diag in diags {
            eprintln!("{diag:?}");
        }
        let pins: usize = lib.cells.iter().map(|c| c.pins.len()).sum();
        if lib.cells.len() > 50_000 || pins > 250_000 {
            return Err("cell/pin budget exceeded".into());
        }
        for cell in &lib.cells {
            for pin in &cell.pins {
                if [&cell.name, &pin.name]
                    .iter()
                    .any(|s| s.contains(['\t', '\n', '\r']))
                {
                    return Err("identifier cannot be represented in a TSV field".into());
                }
                let default = match pin.direction {
                    Direction::Input => lib.default_input_pin_cap,
                    Direction::Output => lib.default_output_pin_cap,
                    Direction::Inout => lib.default_inout_pin_cap,
                    Direction::Internal => None,
                };
                let cap = pin.capacitance_given.then_some(pin.capacitance).or(default);
                let upper = [
                    cap,
                    pin.rise_capacitance,
                    pin.fall_capacitance,
                    pin.rise_capacitance_range.map(|r| r.1),
                    pin.fall_capacitance_range.map(|r| r.1),
                ]
                .into_iter()
                .flatten()
                .reduce(f64::max)
                .map(|c| c * lib.cap_unit_pf);
                println!(
                    "{}\t{}\t{:?}\t{}\t{}\t{}\t{}\t{}",
                    cell.name,
                    pin.name,
                    pin.direction,
                    number(upper),
                    number(lib.pin_max_capacitance(pin).map(|c| c * lib.cap_unit_pf)),
                    lib.pin_fanout_load(pin),
                    pin.is_clock,
                    number(lib.pin_max_fanout(pin))
                );
            }
        }
    }
    Ok(())
}

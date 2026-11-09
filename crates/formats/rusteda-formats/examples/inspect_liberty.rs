// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! Bounded real-library import probe; diagnostics are never discarded.
use rusteda_formats::liberty::read_file_diags;
#[allow(clippy::print_stdout)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        if path.metadata()?.len() > 64 * 1024 * 1024 {
            return Err("input exceeds 64 MiB".into());
        }
        let (lib, diags) = read_file_diags(path)?;
        println!(
            "{}: {} cells, {} pins, {} arcs, {} diagnostics",
            arg,
            lib.cells.len(),
            lib.cells.iter().map(|c| c.pins.len()).sum::<usize>(),
            lib.cells
                .iter()
                .flat_map(|c| &c.pins)
                .map(|p| p.arcs.len())
                .sum::<usize>(),
            diags.len()
        );
        for cell in lib.cells.iter().filter(|c| c.is_macro_cell) {
            println!(
                "MACRO {} pins={} area={} problems={:?}",
                cell.name,
                cell.pins.len(),
                cell.area,
                cell.problems
            );
        }
        for diag in diags {
            println!("{diag:?}");
        }
    }
    Ok(())
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The helpers of the Ibex recipe's native frontend as native commands: one ECO round
//! (`rusteda eco-round`, RCLI-REQ-0027) and the planning helpers (`rusteda frontend-profiles`,
//! `frontend-physical`, `frontend-pins`, `frontend-validate`, RCLI-REQ-0028). Each command writes the files and the standard output
//! of its reference helper (`integration/ibex_sg13g2_ez130/native_frontend_inputs.py` and
//! `native_ecos.py` of the RustEDA recipe) byte for byte from the same inputs; `py` and
//! `context` hold the value, digest and workspace rules they share with it.

pub(crate) mod context;
pub mod eco;
mod eco_estimated;
mod eco_graph;
mod eco_inputs;
pub mod helpers;
pub(crate) mod liberty;
pub(crate) mod py;

use std::io::Write;

/// Writes a command's standard output.
pub(crate) fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// With `RUSTEDA_FLOW_TRACE` set, writes the time since the first call and the label to standard
/// error: where a command spends its time.
pub(crate) fn trace(label: &str) {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Option<Instant>> = OnceLock::new();
    let start =
        START.get_or_init(|| std::env::var_os("RUSTEDA_FLOW_TRACE").map(|_| Instant::now()));
    if let Some(start) = start {
        eprintln!(
            "trace {:9.3} ms  {label}",
            start.elapsed().as_secs_f64() * 1e3
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The planning helpers of the Ibex recipe's native frontend as native commands
//! (RCLI-REQ-0028): `frontend-profiles` (cell policy, delay and sizing profiles, cell buses,
//! IO projection and CTS constraints from the PDK's Liberty and LEF and the SDC),
//! `frontend-physical` (the physical and routing configuration bound to the clock root),
//! `frontend-pins` (the load-aware pin plan of the pin-planning placement) and
//! `frontend-validate` (the frontend's result record and the final check of the run's flow
//! digest cache). Each writes its reference helper's files (`native_frontend_inputs.py
//! profiles|physical|pins|validate`) byte for byte into a fresh workspace and nothing on
//! standard output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::context::{
    Context, Failure, Res, at, fresh, key_error, need, read_json, resolve, show, text, write_json,
};
use super::eco_inputs::{Scan, driving_cell, set_load, stripped_lines};
use super::liberty::{self, Group, Library, Table, unquote};
use crate::estimate::{Json, json_string, obj, py_round, py_sum, read_text, repr, space, tokens};

/// What the four helper commands share (`--workspace` is the global `-w/--workspace`).
#[derive(clap::Args, Debug)]
pub struct HelperArgs {
    /// The run's pinned context (`rusteda.native-frontend-context.v1`).
    #[arg(long = "context", value_name = "FILE")]
    pub context: PathBuf,
    /// Base revision of the workspace; only `empty` (a fresh directory) is accepted.
    #[arg(long = "base-rev", value_name = "REV")]
    pub base_rev: String,
}

#[derive(clap::Args, Debug)]
pub struct PhysicalArgs {
    #[command(flatten)]
    pub common: HelperArgs,
    /// The roots of the reset round's bundle (`bundle-reset/roots.json`).
    #[arg(long = "roots", value_name = "FILE")]
    pub roots: PathBuf,
}

#[derive(clap::Args, Debug)]
pub struct PinsArgs {
    #[command(flatten)]
    pub common: HelperArgs,
    /// The placed DEF of the pin-planning placement.
    #[arg(long = "def", value_name = "FILE")]
    pub def: PathBuf,
}

/// One helper command.
#[derive(Debug)]
pub enum Helper<'a> {
    Profiles(&'a HelperArgs),
    Physical(&'a PhysicalArgs),
    Pins(&'a PinsArgs),
    Validate(&'a HelperArgs),
}

/// Runs a helper command for the workspace `ws`.
pub fn run(ws: &Path, helper: Helper<'_>) -> ExitCode {
    // The reference's alarm.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(180));
        eprint!("{}", failed(&Failure::new("TIMEOUT", "helper wall budget")));
        std::process::exit(2);
    });
    let common = match &helper {
        Helper::Profiles(a) | Helper::Validate(a) => *a,
        Helper::Physical(a) => &a.common,
        Helper::Pins(a) => &a.common,
    };
    let work = || -> Res<()> {
        let c = Context::load(&common.context)?;
        {
            let _lock = fresh(ws, &common.base_rev)?;
            match &helper {
                Helper::Profiles(_) => profiles(&c, ws)?,
                Helper::Physical(a) => physical(&c, &a.roots, ws)?,
                Helper::Pins(a) => pins(&c, &a.def, ws)?,
                Helper::Validate(_) => validate(&c, ws)?,
            }
            fs::write(ws.join("HEAD"), "rev1\n")?;
        }
        c.digests.save_flow();
        Ok(())
    };
    match work() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprint!("{}", failed(&e));
            ExitCode::from(2)
        }
    }
}

fn failed(e: &Failure) -> String {
    let mut line = String::from("{\"status\": \"failed\", \"code\": ");
    json_string(&mut line, &e.code);
    line.push_str(", \"error\": ");
    json_string(&mut line, &e.message);
    line.push_str("}\n");
    line
}

fn list<'j>(v: &'j Json, k: &str) -> Res<&'j [Json]> {
    at(v, k)?
        .as_list()
        .ok_or_else(|| Failure::new("INPUT", format!("list required: {k}")))
}

fn strs(v: &Json, k: &str) -> Res<Vec<String>> {
    list(v, k)?
        .iter()
        .map(|s| {
            s.as_str()
                .map(str::to_string)
                .ok_or_else(|| Failure::new("INPUT", format!("strings required: {k}")))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// LEF text of the profile builders (one statement per line, as LEF writers emit it).

fn first_token(line: &str) -> (usize, &str) {
    let b = line.as_bytes();
    let start = b.iter().position(|c| !space(*c)).unwrap_or(b.len());
    let end = b[start..]
        .iter()
        .position(|c| space(*c))
        .map_or(b.len(), |k| start + k);
    (start, &line[start..end])
}

/// `WORD\s+(\S+)\s*$` from the first token of the line; with `flush`, the word must start the
/// line.
fn named<'l>(line: &'l str, word: &str, flush: bool) -> Option<&'l str> {
    let t = tokens(line);
    let (start, first) = first_token(line);
    (t.len() == 2 && first == word && (!flush || start == 0)).then(|| t[1])
}

/// The blocks `WORD name ... END name` of `lines[range]`: (name, first body line, END line).
fn blocks<'l>(lines: &[&'l str], word: &str, flush: bool) -> Vec<(&'l str, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(name) = named(lines[i], word, flush) else {
            i += 1;
            continue;
        };
        let end = (i + 1..lines.len()).find(|&k| named(lines[k], "END", flush) == Some(name));
        match end {
            Some(end) => {
                out.push((name, i + 1, end));
                i = end + 1;
            }
            None => i += 1,
        }
    }
    out
}

fn blanks(c: &mut Scan<'_>) -> bool {
    c.run(|ch| ch.is_ascii() && space(ch as u8)).is_some()
}

/// `^\s*SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)\s*;`: the first matching line of the body.
fn lef_size(body: &[&str]) -> Res<Option<(f64, f64, String)>> {
    for line in body {
        let (start, first) = first_token(line);
        if first != "SIZE" {
            continue;
        }
        let mut c = Scan::new(&line[start + 4..]);
        if !blanks(&mut c) {
            continue;
        }
        let Some(w) = c.run(|ch| ch.is_ascii_digit() || ch == '.') else {
            continue;
        };
        if !blanks(&mut c) || !c.lit("BY") || !blanks(&mut c) {
            continue;
        }
        let Some(h) = c.run(|ch| ch.is_ascii_digit() || ch == '.') else {
            continue;
        };
        blanks(&mut c);
        if !c.lit(";") {
            continue;
        }
        return Ok(Some((
            liberty::float(w)?,
            liberty::float(h)?,
            w.to_string(),
        )));
    }
    Ok(None)
}

/// `^\s*CLASS\s+([^;]+);`: the class of the body with single blanks.
fn lef_class(body: &[&str]) -> Option<String> {
    for (k, line) in body.iter().enumerate() {
        let (start, first) = first_token(line);
        if first != "CLASS" {
            continue;
        }
        // The text up to the next `;`, which may be on a later line.
        let mut text = line[start + 5..].to_string();
        let mut next = k + 1;
        while !text.contains(';') && next < body.len() {
            text.push('\n');
            text.push_str(body[next]);
            next += 1;
        }
        let Some(end) = text.find(';') else { continue };
        let value = &text[..end];
        let blank_start = value.starts_with(|ch: char| ch.is_ascii() && space(ch as u8));
        if !blank_start || value.len() < 2 {
            continue;
        }
        return Some(tokens(value).join(" "));
    }
    None
}

/// The first `WORD\s+(\w+)` of the text.
fn word_after<'t>(text: &'t str, word: &str) -> Option<&'t str> {
    let mut from = 0;
    while let Some(k) = text[from..].find(word) {
        let mut c = Scan::new(&text[from + k + word.len()..]);
        if blanks(&mut c) {
            if let Some(w) = c.run(|ch| ch.is_alphanumeric() || ch == '_') {
                return Some(w);
            }
        }
        from += k + 1;
    }
    None
}

/// Every `WORD\s+([0-9.]+)` of the text, as `sum(map(float, ...))`; None without a match.
fn area_sum(text: &str, word: &str) -> Res<Option<f64>> {
    let mut values = Vec::new();
    let mut from = 0;
    while let Some(k) = text[from..].find(word) {
        let begin = from + k + word.len();
        let mut c = Scan::new(&text[begin..]);
        if blanks(&mut c) {
            if let Some(n) = c.run(|ch| ch.is_ascii_digit() || ch == '.') {
                values.push(liberty::float(n)?);
                from = text.len() - c.rest().len();
                continue;
            }
        }
        from += k + 1;
    }
    Ok(py_sum(values))
}

#[derive(Debug)]
struct LefPin {
    direction: Option<String>,
    usage: String,
    gate_area_um2: Option<f64>,
    diff_area_um2: Option<f64>,
}

#[derive(Debug)]
struct LefMacro {
    cls: Option<String>,
    size_um: Option<(f64, f64)>,
    /// The width as the LEF writes it.
    width_text: Option<String>,
    pins: Vec<(String, LefPin)>,
}

/// `lef_macros(path)` of the delay profile builder: CLASS, SIZE and per-pin direction, use and
/// antenna areas of every MACRO.
fn lef_macros(text: &str) -> Res<Vec<(String, LefMacro)>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<(String, LefMacro)> = Vec::new();
    for (name, first, end) in blocks(&lines, "MACRO", true) {
        let body = &lines[first..end];
        let size = lef_size(body)?;
        let mut pins: Vec<(String, LefPin)> = Vec::new();
        for (pin, a, b) in blocks(body, "PIN", false) {
            let pb = body[a..b].join("\n");
            let record = LefPin {
                direction: word_after(&pb, "DIRECTION").map(str::to_string),
                usage: word_after(&pb, "USE").unwrap_or("SIGNAL").to_string(),
                gate_area_um2: area_sum(&pb, "ANTENNAGATEAREA")?,
                diff_area_um2: area_sum(&pb, "ANTENNADIFFAREA")?,
            };
            match pins.iter_mut().find(|(n, _)| n == pin) {
                Some(slot) => slot.1 = record,
                None => pins.push((pin.to_string(), record)),
            }
        }
        let record = LefMacro {
            cls: lef_class(body),
            size_um: size.as_ref().map(|s| (s.0, s.1)),
            width_text: size.map(|s| s.2),
            pins,
        };
        match out.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = record,
            None => out.push((name.to_string(), record)),
        }
    }
    Ok(out)
}

fn size_json(size: Option<(f64, f64)>) -> Json {
    size.map_or(Json::Null, |(w, h)| {
        Json::List(vec![Json::Float(w), Json::Float(h)])
    })
}

// ---------------------------------------------------------------------------------------------
// frontend-profiles.

fn pins_by<'g>(cell: &'g Group, direction: &str) -> Vec<&'g Group> {
    cell.sub("pin")
        .filter(|p| p.attr("direction") == Some(direction))
        .collect()
}

/// `buffer_data(library, name, rep)` of the delay profile builder: the data of a proven
/// non-inverting buffer, or the reason it is none.
fn buffer_data(
    lib: &Library,
    name: &str,
    slew_ns: f64,
    load_pf: f64,
) -> Res<Result<Vec<(&'static str, Json)>, String>> {
    let Some(cell) = lib.cell(name) else {
        return Ok(Err("absent from this corner Liberty".into()));
    };
    if cell.attr("dont_use").unwrap_or("false").to_lowercase() == "true" {
        return Ok(Err("Liberty dont_use".into()));
    }
    let pins: Vec<&Group> = cell.sub("pin").collect();
    let (inputs, outputs) = (pins_by(cell, "input"), pins_by(cell, "output"));
    if inputs.len() != 1 || outputs.len() != 1 || pins.len() != 2 {
        return Ok(Err(format!(
            "{} inputs / {} outputs (needs exactly one of each)",
            inputs.len(),
            outputs.len()
        )));
    }
    let (a, z) = (inputs[0], outputs[0]);
    let Some(function) = z.attr("function") else {
        return Ok(Err("output has no Liberty function".into()));
    };
    let bare: String = function
        .chars()
        .filter(|c| !(c.is_ascii() && space(*c as u8)) && !matches!(c, '(' | ')' | '"'))
        .collect();
    if bare != a.name() {
        return Ok(Err(format!(
            "Liberty function '{function}' is not the identity of {}",
            a.name()
        )));
    }
    let arcs: Vec<&Group> = z
        .sub("timing")
        .filter(|t| t.attr("related_pin") == Some(a.name()))
        .collect();
    if arcs.len() != 1 {
        return Ok(Err(format!("{} timing arcs from {}", arcs.len(), a.name())));
    }
    let arc = arcs[0];
    let sense = arc.attr("timing_sense");
    let kind = arc.attr("timing_type").unwrap_or("combinational");
    if sense != Some("positive_unate") || kind != "combinational" {
        return Ok(Err(format!(
            "timing arc {}/{kind} is not positive_unate combinational",
            sense.unwrap_or("None")
        )));
    }
    let (t, c) = (lib.time_unit_ns()?, lib.cap_unit_pf()?);
    let (slew, load) = (slew_ns / t, load_pf / c);
    let look = |kind: &str| -> Res<Json> {
        let groups: Vec<&Group> = arc.sub(kind).collect();
        if groups.len() != 1 {
            return Err(Failure::new(
                "LIBERTY",
                format!("{name}: {kind} table count {}", groups.len()),
            ));
        }
        Ok(Json::Float(py_round(
            Table::new(groups[0], lib)?.lookup(slew, load)? * t,
            9,
        )))
    };
    let missing = |what: &str| Failure::new("INPUT", format!("{name}: {what} missing"));
    let cap = liberty::float(
        a.attr("capacitance")
            .ok_or_else(|| missing("capacitance"))?,
    )?;
    let area = liberty::float(cell.attr("area").ok_or_else(|| missing("area"))?)?;
    Ok(Ok(vec![
        ("input_pin", Json::str(a.name())),
        ("output_pin", Json::str(z.name())),
        ("function", Json::str(function)),
        ("area_um2", Json::Float(area)),
        ("input_cap_pf", Json::Float(py_round(cap * c, 9))),
        ("timing_sense", Json::str("positive_unate")),
        (
            "delay_ns",
            obj(vec![
                ("rise", look("cell_rise")?),
                ("fall", look("cell_fall")?),
            ]),
        ),
        (
            "output_slew_ns",
            obj(vec![
                ("rise", look("rise_transition")?),
                ("fall", look("fall_transition")?),
            ]),
        ),
    ]))
}

/// `norm(expr)` of the sizing profile builder.
fn norm(expr: Option<&str>) -> String {
    expr.unwrap_or("")
        .trim_matches('"')
        .chars()
        .filter(|c| !(c.is_ascii() && space(*c as u8)))
        .collect()
}

fn sorted_object(mut items: Vec<(String, Json)>) -> Json {
    items.sort_by(|a, b| a.0.cmp(&b.0));
    Json::Object(items)
}

/// `describe(cell)` of the sizing profile builder: the signature of a sizable cell.
fn describe(cell: &Group) -> Result<Vec<(&'static str, Json)>, String> {
    if cell.sub("statetable").next().is_some()
        || cell
            .attr("clock_gating_integrated_cell")
            .is_some_and(|v| !v.is_empty())
    {
        return Err("state table or clock-gating cell".into());
    }
    let mut pins: Vec<(String, Json)> = Vec::new();
    let mut functions: Vec<(String, Json)> = Vec::new();
    let put = |items: &mut Vec<(String, Json)>, k: &str, v: Json| match items
        .iter_mut()
        .find(|(n, _)| n == k)
    {
        Some(slot) => slot.1 = v,
        None => items.push((k.to_string(), v)),
    };
    for pin in cell.sub("pin") {
        let direction = pin.attr("direction");
        if !matches!(direction, Some("input" | "output")) {
            return Err(format!(
                "pin {} direction {}",
                pin.name(),
                direction.unwrap_or("None")
            ));
        }
        put(&mut pins, pin.name(), Json::str(direction.unwrap_or("")));
        if direction == Some("output") {
            if pin.attr("three_state").is_some_and(|v| !v.is_empty()) {
                return Err("three-state output".into());
            }
            let function = pin.attr("function").filter(|v| !v.is_empty());
            if function.is_none() {
                return Err(format!("output {} without function", pin.name()));
            }
            put(&mut functions, pin.name(), Json::Str(norm(function)));
        }
    }
    if cell.sub("bus").next().is_some() || cell.sub("bundle").next().is_some() {
        return Err("bus or bundle pins".into());
    }
    if functions.is_empty() {
        return Err("no output with a function".into());
    }
    const SEQUENTIAL_KEYS: [&str; 8] = [
        "clocked_on",
        "next_state",
        "clear",
        "preset",
        "clear_preset_var1",
        "clear_preset_var2",
        "enable",
        "data_in",
    ];
    let mut seq = Vec::new();
    for g in &cell.groups {
        if matches!(g.kind.as_str(), "ff" | "latch" | "ff_bank" | "latch_bank") {
            let mut item = vec![
                Json::str(&g.kind),
                Json::List(g.args.iter().map(|a| Json::str(unquote(a))).collect()),
            ];
            for k in SEQUENTIAL_KEYS {
                if let Some(v) = g.raw(k).filter(|v| !v.is_empty()) {
                    item.push(Json::List(vec![Json::str(k), Json::Str(norm(Some(v)))]));
                }
            }
            seq.push(Json::List(item));
        }
    }
    Ok(vec![
        ("pins", sorted_object(pins)),
        ("functions", sorted_object(functions)),
        ("sequential", Json::List(seq)),
    ])
}

const DRIVE_INPUT_SLEW_NS: f64 = 0.2;
const DRIVE_LOADS_PF: [f64; 2] = [0.005, 0.05];

/// `drive_data(cell, library)` of the sizing profile builder: input caps and the worst output
/// delay and transition at the two drive loads.
fn drive_data(cell: &Group, lib: &Library) -> Res<Json> {
    let (t, c) = (lib.time_unit_ns()?, lib.cap_unit_pf()?);
    let mut caps: Vec<(String, Json)> = Vec::new();
    for pin in cell.sub("pin") {
        if pin.attr("direction") == Some("input") {
            let Some(cap) = pin.attr("capacitance").and_then(|v| liberty::float(v).ok()) else {
                return Ok(Json::Null);
            };
            let value = Json::Float(py_round(cap * c, 9));
            match caps.iter_mut().find(|(n, _)| n == pin.name()) {
                Some(slot) => slot.1 = value,
                None => caps.push((pin.name().to_string(), value)),
            }
        }
    }
    let caps_only = |caps: Vec<(String, Json)>| obj(vec![("input_cap_pf", sorted_object(caps))]);
    let outputs = pins_by(cell, "output");
    if outputs.len() != 1 {
        return Ok(caps_only(caps));
    }
    let (mut delay, mut slew) = ([0.0f64; 2], [0.0f64; 2]);
    let mut arcs = 0;
    for arc in outputs[0].sub("timing") {
        let kind = arc.attr("timing_type").unwrap_or("combinational");
        if !matches!(kind, "combinational" | "rising_edge" | "falling_edge") {
            continue;
        }
        for (k, load) in DRIVE_LOADS_PF.iter().enumerate() {
            for (kind, to_delay) in [
                ("cell_rise", true),
                ("cell_fall", true),
                ("rise_transition", false),
                ("fall_transition", false),
            ] {
                for g in arc.sub(kind) {
                    let looked = Table::new(g, lib)
                        .and_then(|table| table.lookup(DRIVE_INPUT_SLEW_NS / t, load / c));
                    let v = match looked {
                        Ok(v) => v * t,
                        Err(e) if e.code == "LIBERTY" => return Ok(caps_only(caps)),
                        Err(e) => return Err(e),
                    };
                    let out = if to_delay { &mut delay } else { &mut slew };
                    if v > out[k] {
                        out[k] = v;
                    }
                }
            }
        }
        arcs += 1;
    }
    if arcs == 0 {
        return Ok(caps_only(caps));
    }
    let rounded =
        |x: &[f64; 2]| Json::List(x.iter().map(|v| Json::Float(py_round(*v, 6))).collect());
    Ok(obj(vec![
        ("input_cap_pf", sorted_object(caps)),
        ("output_pin", Json::str(outputs[0].name())),
        ("delay_ns", rounded(&delay)),
        ("slew_ns", rounded(&slew)),
    ]))
}

/// `create_clock -name N -period P [get_ports {PORT}]`.
fn create_clock(line: &str) -> Option<(&str, &str, &str)> {
    let mut c = Scan::new(line);
    if !c.lit("create_clock -name ") {
        return None;
    }
    let name = c.run(|ch| !ch.is_whitespace())?;
    if !c.lit(" -period ") {
        return None;
    }
    let period = c.run(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E' | '+' | '-'))?;
    if !c.lit(" [get_ports {") {
        return None;
    }
    let port = c.run(|ch| ch != '{' && ch != '}')?;
    (c.lit("}]") && c.end()).then_some((name, period, port))
}

/// `float(line.split()[1])`.
fn second_float(line: &str) -> Res<f64> {
    let t = tokens(line);
    liberty::float(
        t.get(1)
            .ok_or_else(|| Failure::new("INPUT", "list index out of range"))?,
    )
}

/// `io_and_clock(sdc, selection)`: the IO projection, the CTS projection and the clock.
fn io_and_clock(c: &Context, sdc: &Path, selection: &Json) -> Res<(Json, String, Json)> {
    let mut drivers: Vec<(String, Json)> = Vec::new();
    let mut loads: Vec<(String, Json)> = Vec::new();
    let mut clock: Vec<(String, Json)> = Vec::new();
    let put = |items: &mut Vec<(String, Json)>, k: &str, v: Json| match items
        .iter_mut()
        .find(|(n, _)| n == k)
    {
        Some(slot) => slot.1 = v,
        None => items.push((k.to_string(), v)),
    };
    let sdc_text = read_text(sdc)?;
    for line in stripped_lines(&sdc_text) {
        need(
            !line.starts_with("set_units"),
            "explicit IO projection requires ns/pF source units",
        )?;
        if line.starts_with("set_driving_cell") {
            let m = driving_cell(line);
            need(m.is_some(), "unsupported literal driving-cell projection")?;
            if let Some((cell, pin, _, _, port)) = m {
                let d = obj(vec![("cell", Json::str(cell)), ("pin", Json::str(pin))]);
                put(&mut drivers, port, d);
            }
        }
        if line.starts_with("set_load") {
            let m = set_load(line, true);
            need(m.is_some(), "unsupported literal load projection")?;
            if let Some((value, ports)) = m {
                put(&mut loads, ports, Json::Float(liberty::float(value)?));
            }
        }
        if line.starts_with("create_clock") {
            let m = create_clock(line);
            need(
                m.is_some() && clock.is_empty(),
                "one literal primary clock required",
            )?;
            if let Some((name, period, port)) = m {
                clock = vec![
                    ("name".to_string(), Json::str(name)),
                    ("period".to_string(), Json::Float(liberty::float(period)?)),
                    ("port".to_string(), Json::str(port)),
                ];
            }
        }
        if line.starts_with("set_clock_transition") {
            put(&mut clock, "transition", Json::Float(second_float(line)?));
        }
        if line.starts_with("set_clock_uncertainty") {
            put(&mut clock, "uncertainty", Json::Float(second_float(line)?));
        }
        if line.starts_with("set_max_fanout") {
            put(&mut clock, "fanout", Json::Float(second_float(line)?));
        }
    }
    let clock = Json::Object(clock);
    let primary = text(selection, "primary_clock")?;
    let fields = [
        "name",
        "port",
        "period",
        "transition",
        "uncertainty",
        "fanout",
    ];
    need(
        clock.get("name").and_then(Json::as_str) == Some(primary)
            && clock.get("port").and_then(Json::as_str) == Some(primary)
            && clock.as_object().is_some_and(|items| items.len() == 6)
            && fields.iter().all(|f| clock.get(f).is_some()),
        "incomplete original clock projection",
    )?;
    let number = |k: &str| -> Res<String> { Ok(repr(at(&clock, k)?.float()?)) };
    let n = primary;
    let projection = format!(
        "# CTS-only literal projection; full original SDC remains mandatory for STA.\n\
         create_clock -name {n} -period {} [get_ports {{{n}}}]\n\
         set_clock_transition {} [get_clocks {{{n}}}]\n\
         set_clock_uncertainty {} [get_clocks {{{n}}}]\n\
         set_propagated_clock [get_clocks {{{n}}}]\n\
         set_max_fanout {} [current_design]\n",
        number("period")?,
        number("transition")?,
        number("uncertainty")?,
        number("fanout")?,
    );
    let io = obj(vec![
        ("sdc_sha256", at(&c.rec(sdc)?, "sha256")?.clone()),
        ("drivers", Json::Object(drivers)),
        ("loads_pf", Json::Object(loads)),
    ]);
    Ok((io, projection, clock))
}

fn with_role(role: &str, record: Json) -> Json {
    let mut items = vec![("role".to_string(), Json::str(role))];
    items.extend(record.as_object().unwrap_or(&[]).iter().cloned());
    Json::Object(items)
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `profiles(c, out)`.
fn profiles(c: &Context, out: &Path) -> Res<()> {
    let selection = c.selection()?;
    let mut names: Vec<String> = Vec::new();
    for x in list(selection, "buffers")? {
        names.push(text(x, "cell")?.to_string());
    }
    let delay_cells = strs(selection, "delay_cells")?;
    names.extend(delay_cells.iter().cloned());
    let unique: HashSet<&str> = names.iter().map(String::as_str).collect();
    need(
        names.len() <= 32 && unique.len() == names.len(),
        "explicit cell selection budget",
    )?;
    let diode = selection.get("port_diode").filter(|d| **d != Json::Null);
    if let Some(d) = diode {
        let ok = d.as_object().is_some_and(|items| {
            items.len() == 2 && d.get("cell").is_some() && d.get("pin").is_some()
        }) && d
            .get("cell")
            .and_then(Json::as_str)
            .is_some_and(|cell| !unique.contains(cell));
        need(ok, "port diode selection")?;
    }
    let mut wanted: Vec<&str> = names.iter().map(String::as_str).collect();
    if let Some(cell) = diode.and_then(|d| d.get("cell")).and_then(Json::as_str) {
        wanted.push(cell);
    }
    let corners = ["tt", "ss"];
    // The two corner libraries are read beside each other.
    let (std_tt, std_ss) = (c.file("std_tt")?, c.file("std_ss")?);
    let (lib_tt, lib_ss) = std::thread::scope(|scope| {
        let ss = scope.spawn(|| Library::read(&std_ss, Some(&wanted)));
        let tt = Library::read(&std_tt, Some(&wanted));
        (
            tt,
            ss.join()
                .unwrap_or_else(|_| Err(Failure::new("INTERNAL", "Liberty reader"))),
        )
    });
    let libs = [("tt", lib_tt?), ("ss", lib_ss?)];
    let std_lef = c.file("std_lef")?;
    let macros = lef_macros(&read_text(&std_lef)?)?;
    let macro_of = |n: &str| macros.iter().find(|(k, _)| k == n).map(|(_, m)| m);
    // lef_widths(std_lef, 1000) of the sizing profile builder.
    let mut widths: HashMap<&str, i64> = HashMap::default();
    for (name, m) in &macros {
        if let (Some((w, _)), Some(written)) = (m.size_um, &m.width_text) {
            let width = w * 1000.0;
            if (width - width.round_ties_even()).abs() > 1e-6 {
                return Err(Failure::new(
                    "INPUT",
                    format!("{name}: width {written} um is not an integer number of DBU"),
                ));
            }
            widths.insert(name, width.round_ties_even() as i64);
        }
    }
    let mut antenna = Vec::new();
    if let Some(d) = diode {
        // Same admission as the delay profile builder: LEF ANTENNACELL, one INPUT signal pin
        // with ANTENNADIFFAREA, and a function-free single-pin Liberty cell.
        let (n, pin) = (text(d, "cell")?, text(d, "pin")?);
        let m = macro_of(n);
        let signal: Vec<&(String, LefPin)> = m
            .map(|m| m.pins.iter().filter(|(_, v)| v.usage == "SIGNAL").collect())
            .unwrap_or_default();
        let only = (signal.len() == 1 && signal[0].0 == pin).then(|| &signal[0].1);
        let ok = m.is_some_and(|m| m.cls.as_deref() == Some("CORE ANTENNACELL"))
            && only.is_some_and(|p| {
                p.direction.as_deref() == Some("INPUT") && p.diff_area_um2.is_some_and(|a| a != 0.0)
            });
        need(ok, "port diode is not a single-input LEF antenna cell")?;
        for (co, lib) in &libs {
            let cell = lib.cell(n);
            let pins: Vec<&Group> = cell.map(|c| c.sub("pin").collect()).unwrap_or_default();
            need(
                cell.is_some()
                    && pins.len() == 1
                    && pins[0].name() == pin
                    && !pins
                        .iter()
                        .any(|g| g.attr("function").is_some_and(|f| !f.is_empty())),
                format!("{co}/{n}: port diode Liberty pin set"),
            )?;
        }
        let area = libs[0]
            .1
            .cell(n)
            .and_then(|cell| cell.attr("area"))
            .ok_or_else(|| Failure::new("INPUT", "port diode area"))?;
        let (m, p) = (
            m.ok_or_else(|| key_error(n))?,
            only.ok_or_else(|| key_error(pin))?,
        );
        antenna.push(obj(vec![
            ("name", Json::str(n)),
            ("pin", Json::str(pin)),
            ("area_um2", Json::Float(liberty::float(area)?)),
            ("lef_size_um", size_json(m.size_um)),
            ("lef_diff_area_um2", Json::opt_float(p.diff_area_um2)),
            (
                "lef_gate_area_um2",
                Json::Float(p.gate_area_um2.filter(|a| *a != 0.0).unwrap_or(0.0)),
            ),
            ("policy_class", Json::str("physical_only")),
        ]));
    }
    let sources = Json::List(vec![
        with_role("liberty_tt", c.rec(&c.file("std_tt")?)?),
        with_role("liberty_ss", c.rec(&c.file("std_ss")?)?),
        with_role("cell_lef", c.rec(&c.file("cell_lef")?)?),
        with_role("original_cell_lef", c.rec(&std_lef)?),
    ]);
    let corner_list = Json::List(corners.iter().map(|c| Json::str(*c)).collect());
    let policy = obj(vec![
        ("allowed", Json::List(names.iter().map(Json::str).collect())),
        ("corners", corner_list.clone()),
        (
            "purpose",
            Json::str("identity buffer/delay flow policy from original PDK"),
        ),
    ]);
    write_json(&out.join("cell-policy.json"), &policy)?;
    let (slew_ns, load_pf) = (0.2, 0.01);
    let mut buffers = Vec::new();
    // (max_capacitance_pf, area_um2, name, record)
    let mut members: Vec<(Option<f64>, f64, String, Json)> = Vec::new();
    for name in &names {
        let mut per: Vec<Vec<(&'static str, Json)>> = Vec::new();
        for (co, lib) in &libs {
            match buffer_data(lib, name, slew_ns, load_pf)? {
                Ok(d) => per.push(d),
                Err(reason) => return Err(Failure::new("INPUT", format!("{co}/{name}: {reason}"))),
            }
        }
        let field = |d: &[(&'static str, Json)], k: &str| -> Json {
            d.iter()
                .find(|(n, _)| *n == k)
                .map_or(Json::Null, |(_, v)| v.clone())
        };
        let by_corner = |k: &str| {
            Json::Object(
                corners
                    .iter()
                    .zip(&per)
                    .map(|(co, d)| (co.to_string(), field(d, k)))
                    .collect(),
            )
        };
        let reference = &per[0];
        let m = macro_of(name).ok_or_else(|| key_error(name))?;
        buffers.push(obj(vec![
            ("name", Json::str(name)),
            ("input_pin", field(reference, "input_pin")),
            ("output_pin", field(reference, "output_pin")),
            ("function", field(reference, "function")),
            (
                "function_check",
                Json::str("identity in original TT/SS Liberty"),
            ),
            ("timing_sense", Json::str("positive_unate")),
            ("area_um2", field(reference, "area_um2")),
            ("lef_size_um", size_json(m.size_um)),
            ("policy_class", Json::str("cts_hold")),
            ("input_cap_pf", by_corner("input_cap_pf")),
            ("delay_ns", by_corner("delay_ns")),
            ("output_slew_ns", by_corner("output_slew_ns")),
        ]));
        if !delay_cells.contains(name) {
            let cell = libs[0].1.cell(name).ok_or_else(|| key_error(name))?;
            let output = pins_by(cell, "output");
            let cap = output
                .first()
                .ok_or_else(|| Failure::new("INPUT", "StopIteration"))?
                .attr("max_capacitance")
                .filter(|v| !v.is_empty());
            let cap = match cap {
                Some(v) => Some(liberty::float(v)?),
                None => None,
            };
            let area = liberty::float(
                cell.attr("area")
                    .ok_or_else(|| Failure::new("INPUT", "cell area missing"))?,
            )?;
            let mut drive = Vec::new();
            for (co, lib) in &libs {
                let cell = lib.cell(name).ok_or_else(|| key_error(name))?;
                drive.push((*co, drive_data(cell, lib)?));
            }
            let width = widths.get(name.as_str()).ok_or_else(|| key_error(name))?;
            let record = obj(vec![
                ("name", Json::str(name)),
                ("width_dbu", Json::Int((*width).into())),
                ("area_um2", Json::Float(area)),
                ("max_capacitance_pf", Json::opt_float(cap)),
                ("drive", obj(drive)),
            ]);
            members.push((cap, area, name.clone(), record));
        }
    }
    let generator = Json::Str(show(&resolve(&c.file("prepare")?)?));
    let policy_record = c.rec(&out.join("cell-policy.json"))?;
    let delay = obj(vec![
        ("schema", Json::str("rustroad.delay-cells.v1")),
        ("pdk", Json::str("ihp-sg13g2")),
        ("library", Json::str("ez130_8t")),
        ("generator", generator.clone()),
        ("sources", sources.clone()),
        (
            "cell_policy",
            obj(vec![
                ("path", at(&policy_record, "path")?.clone()),
                ("sha256", at(&policy_record, "sha256")?.clone()),
            ]),
        ),
        (
            "corners",
            Json::List(vec![
                obj(vec![
                    ("name", Json::str("tt")),
                    ("liberty", Json::Str(stem(&std_tt))),
                    ("rc", Json::str("nom")),
                ]),
                obj(vec![
                    ("name", Json::str("ss")),
                    ("liberty", Json::Str(stem(&std_ss))),
                    ("rc", Json::str("nom")),
                ]),
            ]),
        ),
        (
            "representative",
            obj(vec![
                ("input_slew_ns", Json::Float(slew_ns)),
                ("load_pf", Json::Float(load_pf)),
                (
                    "reason",
                    Json::str("diagnostic estimate; re-time every ECO at both corners"),
                ),
            ]),
        ),
        ("selection", policy),
        ("buffers", Json::List(buffers)),
        ("antenna_cells", Json::List(antenna)),
        ("excluded", Json::List(Vec::new())),
    ]);
    write_json(&out.join("delay.json"), &delay)?;
    let leaf = text(at(selection, "reset")?, "leaf")?;
    let signature = libs[0]
        .1
        .cell(leaf)
        .ok_or_else(|| key_error(leaf))
        .map(describe)?;
    need(signature.is_ok(), "sizing signature")?;
    members.sort_by(|a, b| {
        let cap = |m: &(Option<f64>, f64, String, Json)| m.0.filter(|c| *c != 0.0).unwrap_or(0.0);
        cap(a)
            .partial_cmp(&cap(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut family = vec![
        ("id", Json::str("BUFX")),
        ("order", Json::str("max_capacitance_pf then area_um2")),
    ];
    family.extend(signature.unwrap_or_default());
    family.push((
        "cells",
        Json::List(members.into_iter().map(|m| m.3).collect()),
    ));
    let sizing = obj(vec![
        ("schema", Json::str("rustroad.sizing-profile.v1")),
        ("pdk", Json::str("ihp-sg13g2")),
        ("library", Json::str("ez130_8t")),
        ("generator", generator),
        ("sources", sources),
        ("dbu_per_um", Json::Int(1000)),
        (
            "rule",
            Json::str("original TT/SS identity functions and original LEF geometry"),
        ),
        (
            "drive_model",
            obj(vec![
                ("input_slew_ns", Json::Float(DRIVE_INPUT_SLEW_NS)),
                (
                    "loads_pf",
                    Json::List(DRIVE_LOADS_PF.iter().map(|v| Json::Float(*v)).collect()),
                ),
                ("corners", corner_list),
            ]),
        ),
        ("families", Json::List(vec![obj(family)])),
        ("excluded", Json::List(Vec::new())),
    ]);
    write_json(&out.join("sizing.json"), &sizing)?;
    let interfaces = read_json(&c.file("interfaces")?, 1 << 20)?;
    let floorplan = read_json(&c.file("floorplan")?, 1 << 20)?;
    let buses = at(&interfaces, "cell_buses")?;
    need(
        buses.py_eq(at(&floorplan, "pin_buses")?),
        "original interface and floorplan bus ranges differ",
    )?;
    write_json(&out.join("cell-buses.json"), buses)?;
    let sdc = c.file("sdc")?;
    let (io, cts, clock) = io_and_clock(c, &sdc, selection)?;
    write_json(&out.join("io.json"), &io)?;
    fs::write(out.join("cts.sdc"), cts)?;
    write_json(
        &out.join("receipt.json"),
        &obj(vec![
            ("status", Json::str("profiles_prepared")),
            ("inputs", at(&c.doc, "files")?.clone()),
            ("clock_projection", clock),
            ("full_sdc", c.rec(&sdc)?),
            (
                "unverified",
                Json::List(vec![Json::str("all physical/timing signoff")]),
            ),
        ]),
    )
}

// ---------------------------------------------------------------------------------------------
// frontend-physical.

/// `Path(p).resolve()` of a path that need not exist: the canonical form of its longest
/// existing prefix, then the rest as written.
fn resolve_lenient(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut head = p.to_path_buf();
    loop {
        if let Ok(real) = fs::canonicalize(&head) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (head.parent().map(Path::to_path_buf), head.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                head = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// `routing_capture(routing, root)`: materializes the one explicit run-local, bounded
/// failure-capture policy.
fn routing_capture(routing: &mut Json, root: &Path) -> Res<Option<Vec<(&'static str, Json)>>> {
    const TEMPLATE: &str = "{output}/backend/route-budget-error.json";
    let replay = routing.get("limits").and_then(|l| l.get("replay"));
    let path = replay.and_then(|r| r.get("path"));
    let is_dict = replay.is_some_and(|r| r.as_object().is_some());
    if !is_dict || path.and_then(Json::as_str) != Some(TEMPLATE) {
        need(
            !(is_dict
                && path
                    .and_then(Json::as_str)
                    .is_some_and(|p| p.contains("{output}"))),
            "unsupported routing capture template",
        )?;
        return Ok(None);
    }
    let replay = replay.ok_or_else(|| key_error("replay"))?;
    let fields = ["path", "net", "round", "max_bytes", "phase"];
    need(
        replay.as_object().is_some_and(|items| items.len() == 5)
            && fields.iter().all(|f| replay.get(f).is_some()),
        "routing capture template keys differ",
    )?;
    let max_bytes = at(replay, "max_bytes")?.clone();
    let phase = at(replay, "phase")?.clone();
    need(
        *at(replay, "net")? == Json::Null
            && *at(replay, "round")? == Json::Null
            && max_bytes == Json::Int(536_870_912)
            && phase.as_str() == Some("post_budget_error"),
        "routing capture template policy differs",
    )?;
    let root = resolve(root)?;
    let literal = root.join("backend/route-budget-error.json");
    need(
        fs::symlink_metadata(&literal).is_err(),
        "routing capture output already exists",
    )?;
    let capture = resolve_lenient(&literal);
    need(
        capture.starts_with(&root) && capture != root,
        "routing capture escapes this producer run",
    )?;
    need(
        fs::symlink_metadata(&capture).is_err(),
        "routing capture output already exists",
    )?;
    if let Json::Object(top) = routing {
        if let Some((_, Json::Object(limits))) = top.iter_mut().find(|(k, _)| k == "limits") {
            if let Some((_, replay)) = limits.iter_mut().find(|(k, _)| k == "replay") {
                replay.set("path", Json::Str(show(&capture)));
            }
        }
    }
    Ok(Some(vec![
        ("template", Json::str(TEMPLATE)),
        ("snapshot_path", Json::Str(show(&capture))),
        ("max_bytes", max_bytes),
        ("phase", phase),
        (
            "scope",
            Json::str("failure diagnostic only; no route or signoff pass"),
        ),
    ]))
}

/// `physical(c, args, out)`.
fn physical(c: &Context, roots_path: &Path, out: &Path) -> Res<()> {
    let roots = read_json(&c.dynamic(roots_path)?, 1 << 20)?;
    let mut data = read_json(&c.file("physical")?, 1 << 20)?;
    let root_net = at(&roots, "clock_root")?.clone();
    at(&data, "clock")?;
    if let Json::Object(items) = &mut data {
        if let Some((_, clock)) = items.iter_mut().find(|(k, _)| k == "clock") {
            need(clock.as_object().is_some(), "physical clock policy")?;
            clock.set("root_net", root_net);
        }
    }
    // Only lattices with out-of-flow LU.a/LU.b evidence on this floorplan: the legacy aligned
    // 15 um, the 28.98 um stagger, and the 42 um stagger with unpaired n-well edge
    // densification. No in-flow step checks LU.a.
    let tap = at(&data, "tap")?;
    let max_gap = at(tap, "max_gap")?;
    let pattern = tap.get("pattern").unwrap_or(&Json::Null);
    let staggered = |phase: i128, unpaired: Option<&str>| {
        let mut items = vec![
            ("kind", Json::str("staggered")),
            ("phase", Json::Int(phase)),
        ];
        if let Some(u) = unpaired {
            items.push(("unpaired", Json::str(u)));
        }
        obj(items)
    };
    let lattices = [
        (Json::Int(15000), Json::Null),
        (Json::Int(28980), staggered(14490, None)),
        (Json::Int(42000), staggered(21000, Some("cell_top"))),
    ];
    need(
        list(&data, "filler_cells")?.len() == 9
            && lattices
                .iter()
                .any(|(gap, p)| gap.py_eq(max_gap) && p.py_eq(pattern))
            && at(tap, "body_view")?.as_str() == Some("gds"),
        "matched decap/tap template differs",
    )?;
    write_json(&out.join("physical.json"), &data)?;
    let mut routing = read_json(&c.file("routing")?, 1 << 20)?;
    let layers = at(&routing, "track_router")?;
    need(
        at(layers, "min_routing_layer")?.as_str() == Some("Metal2")
            && at(layers, "max_routing_layer")?.as_str() == Some("TopMetal2"),
        "matched routing layer policy differs",
    )?;
    let capture = routing_capture(&mut routing, &c.root)?;
    write_json(&out.join("routing.json"), &routing)?;
    let mut inputs = Vec::new();
    for k in ["physical", "floorplan", "routing"] {
        inputs.push((k, c.rec(&c.file(k)?)?));
    }
    let mut receipt = vec![
        ("inputs", obj(inputs)),
        ("roots", c.rec(roots_path)?),
        (
            "scope",
            Json::str("route policy prepared, route not executed"),
        ),
    ];
    if let Some(mut capture) = capture {
        capture.push(("materialized_routing", c.rec(&out.join("routing.json"))?));
        receipt.push(("routing_capture", obj(capture)));
    }
    write_json(&out.join("receipt.json"), &obj(receipt))
}

// ---------------------------------------------------------------------------------------------
// frontend-pins.

fn int_of(v: &Json, k: &str) -> Res<i64> {
    at(v, k)?
        .as_int()
        .ok_or_else(|| Failure::new("INPUT", format!("integer required: {k}")))
}

/// `a // b` for a positive `b`.
fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}

/// `macro_sizes(lef)`: MACRO -> (width, height) in DBU of 1/1000 um.
fn macro_sizes(lef: &Path) -> Res<HashMap<String, (i64, i64)>> {
    let mut sizes = HashMap::default();
    let mut name: Option<String> = None;
    let dbu = |t: &str| -> Res<i64> { Ok((liberty::float(t)? * 1000.0).round_ties_even() as i64) };
    for line in read_text(lef)?.split('\n') {
        let t = tokens(line);
        if t.len() >= 2 && t[0] == "MACRO" {
            name = Some(t[1].to_string());
        } else if t.len() >= 4 && t[0] == "SIZE" && t[2] == "BY" {
            if let Some(n) = name.take().filter(|n| !n.is_empty()) {
                sizes.insert(n, (dbu(t[1])?, dbu(t[3])?));
            }
        }
    }
    need(
        !sizes.is_empty() && sizes.len() <= 4096,
        "cell LEF macro budget",
    )?;
    Ok(sizes)
}

/// `re.search(r'\nNAME (\d+) ;(.*?)\nEND NAME', text, re.S)`: the count and the body.
fn section<'t>(text: &'t str, name: &str) -> Option<(&'t str, &'t str)> {
    let head = format!("\n{name} ");
    let tail = format!("\nEND {name}");
    let mut from = 0;
    while let Some(k) = text[from..].find(&head) {
        let start = from + k + head.len();
        let mut c = Scan::new(&text[start..]);
        if let Some(count) = c.digits() {
            if c.lit(" ;") {
                let body = c.rest();
                if let Some(end) = body.find(&tail) {
                    return Some((count, &body[..end]));
                }
            }
        }
        from += k + 1;
    }
    None
}

/// `re.search(r'\+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \) (\S+)', item)`.
fn placed(item: &str) -> Option<(i64, i64, &str)> {
    let int = |c: &mut Scan<'_>| -> Option<i64> {
        let rest = c.rest();
        c.lit("-");
        c.digits()?;
        rest[..rest.len() - c.rest().len()].parse().ok()
    };
    let mut from = 0;
    while let Some(k) = item[from..].find("+ ") {
        let mut c = Scan::new(&item[from + k + 2..]);
        if (c.lit("PLACED") || c.lit("FIXED")) && c.lit(" ( ") {
            if let Some(x) = int(&mut c) {
                if c.lit(" ") {
                    if let Some(y) = int(&mut c) {
                        if c.lit(" ) ") {
                            if let Some(o) = c.run(|ch| !ch.is_whitespace()) {
                                return Some((x, y, o));
                            }
                        }
                    }
                }
            }
        }
        from += k + 1;
    }
    None
}

/// `re.findall(r'\( (\S+) (\S+) \)', text)`.
fn pairs(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(k) = text[from..].find("( ") {
        let begin = from + k;
        let mut c = Scan::new(&text[begin + 2..]);
        let found = (|| {
            let a = c.run(|ch| !ch.is_whitespace())?;
            if !c.lit(" ") {
                return None;
            }
            let b = c.run(|ch| !ch.is_whitespace())?;
            c.lit(" )").then_some((a, b))
        })();
        match found {
            Some(pair) => {
                out.push(pair);
                from = text.len() - c.rest().len();
            }
            None => from = begin + 1,
        }
    }
    out
}

/// `pins(c, args, out)`: the load-aware I/O plan from this run's pin-planning placement (DBU,
/// die origin 0,0): each signal pin takes the nearest core side of its load centroid,
/// strongest preference first, with the centroid as its along-side target.
fn pins(c: &Context, def: &Path, out: &Path) -> Res<()> {
    let root = &c.root;
    let placed_def = c.dynamic(def)?;
    need(
        fs::metadata(&placed_def)?.len() <= 128 << 20,
        "placed DEF byte budget",
    )?;
    let physical_path = root.join("frontend/physical/physical.json");
    let mut data = read_json(&c.dynamic(&physical_path)?, 1 << 20)?;
    need(
        data.get("io_plan").is_none(),
        "physical config already plans its pins",
    )?;
    let cell_lef = c.file("cell_lef")?;
    let sizes = macro_sizes(&cell_lef)?;
    let diode = c
        .selection()?
        .get("port_diode")
        .and_then(|d| d.get("cell"))
        .and_then(Json::as_str);
    let floorplan_path = c.file("floorplan")?;
    let floor = read_json(&floorplan_path, 1 << 20)?;
    let (die, core) = (at(&floor, "die_area")?, at(&floor, "core_area")?);
    need(
        int_of(die, "lx")? == 0 && int_of(die, "ly")? == 0,
        "die origin",
    )?;
    let text = read_text(&placed_def)?;
    let components = section(&text, "COMPONENTS");
    let count = components.and_then(|(n, _)| n.parse::<i64>().ok());
    need(
        count.is_some_and(|n| n <= 1_000_000),
        "placed component budget",
    )?;
    // Instance -> (master, centre x, centre y).
    let mut cells: HashMap<&str, (&str, i64, i64)> = HashMap::default();
    for item in components.map_or("", |c| c.1).split(';') {
        let t = tokens(item);
        if t.len() < 3 || t[0] != "-" {
            continue;
        }
        let g = placed(item);
        let size = sizes.get(t[2]);
        need(
            g.is_some() && size.is_some(),
            format!("unplaced or unknown component: {}", t[1]),
        )?;
        let (Some((x, y, orientation)), Some(&(w, h))) = (g, size) else {
            continue;
        };
        let (w, h) = if matches!(orientation, "E" | "W" | "FE" | "FW") {
            (h, w)
        } else {
            (w, h)
        };
        cells.insert(t[1], (t[2], x + floor_div(w, 2), y + floor_div(h, 2)));
    }
    need(Some(cells.len() as i64) == count, "placed component count")?;
    let pin_section = section(&text, "PINS");
    need(
        pin_section
            .and_then(|(n, _)| n.parse::<i64>().ok())
            .is_some_and(|n| n <= 100_000),
        "placed pin budget",
    )?;
    let mut names: Vec<&str> = Vec::new();
    for item in pin_section.map_or("", |p| p.1).split(';') {
        if item.contains("+ USE SIGNAL") {
            let t = tokens(item);
            if t.len() >= 2 && t[0] == "-" {
                names.push(t[1]);
            }
        }
    }
    let mut index: HashMap<&str, usize> = HashMap::default();
    for (k, n) in names.iter().enumerate() {
        index.insert(n, k);
    }
    need(
        index.len() == names.len() && !names.is_empty(),
        "signal pin names",
    )?;
    let mut loads: Vec<Vec<(i64, i64)>> = vec![Vec::new(); names.len()];
    let nets = section(&text, "NETS");
    need(
        nets.and_then(|(n, _)| n.parse::<i64>().ok())
            .is_some_and(|n| n <= 2_000_000),
        "placed net budget",
    )?;
    for net in nets.map_or("", |n| n.1).split("\n- ").skip(1) {
        let head = net.split("+ ROUTED").next().unwrap_or("");
        let head = head.split(';').next().unwrap_or("");
        let ends = pairs(head);
        for &(i, p) in &ends {
            if i != "PIN" {
                continue;
            }
            let slot = index.get(p).copied();
            need(slot.is_some(), format!("net names an unplanned pin: {p}"))?;
            let slot = slot.unwrap_or(0);
            for &(inst, _) in &ends {
                if inst == "PIN" {
                    continue;
                }
                let &(master, x, y) = cells.get(inst).ok_or_else(|| key_error(inst))?;
                if Some(master) != diode {
                    loads[slot].push((x, y));
                }
            }
        }
    }
    let (clx, cux) = (int_of(core, "lx")?, int_of(core, "ux")?);
    let (cly, cuy) = (int_of(core, "ly")?, int_of(core, "uy")?);
    let mid = (floor_div(clx + cux, 2), floor_div(cly + cuy, 2));
    let centroid: Vec<(i64, i64)> = loads
        .iter()
        .map(|v| {
            if v.is_empty() {
                mid
            } else {
                let n = v.len() as i64;
                (
                    floor_div(v.iter().map(|p| p.0).sum::<i64>(), n),
                    floor_div(v.iter().map(|p| p.1).sum::<i64>(), n),
                )
            }
        })
        .collect();
    let dist = |p: (i64, i64)| [p.0 - clx, cux - p.0, p.1 - cly, cuy - p.1];
    // Conservative side capacity: a quarter of the slots at width plus spacing.
    let pitch = 4 * (int_of(&data, "io_width")?.max(1) + int_of(&data, "io_spacing")?);
    need(pitch > 0, "I/O pitch")?;
    let across = floor_div(int_of(die, "uy")? - int_of(die, "ly")?, pitch);
    let along = floor_div(int_of(die, "ux")? - int_of(die, "lx")?, pitch);
    let cap = [across, across, along, along];
    let mut sides: [Vec<usize>; 4] = Default::default();
    let mut order: Vec<usize> = (0..names.len()).collect();
    let preference = |k: usize| {
        let mut d = dist(centroid[k]);
        d.sort_unstable();
        -(d[1] - d[0])
    };
    order.sort_by(|&a, &b| {
        preference(a)
            .cmp(&preference(b))
            .then_with(|| names[a].cmp(names[b]))
    });
    for k in order {
        let d = dist(centroid[k]);
        let mut by_distance = [0usize, 1, 2, 3];
        by_distance.sort_by_key(|&s| (d[s], s));
        let side = by_distance
            .into_iter()
            .find(|&s| (sides[s].len() as i64) < cap[s]);
        need(
            side.is_some(),
            "I/O pins exceed the conservative side capacity",
        )?;
        sides[side.unwrap_or(0)].push(k);
    }
    let target = |s: usize, k: usize| {
        if s < 2 { centroid[k].1 } else { centroid[k].0 }
    };
    for (s, side) in sides.iter_mut().enumerate() {
        side.sort_by(|&a, &b| {
            target(s, a)
                .cmp(&target(s, b))
                .then_with(|| names[a].cmp(names[b]))
        });
    }
    let side_names = ["west", "east", "south", "north"];
    let mut io_plan: Vec<(&str, Json)> = Vec::new();
    for (s, side) in sides.iter().enumerate() {
        io_plan.push((
            side_names[s],
            Json::List(side.iter().map(|&k| Json::str(names[k])).collect()),
        ));
    }
    let mut targets: Vec<(String, Json)> = Vec::new();
    for (s, side) in sides.iter().enumerate() {
        for &k in side {
            targets.push((names[k].to_string(), Json::Int(target(s, k).into())));
        }
    }
    io_plan.push(("targets", Json::Object(targets)));
    data.set("io_plan", obj(io_plan));
    write_json(&out.join("physical.json"), &data)?;
    let mut plan = Vec::new();
    for (s, side) in sides.iter().enumerate() {
        for &k in side {
            plan.push(obj(vec![
                ("pin", Json::str(names[k])),
                ("side", Json::str(side_names[s])),
                ("target_dbu", Json::Int(target(s, k).into())),
                ("loads", Json::Int(loads[k].len() as i128)),
            ]));
        }
    }
    write_json(
        &out.join("receipt.json"),
        &obj(vec![
            (
                "rule",
                Json::str(
                    "nearest core side of the load centroid, strongest preference first; along-side target is the centroid; port diodes excluded",
                ),
            ),
            (
                "inputs",
                obj(vec![
                    ("placed_def", c.rec(&placed_def)?),
                    ("physical", c.rec(&physical_path)?),
                    ("cell_lef", c.rec(&cell_lef)?),
                    ("floorplan", c.rec(&floorplan_path)?),
                ]),
            ),
            (
                "side_counts",
                Json::List(sides.iter().map(|x| Json::Int(x.len() as i128)).collect()),
            ),
            ("pins", Json::List(plan)),
            (
                "scope",
                Json::str(
                    "pin plan from the pin-planning placement; the temporary, repair and final rounds import it",
                ),
            ),
        ]),
    )
}

// ---------------------------------------------------------------------------------------------
// frontend-validate.

/// `validate(c, out)`: requires the three placement rounds' clock trees, the pin plan, the
/// same-run seed, the five ECO receipts and a closed final estimated-RC DRV, and records the
/// frontend's result.
fn validate(c: &Context, out: &Path) -> Res<()> {
    let root = &c.root;
    let receipts = read_json(&root.join("step-receipts.json"), 2 << 20)?;
    for tag in ["temporary", "repair", "final"] {
        let id = format!("fe_{tag}_cts");
        let mut ids = 0;
        for r in receipts.as_list().unwrap_or(&[]) {
            if at(r, "kind")?.as_str() == Some("command") && at(r, "id")?.as_str() == Some(&id) {
                ids += 1;
            }
        }
        need(ids == 1, "exactly one CTS per fresh workspace required")?;
        let ws = root.join(format!("frontend/road-{tag}"));
        let head = fs::read_to_string(ws.join("HEAD"))?;
        need(
            head.trim_matches(|ch: char| ch.is_whitespace()) == "rev3",
            "unexpected final physical revision",
        )?;
        let report = read_json(&root.join(format!("logs/fe_{tag}_cts.stdout")), 2 << 20)?;
        let buffers = at(&report, "clock_buffers")?
            .number()
            .ok_or_else(|| Failure::new("INPUT", "clock_buffers"))?;
        need(
            at(&report, "revision")?.as_str() == Some("rev3")
                && buffers > 0.0
                && at(&report, "clock_topology_status")?.as_str()
                    == Some("unrouted_topology_checked"),
            "native CTS topology not verified",
        )?;
    }
    let pin_plan = read_json(&root.join("frontend/physical-pins/receipt.json"), 1 << 20)?;
    need(
        at(at(&pin_plan, "inputs")?, "placed_def")?
            .py_eq(&c.rec(&root.join("logs/fe_pins_def.stdout"))?),
        "pin plan belongs to another placement",
    )?;
    let seed = read_json(
        &c.dynamic(&root.join("logs/fe_temporary_seed.stdout"))?,
        64 << 20,
    )?;
    let source = at(&seed, "source")?;
    need(
        at(&seed, "schema")?.as_str() == Some("rustroad.placement-seed.v1")
            && at(source, "revision")?.as_str() == Some("rev3")
            && at(source, "imported_netlist_sha256")?.py_eq(at(
                &c.rec(&root.join("frontend/eco-primary-clock/netlist.v"))?,
                "sha256",
            )?),
        "seed not from this-run temporary imported netlist",
    )?;
    let operations = ["reset", "fanout", "primary-clock", "electrical", "hold"];
    let mut plans = Vec::new();
    for op in operations {
        let plan = read_json(
            &root.join(format!("frontend/eco-{op}/receipt.json")),
            16 << 20,
        )?;
        plans.push((op, plan));
    }
    for (op, p) in &plans {
        let expected = if matches!(*op, "electrical" | "hold") {
            "estimated_rc"
        } else {
            "ideal_diagnostic"
        };
        need(
            at(p, "scope")?.as_str() == Some(expected),
            "unexpected frontend timing scope",
        )?;
    }
    let estimate = read_json(&root.join("frontend/estimate-final/result.json"), 1 << 20)?;
    need(
        at(at(at(&estimate, "inputs")?, "def")?, "sha256")?.py_eq(at(
            &c.rec(&root.join("logs/fe_final_def.stdout"))?,
            "sha256",
        )?),
        "final estimate belongs to another placement",
    )?;
    let mut timing = Vec::new();
    for co in ["tt", "ss"] {
        let env = read_json(
            &root.join(format!("logs/fe_final_{co}_drv.stdout")),
            16 << 20,
        )?;
        let scope = at(&env, "scope")?;
        need(
            at(scope, "wire_model")?.as_str() == Some("spef_rc")
                && at(scope, "clock_model")?.as_str() == Some("propagated")
                && at(scope, "cppr")?.as_str() == Some("off"),
            "final scope differs",
        )?;
        let m = at(&env, "metrics")?;
        let mut picked = Vec::new();
        for k in [
            "max_transition_violating",
            "max_capacitance_violating",
            "max_fanout_violating",
            "worst_setup_slack_ns",
            "worst_hold_slack_ns",
            "hold_violating_endpoints",
        ] {
            picked.push((k, at(m, k)?.clone()));
        }
        for (_, v) in &picked[..3] {
            need(
                v.number() == Some(0.0) || *v == Json::Bool(false),
                format!("final estimated-RC DRV not closed: {co}"),
            )?;
        }
        timing.push((co, obj(picked)));
    }
    let rec = |rel: &str| c.rec(&root.join(rel));
    let mut plan_records = Vec::new();
    for (op, p) in &plans {
        plan_records.push((
            *op,
            obj(vec![
                ("operations", at(p, "operations")?.clone()),
                ("receipt", rec(&format!("frontend/eco-{op}/receipt.json"))?),
            ]),
        ));
    }
    let unverified = [
        "route",
        "extracted RC",
        "GDS",
        "DRC",
        "antenna",
        "LVS",
        "extracted_timing",
        "CEC",
        "benchmark",
    ];
    let result = obj(vec![
        ("schema", Json::str("rusteda.native-frontend-result.v1")),
        ("status", Json::str("frontend_completed")),
        (
            "scope",
            Json::str(
                "original RTL, dynamic native ECO, temporary CTS, two placement-estimated RC repair rounds, fresh final one CTS; placement RC estimate, not extraction",
            ),
        ),
        ("tools", at(&c.doc, "tools")?.clone()),
        ("original_inputs", at(&c.doc, "files")?.clone()),
        (
            "final_workspace",
            Json::Str(show(&root.join("frontend/road-final"))),
        ),
        ("final_netlist", rec("logs/fe_final_export.stdout")?),
        ("prects_netlist", rec("frontend/eco-hold/netlist.v")?),
        ("same_run_seed", rec("logs/fe_temporary_seed.stdout")?),
        (
            "pin_plan",
            obj(vec![
                ("physical", rec("frontend/physical-pins/physical.json")?),
                ("receipt", rec("frontend/physical-pins/receipt.json")?),
                ("side_counts", at(&pin_plan, "side_counts")?.clone()),
            ]),
        ),
        ("seed_source", source.clone()),
        (
            "final_estimate",
            obj(vec![
                ("result", rec("frontend/estimate-final/result.json")?),
                ("timing", obj(timing)),
            ]),
        ),
        ("plans", obj(plan_records)),
        (
            "unverified",
            Json::List(unverified.iter().map(|u| Json::str(*u)).collect()),
        ),
    ]);
    write_json(&out.join("result.json"), &result)?;
    c.digests.check_flow(out)
}

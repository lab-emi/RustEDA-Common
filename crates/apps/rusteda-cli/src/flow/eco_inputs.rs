// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The checked inputs of an ECO round (`native_ecos.py`: `Inputs`, `pin_views`,
//! `io_projection`, `bit_labels`): hashed file records, decoded documents, the provenance checks
//! of timing reports against their compiled STA revision, the merged pin view of the two
//! corners and the literal IO projection of the SDC.

use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap as HashMap;
use serde::Deserialize;

use super::context::{
    Digests, Failure, Identity, Loaded, Res, at, identity, is_sha256, key_error, require, show,
    text,
};
use super::py::{Bit, Dict};
use crate::estimate::{Json, space};

/// The budgets of the planner (the reference's command-line defaults).
#[derive(Debug, Clone)]
pub(crate) struct Budgets {
    pub(crate) memory_mib: i64,
    pub(crate) max_operations: i64,
    pub(crate) max_depth: i64,
    pub(crate) max_cells: i64,
    pub(crate) max_connections: i64,
    pub(crate) max_rows: i64,
    pub(crate) max_work: i64,
    pub(crate) max_combinations: i64,
    pub(crate) max_hold_chain: i64,
    pub(crate) max_total_hold_chain: i64,
    pub(crate) timeout_seconds: i64,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            memory_mib: 2048,
            max_operations: 4096,
            max_depth: 8,
            max_cells: 100_000,
            max_connections: 500_000,
            max_rows: 200_000,
            max_work: 10_000_000,
            max_combinations: 200_000,
            max_hold_chain: 4,
            max_total_hold_chain: 8,
            timeout_seconds: 180,
        }
    }
}

impl Budgets {
    /// The receipt's `budgets` object.
    pub(crate) fn to_json(&self) -> Json {
        Json::Object(
            [
                ("memory_mib", self.memory_mib),
                ("max_operations", self.max_operations),
                ("max_depth", self.max_depth),
                ("max_cells", self.max_cells),
                ("max_connections", self.max_connections),
                ("max_rows", self.max_rows),
                ("max_work", self.max_work),
                ("max_combinations", self.max_combinations),
                ("max_hold_chain", self.max_hold_chain),
                ("max_total_hold_chain", self.max_total_hold_chain),
                ("timeout_seconds", self.timeout_seconds),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), Json::Int(v.into())))
            .collect(),
        )
    }

    /// Whether every budget is positive and within the planner's caps.
    pub(crate) fn valid(&self) -> bool {
        [
            (self.memory_mib, 4096),
            (self.max_operations, 4096),
            (self.max_depth, 8),
            (self.max_cells, 100_000),
            (self.max_connections, 500_000),
            (self.max_rows, 200_000),
            (self.max_work, 100_000_000),
            (self.max_combinations, 200_000),
            (self.max_hold_chain, 8),
            (self.max_total_hold_chain, 8),
            (self.timeout_seconds, 600),
        ]
        .iter()
        .all(|&(v, cap)| 0 < v && v <= cap)
    }
}

/// `number(value)`: a finite float that is not negative (positive with `positive`).
pub(crate) fn number(value: f64, positive: bool) -> Res<f64> {
    require(
        value.is_finite() && if positive { value > 0.0 } else { value >= 0.0 },
        "INPUT",
        "finite nonnegative value required",
    )?;
    Ok(value)
}

/// `number(value)` of a JSON value: an `int` or `float` that is not a `bool`.
pub(crate) fn json_number(value: &Json, positive: bool) -> Res<f64> {
    let v = value
        .number()
        .ok_or_else(|| Failure::new("INPUT", "numeric value required"))?;
    number(v, positive)
}

/// `float(text)` of a decimal field.
fn field_float(t: &str) -> Res<f64> {
    let s = t.trim_matches(|c: char| c.is_ascii() && space(c as u8));
    s.parse::<f64>()
        .ok()
        .filter(|_| !s.is_empty())
        .ok_or_else(|| Failure::new("INPUT", format!("could not convert string to float: '{t}'")))
}

/// `handle(text)`: a new ECO name.
pub(crate) fn handle(text: &str) -> Res<&str> {
    let b = text.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 96
        && b[0].is_ascii_alphabetic()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_');
    require(ok, "HANDLE", "invalid new ECO handle")?;
    Ok(text)
}

// ---------------------------------------------------------------------------------------------
// Typed documents.

/// A connectivity graph (`rusteda export --kind connectivity-json`).
#[derive(Debug, Deserialize)]
pub(crate) struct GraphDoc {
    pub(crate) modules: Dict<Module>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Module {
    pub(crate) ports: Option<Dict<Definition>>,
    pub(crate) cells: Option<Dict<CellDef>>,
    pub(crate) netnames: Option<Dict<Definition>>,
}

/// A port or net name: its bits and the bus fields that label them.
#[derive(Debug, Deserialize)]
pub(crate) struct Definition {
    pub(crate) direction: Option<String>,
    pub(crate) bits: Vec<Bit>,
    pub(crate) offset: Option<Json>,
    pub(crate) upto: Option<Json>,
    pub(crate) is_bus: Option<Json>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CellDef {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) port_directions: Dict<String>,
    pub(crate) connections: Dict<Vec<Bit>>,
}

/// `bit_labels(name, definition)`: the label of every bit.
pub(crate) fn bit_labels<'d>(
    name: &str,
    definition: &'d Definition,
) -> Res<impl Iterator<Item = (String, Bit)> + 'd> {
    let count = definition.bits.len();
    let offset = match &definition.offset {
        None => 0,
        Some(v) => v
            .as_int()
            .ok_or_else(|| Failure::new("INPUT", "integer bus offset required"))?,
    };
    let upto = definition.upto.as_ref().is_some_and(Json::truthy);
    let bus = match &definition.is_bus {
        Some(v) => v.truthy(),
        None => count > 1 || offset != 0 || upto,
    };
    let name = name.to_string();
    Ok(definition
        .bits
        .iter()
        .enumerate()
        .map(move |(index, &bit)| {
            let position = offset
                + if upto {
                    (count - 1 - index) as i64
                } else {
                    index as i64
                };
            let label = if bus {
                format!("{name}[{position}]")
            } else {
                name.clone()
            };
            (label, bit)
        }))
}

/// A `ruststa report` envelope with the sections that the planner reads.
#[derive(Debug, Deserialize)]
pub(crate) struct Report {
    #[serde(default = "null")]
    pub(crate) schema: Json,
    #[serde(default = "null")]
    pub(crate) ok: Json,
    #[serde(default = "null")]
    pub(crate) rev: Json,
    pub(crate) workspace: Option<String>,
    pub(crate) scope: Option<Json>,
    pub(crate) completeness: Option<Json>,
    pub(crate) metrics: Option<Json>,
    pub(crate) sections: Option<Sections>,
}

fn null() -> Json {
    Json::Null
}

#[derive(Debug, Deserialize)]
pub(crate) struct Sections {
    pub(crate) drv: Option<RowSection<DrvRow>>,
    pub(crate) endpoints: Option<RowSection<EndpointRow>>,
    pub(crate) hold_fix: Option<HoldFix>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RowSection<R> {
    pub(crate) shown: Option<i64>,
    pub(crate) matched: Option<i64>,
    pub(crate) total: Option<i64>,
    pub(crate) truncated: Option<i64>,
    pub(crate) unevaluated: Option<i64>,
    pub(crate) rows: Vec<R>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum DrvKind {
    #[serde(rename = "max_transition")]
    Transition,
    #[serde(rename = "max_capacitance")]
    Capacitance,
    #[serde(rename = "max_fanout")]
    Fanout,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DrvRow {
    pub(crate) pin: String,
    pub(crate) kind: DrvKind,
    pub(crate) net: String,
    pub(crate) slack: Option<f64>,
    pub(crate) value: Option<f64>,
    pub(crate) limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EndpointRow {
    pub(crate) endpoint: String,
    pub(crate) kind: String,
    pub(crate) setup_slack_ns: Option<f64>,
    pub(crate) hold_slack_ns: Option<f64>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct HoldFix {
    pub(crate) target_margin_ns: f64,
    pub(crate) merged_total: i64,
    pub(crate) merged: Vec<MergedRow>,
    pub(crate) corners: Vec<HoldCorner>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct MergedRow {
    pub(crate) endpoint: String,
    pub(crate) hold_slack_by_corner_ns: Dict<Option<f64>>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct HoldCorner {
    pub(crate) corner: String,
    pub(crate) listed: i64,
    pub(crate) shown: i64,
    pub(crate) endpoints: Vec<HoldEndpoint>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct HoldEndpoint {
    pub(crate) endpoint: String,
    pub(crate) cell: Option<String>,
    pub(crate) net: Option<String>,
    pub(crate) net_driver: Option<String>,
    pub(crate) hold_slack_ns: f64,
    pub(crate) startpoint: String,
    pub(crate) path: Option<HoldPath>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct HoldPath {
    #[serde(default)]
    pub(crate) stages: Vec<Stage>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub(crate) struct Stage {
    pub(crate) pin: Option<String>,
    pub(crate) arc: Option<String>,
    pub(crate) net: Option<String>,
    pub(crate) incr_ns: Option<f64>,
}

impl HoldEndpoint {
    /// `detail.get('path', {}).get('stages', [])`.
    pub(crate) fn stages(&self) -> &[Stage] {
        self.path.as_ref().map_or(&[], |p| &p.stages)
    }
}

/// The kinds of decoded documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Json,
    Graph,
    Report,
    /// Read and hashed, not decoded (LEF, DEF).
    Bytes,
    /// Hashed only.
    Hash,
}

/// The documents this process decoded, by resolved path and SHA-256 of the decoded bytes.
#[derive(Debug, Default)]
pub(crate) struct Docs {
    json: Mutex<HashMap<String, (String, Arc<Json>)>>,
    graphs: Mutex<HashMap<String, (String, Arc<GraphDoc>)>>,
    reports: Mutex<HashMap<String, (String, Arc<Report>)>>,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8], path: &Path) -> Res<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| Failure::new("INPUT", format!("invalid JSON in {}: {e}", show(path))))
}

fn cached<T>(
    map: &Mutex<HashMap<String, (String, Arc<T>)>>,
    digests: &Digests,
    path: &Path,
) -> Res<(String, Arc<T>)>
where
    T: for<'de> Deserialize<'de>,
{
    let loaded = digests.load(path)?;
    let name = show(path);
    if let Some(hit) = map
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&name)
    {
        if hit.0 == loaded.sha256 {
            return Ok(hit.clone());
        }
    }
    let value = Arc::new(decode::<T>(&loaded.bytes, path)?);
    let entry = (loaded.sha256.clone(), value);
    map.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name, entry.clone());
    Ok(entry)
}

impl Docs {
    /// Reads, hashes and decodes the documents beside each other (at most four at a time): the
    /// calls that follow find them decoded. A document that cannot be decoded here is left to
    /// those calls, which report it.
    pub(crate) fn preload(&self, digests: &Digests, items: &[(PathBuf, Kind)]) {
        let mut unique: Vec<&(PathBuf, Kind)> = Vec::new();
        for item in items {
            if !unique.iter().any(|u| u.0 == item.0) {
                unique.push(item);
            }
        }
        let next = Mutex::new(unique.into_iter());
        let workers = super::context::workers(items.len());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let item = next.lock().unwrap_or_else(PoisonError::into_inner).next();
                        let Some((path, kind)) = item else { break };
                        match kind {
                            Kind::Json => drop(cached(&self.json, digests, path)),
                            Kind::Graph => drop(cached(&self.graphs, digests, path)),
                            Kind::Report => drop(cached(&self.reports, digests, path)),
                            Kind::Bytes => drop(digests.load(path)),
                            Kind::Hash => drop(digests.digest(path)),
                        }
                    }
                });
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs.

/// The planner's work counter (`Inputs.work` and `tick`).
#[derive(Debug)]
pub(crate) struct Work {
    units: Cell<i64>,
    max: i64,
}

impl Work {
    pub(crate) fn tick(&self, count: i64) -> Res<()> {
        self.units.set(self.units.get() + count);
        require(
            self.units.get() <= self.max,
            "BUDGET",
            "planner work budget exceeded",
        )
    }
    pub(crate) fn units(&self) -> i64 {
        self.units.get()
    }
}

/// One merged or per-corner pin model of a pin table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PinModel {
    pub(crate) cap: Option<f64>,
    pub(crate) limit: Option<f64>,
    pub(crate) fanout: f64,
    pub(crate) clock: bool,
    /// `Input`, `Output`, `Inout` or `Internal`.
    pub(crate) direction: Arc<str>,
    /// Infinite when no corner limits the fanout.
    pub(crate) max_fanout: f64,
}

#[derive(Debug)]
struct RawPin {
    cap: Option<f64>,
    limit: Option<f64>,
    fanout: f64,
    clock: bool,
    max_fanout: Option<f64>,
    direction: Arc<str>,
}

/// `pin_views(inputs, corners)`: the merged view of a `(cell, pin)` over the corners.
#[derive(Debug)]
pub(crate) struct Pins {
    views: Vec<HashMap<String, RawPin>>,
    memo: RefCell<HashMap<String, PinModel>>,
}

fn pin_key(cell: &str, pin: &str) -> String {
    format!("{cell}\t{pin}")
}

impl Pins {
    pub(crate) fn get(&self, cell: &str, pin: &str) -> Res<PinModel> {
        let key = pin_key(cell, pin);
        if let Some(hit) = self.memo.borrow().get(&key) {
            return Ok(hit.clone());
        }
        let mut values = Vec::with_capacity(self.views.len());
        for view in &self.views {
            match view.get(&key) {
                Some(v) => values.push(v),
                None => {
                    return Err(Failure::new(
                        "PIN_TABLE",
                        format!("pin absent in a corner: {cell}/{pin}"),
                    ));
                }
            }
        }
        require(
            values.iter().all(|v| v.direction == values[0].direction),
            "PIN_TABLE",
            "mixed pin directions",
        )?;
        // max() and min() keep the first of equal values, as the reference's do.
        let first_max = |a: f64, b: f64| if b > a { b } else { a };
        let first_min = |a: f64, b: f64| if b < a { b } else { a };
        let cap = values
            .iter()
            .map(|v| v.cap)
            .collect::<Option<Vec<f64>>>()
            .and_then(|caps| caps.into_iter().reduce(first_max));
        let model = PinModel {
            cap,
            limit: values.iter().filter_map(|v| v.limit).reduce(first_min),
            fanout: values
                .iter()
                .map(|v| v.fanout)
                .reduce(first_max)
                .unwrap_or(0.0),
            clock: values.iter().any(|v| v.clock),
            direction: values[0].direction.clone(),
            max_fanout: values
                .iter()
                .filter_map(|v| v.max_fanout)
                .reduce(first_min)
                .unwrap_or(f64::INFINITY),
        };
        self.memo.borrow_mut().insert(key, model.clone());
        Ok(model)
    }
}

/// The literal IO projection of the SDC (`io.json`).
#[derive(Debug)]
pub(crate) struct Io {
    /// Input port -> (driving cell, pin).
    pub(crate) drivers: HashMap<String, (String, String)>,
    /// Output port -> load (pF).
    pub(crate) loads_pf: HashMap<String, f64>,
}

/// A cursor for the fixed line patterns of the reference.
pub(crate) struct Scan<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(s: &'a str) -> Scan<'a> {
        Scan { s, i: 0 }
    }
    pub(crate) fn lit(&mut self, l: &str) -> bool {
        if self.s[self.i..].starts_with(l) {
            self.i += l.len();
            true
        } else {
            false
        }
    }
    /// One or more characters that satisfy `ok`.
    pub(crate) fn run(&mut self, ok: impl Fn(char) -> bool) -> Option<&'a str> {
        let rest = &self.s[self.i..];
        let n = rest.find(|c: char| !ok(c)).unwrap_or(rest.len());
        if n == 0 {
            return None;
        }
        self.i += n;
        Some(&rest[..n])
    }
    pub(crate) fn digits(&mut self) -> Option<&'a str> {
        self.run(|c| c.is_ascii_digit())
    }
    pub(crate) fn end(&self) -> bool {
        self.i == self.s.len()
    }
    pub(crate) fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }
}

fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn sdc_number(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')
}

fn brace_free(c: char) -> bool {
    c != '{' && c != '}'
}

/// `str.splitlines()` after universal newlines, each line stripped.
pub(crate) fn stripped_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split([
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ])
    .map(|l| l.trim_matches(py_space))
}

/// `set_driving_cell -lib_cell C -pin {P} -input_transition_rise R -input_transition_fall F
/// [get_ports {PORT}]`: (cell, pin, rise, fall, port).
pub(crate) fn driving_cell(line: &str) -> Option<(&str, &str, &str, &str, &str)> {
    let mut c = Scan::new(line);
    if !c.lit("set_driving_cell -lib_cell ") {
        return None;
    }
    let cell = c.run(|ch| !py_space(ch))?;
    if !c.lit(" -pin {") {
        return None;
    }
    let pin = c.run(brace_free)?;
    if !c.lit("} -input_transition_rise ") {
        return None;
    }
    let rise = c.run(sdc_number)?;
    if !c.lit(" -input_transition_fall ") {
        return None;
    }
    let fall = c.run(sdc_number)?;
    if !c.lit(" [get_ports {") {
        return None;
    }
    let port = c.run(brace_free)?;
    (c.lit("}]") && c.end()).then_some((cell, pin, rise, fall, port))
}

/// `set_load [-pin_load ]N [get_ports {PORTS}]`: (value, ports); `pin_load` says whether the
/// option is required.
pub(crate) fn set_load(line: &str, pin_load: bool) -> Option<(&str, &str)> {
    let mut c = Scan::new(line);
    if !c.lit("set_load ") {
        return None;
    }
    if !c.lit("-pin_load ") && pin_load {
        return None;
    }
    let value = c.run(sdc_number)?;
    if !c.lit(" [get_ports {") {
        return None;
    }
    let ports = c.run(brace_free)?;
    (c.lit("}]") && c.end()).then_some((value, ports))
}

/// The inputs of one planner run: every file record is hashed once and listed in the order of
/// its first use (the receipt's `inputs`).
pub(crate) struct Inputs<'a> {
    pub(crate) digests: &'a Digests,
    pub(crate) docs: &'a Docs,
    pub(crate) args: Budgets,
    /// The directory of `inputs.json`: relative record paths start here.
    pub(crate) base: PathBuf,
    pub(crate) manifest: Json,
    files: RefCell<Vec<(String, String)>>,
    index: RefCell<HashMap<String, usize>>,
    pub(crate) work: Work,
    report_identity: RefCell<Option<(String, String)>>,
    xxh3_memo: &'a RefCell<HashMap<String, (Identity, String)>>,
}

impl std::fmt::Debug for Inputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs").field("base", &self.base).finish()
    }
}

/// `Path(p).resolve()`: the canonical path of an existing file, else the path as written.
fn resolve_lenient(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl<'a> Inputs<'a> {
    pub(crate) fn new(
        path: &Path,
        args: Budgets,
        digests: &'a Digests,
        docs: &'a Docs,
        xxh3_memo: &'a RefCell<HashMap<String, (Identity, String)>>,
    ) -> Res<Inputs<'a>> {
        let resolved = resolve_lenient(path);
        let base = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
        let manifest = read_json(path, 2 << 20)?;
        require(
            manifest.get("schema").and_then(Json::as_str) == Some("rusteda.native-eco-inputs.v1"),
            "SCHEMA",
            "input bundle schema",
        )?;
        let max_work = args.max_work;
        let inputs = Inputs {
            digests,
            docs,
            args,
            base,
            manifest,
            files: RefCell::default(),
            index: RefCell::default(),
            work: Work {
                units: Cell::new(0),
                max: max_work,
            },
            report_identity: RefCell::new(None),
            xxh3_memo,
        };
        let sha = inputs.sha(&resolved)?;
        inputs.set_file(show(&resolved), sha);
        Ok(inputs)
    }

    pub(crate) fn tick(&self, count: i64) -> Res<()> {
        self.work.tick(count)
    }

    /// `sha(path)`: the memoized SHA-256 of the resolved path.
    pub(crate) fn sha(&self, path: &Path) -> Res<String> {
        self.digests.digest(&resolve_lenient(path))
    }

    /// `self.files[key] = sha`: a new path joins the end of the list.
    pub(crate) fn set_file(&self, key: String, sha: String) {
        let mut index = self.index.borrow_mut();
        let mut files = self.files.borrow_mut();
        match index.get(&key) {
            Some(&i) => files[i].1 = sha,
            None => {
                index.insert(key.clone(), files.len());
                files.push((key, sha));
            }
        }
    }

    /// The files in the order of their first use.
    pub(crate) fn files(&self) -> Vec<(String, String)> {
        self.files.borrow().clone()
    }

    /// `file(record, cap)`: the resolved path of a file record whose SHA-256 holds.
    pub(crate) fn file(&self, record: &Json, cap: u64) -> Res<PathBuf> {
        let path = record.get("path").and_then(Json::as_str);
        require(
            record.as_object().is_some() && path.is_some(),
            "INPUT",
            "file record required",
        )?;
        let path = path.unwrap_or_default();
        let sha = record.get("sha256").and_then(Json::as_str).unwrap_or("");
        require(
            path.len() <= 4096 && is_sha256(sha),
            "INPUT",
            "path or SHA256 invalid",
        )?;
        let path = resolve_lenient(&self.base.join(path));
        let ok = fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= cap);
        require(
            ok,
            "BUDGET",
            format!("missing or oversized input: {}", show(&path)),
        )?;
        let key = show(&path);
        let known = self.index.borrow().get(&key).copied();
        match known {
            None => {
                require(
                    self.files.borrow().len() < 96,
                    "BUDGET",
                    "input file-count budget",
                )?;
                require(
                    self.digests.digest(&path)? == sha,
                    "STALE_INPUT",
                    format!("SHA256 mismatch: {key}"),
                )?;
                self.set_file(key, sha.to_string());
            }
            Some(i) => require(
                self.files.borrow()[i].1 == sha,
                "STALE_INPUT",
                "inconsistent file records",
            )?,
        }
        Ok(path)
    }

    /// `file(record)` with the default 64 MiB budget.
    pub(crate) fn file64(&self, record: &Json) -> Res<PathBuf> {
        self.file(record, 64 << 20)
    }

    /// `file(record, cap)` with the bytes of the file: they are read and hashed once (a
    /// preloaded file is taken as it is), and the record has to name exactly these bytes.
    pub(crate) fn loaded(&self, record: &Json, cap: u64) -> Res<(PathBuf, Arc<Loaded>)> {
        if let Some(p) = record.get("path").and_then(Json::as_str) {
            let path = resolve_lenient(&self.base.join(p));
            if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= cap) {
                let _ = self.digests.load(&path);
            }
        }
        let path = self.file(record, cap)?;
        let loaded = self.digests.load(&path)?;
        require(
            Some(loaded.sha256.as_str()) == record.get("sha256").and_then(Json::as_str),
            "STALE_INPUT",
            format!("SHA256 mismatch: {}", show(&path)),
        )?;
        Ok((path, loaded))
    }

    fn checked<T>(
        &self,
        record: &Json,
        cap: u64,
        map: &Mutex<HashMap<String, (String, Arc<T>)>>,
    ) -> Res<Arc<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let (path, loaded) = self.loaded(record, cap)?;
        let (sha, value) = cached(map, self.digests, &path)?;
        require(
            sha == loaded.sha256,
            "STALE_INPUT",
            format!("SHA256 mismatch: {}", show(&path)),
        )?;
        Ok(value)
    }

    /// `document(record, cap)`: the decoded JSON document of a file record.
    pub(crate) fn document(&self, record: &Json, cap: u64) -> Res<Arc<Json>> {
        self.checked(record, cap, &self.docs.json)
    }

    pub(crate) fn graph(&self, record: &Json) -> Res<Arc<GraphDoc>> {
        self.checked(record, 64 << 20, &self.docs.graphs)
    }

    /// `tool(name)`: a pinned tool; `rustroad` and `xxhsum` must be ELF executables.
    pub(crate) fn tool(&self, name: &str) -> Res<PathBuf> {
        let path = self.file(at(at(&self.manifest, "tools")?, name)?, 256 << 20)?;
        if name == "rustroad" || name == "xxhsum" {
            let mut magic = [0u8; 4];
            let ok = File::open(&path)
                .and_then(|mut f| f.read_exact(&mut magic))
                .is_ok();
            require(
                ok && magic == *b"\x7fELF",
                "NATIVE_CLI",
                "direct native ELF required for bounded cleanup",
            )?;
        }
        Ok(path)
    }

    /// `command(argv, directory, label)`: runs a pinned tool with its output retained.
    pub(crate) fn command(&self, argv: &[String], directory: &Path, label: &str) -> Res<()> {
        self.tick(1)?;
        let list = Json::List(argv.iter().map(|a| Json::Str(a.clone())).collect());
        fs::write(
            directory.join(format!("{label}-command.json")),
            list.dumps(Some(2)) + "\n",
        )?;
        let out = File::create(directory.join(format!("{label}.stdout")))?;
        let err = File::create(directory.join(format!("{label}.stderr")))?;
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .env("RAYON_NUM_THREADS", "2")
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(self.args.timeout_seconds as u64);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::new(
                    "NATIVE_CLI",
                    format!(
                        "{label} timed out after {} seconds",
                        self.args.timeout_seconds
                    ),
                ));
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        let code = status
            .code()
            .map_or_else(|| "a signal".to_string(), |c| c.to_string());
        require(
            status.success(),
            "NATIVE_CLI",
            format!("{label} exited {code}; see retained stderr"),
        )
    }

    /// `xxh3(path)`: the XXH3-64 of a file by the pinned `xxhsum`, once per file version.
    pub(crate) fn xxh3(&self, path: &Path) -> Res<String> {
        let meta = fs::metadata(path)?;
        let name = show(&resolve_lenient(path));
        let key = identity(&meta);
        if let Some(hit) = self.xxh3_memo.borrow().get(&name) {
            if hit.0 == key {
                // The pinned tool is still checked and listed, as when it runs.
                self.tool("xxhsum")?;
                return Ok(hit.1.clone());
            }
        }
        let tool = self.tool("xxhsum")?;
        let out = Command::new(&tool)
            .arg("-H3")
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        require(
            out.status.success(),
            "HASH_TOOL",
            "XXH3-64 executable failed",
        )?;
        let data = String::from_utf8_lossy(&out.stdout);
        // `XXH3_<16 hex>`, blanks, the file name and at most one newline.
        let parsed = || -> Option<String> {
            let rest = data.strip_prefix("XXH3_")?;
            let hash = rest.get(..16)?;
            if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let tail = &rest[16..];
            let name = tail.trim_start_matches([' ', '\t']);
            let name = name.strip_suffix('\n').unwrap_or(name);
            (name.len() < tail.len() && !name.is_empty() && !name.contains('\n'))
                .then(|| format!("xxh3:{}", hash.to_ascii_lowercase()))
        };
        let Some(hash) = parsed() else {
            return Err(Failure::new(
                "HASH_TOOL",
                "unexpected XXH3-64 executable output",
            ));
        };
        if identity(&fs::metadata(path)?) == key {
            self.xxh3_memo
                .borrow_mut()
                .insert(name, (key, hash.clone()));
        }
        Ok(hash)
    }

    /// The corner record of the manifest.
    pub(crate) fn corner(&self, corner: &str) -> Res<&Json> {
        at(at(&self.manifest, "corners")?, corner)
    }

    /// `report(corner, kind, timed, sdc)`: a complete report envelope of the corner, bound to
    /// one compiled STA revision whose sources are the current netlist, constraints, pin
    /// libraries and parasitics.
    pub(crate) fn report(
        &self,
        corner: &str,
        kind: &str,
        timed: &Path,
        sdc: &Path,
    ) -> Res<Arc<Report>> {
        let env = self.checked(
            at(self.corner(corner)?, kind)?,
            64 << 20,
            &self.docs.reports,
        )?;
        require(
            env.schema.as_str() == Some("rusteda.v1") && env.ok == Json::Bool(true),
            "REPORT",
            "successful native envelope required",
        )?;
        let rev = env.rev.as_str().unwrap_or("");
        let digits = rev.strip_prefix("rev").unwrap_or("");
        require(
            (1..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()),
            "REPORT",
            "immutable revision required",
        )?;
        let scope = env.scope.as_ref().ok_or_else(|| key_error("scope"))?;
        let corners = at(scope, "corners")?;
        let has_corner = match corners {
            Json::List(items) => items.iter().any(|c| c.as_str() == Some(corner)),
            Json::Str(s) => s.contains(corner),
            Json::Object(items) => items.iter().any(|(k, _)| k == corner),
            _ => false,
        };
        require(
            has_corner && at(scope, "cppr")?.as_str() == Some("off"),
            "REPORT_SCOPE",
            "missing corner or inconsistent CPPR",
        )?;
        require(
            at(scope, "clock_model")?.as_str() == Some("propagated"),
            "REPORT_SCOPE",
            "propagated clock timing required",
        )?;
        let workspace = env
            .workspace
            .as_deref()
            .ok_or_else(|| key_error("workspace"))?;
        let folder = resolve_lenient(Path::new(workspace));
        let identity = (show(&folder), rev.to_string());
        {
            let mut known = self.report_identity.borrow_mut();
            require(
                known.as_ref().is_none_or(|k| *k == identity),
                "STALE_REPORT",
                "reports must share one STA workspace revision",
            )?;
            *known = Some(identity);
        }
        let lock = File::open(folder.join("workspace.lock"))?;
        lock.try_lock_shared()
            .map_err(|_| Failure::new("IO", "[Errno 11] Resource temporarily unavailable"))?;
        let number: u64 = digits.parse().unwrap_or(0);
        let meta_path = folder
            .join("revisions")
            .join(format!("rev{number:04}"))
            .join("meta.json");
        let meta = read_json(&meta_path, 4 << 20)?;
        let sources = at(&meta, "sources")?.as_list().unwrap_or(&[]);
        for (role, path) in [("netlist", timed), ("constraints", sdc)] {
            let mut found = Vec::new();
            for x in sources {
                if at(x, "table")?.as_str() == Some(role) {
                    found.push(x);
                }
            }
            require(
                found.len() == 1,
                "REPORT_PROVENANCE",
                format!("exactly one source required: {role}"),
            )?;
            let same = resolve_lenient(Path::new(text(found[0], "path")?)) == path
                && at(found[0], "hash")?.as_str() == Some(self.xxh3(path)?.as_str());
            require(
                same,
                "STALE_REPORT",
                format!("compiled source does not match current {role}"),
            )?;
        }
        let libraries = at(&meta, "libraries")?.as_list().unwrap_or(&[]);
        let tables = at(self.corner(corner)?, "pin_tables")?
            .as_list()
            .unwrap_or(&[]);
        for record in tables {
            let library = at(record, "library")?;
            let source = self.file64(library)?;
            let mut found = Vec::new();
            for x in libraries {
                if at(x, "corner")?.as_str() == Some(corner)
                    && resolve_lenient(Path::new(text(x, "source")?)) == source
                {
                    found.push(x);
                }
            }
            require(
                found.len() == 1,
                "REPORT_PROVENANCE",
                "pin library missing from compiled corner",
            )?;
            let cached = folder.join(Path::new(text(found[0], "compiled")?).with_extension("lib"));
            let sha = text(library, "sha256")?;
            require(
                cached.is_file() && self.sha(&cached)? == sha,
                "STALE_REPORT",
                "compiled raw Liberty cache differs from current pin-library source",
            )?;
            self.set_file(show(&cached), sha.to_string());
        }
        if at(scope, "wire_model")?.as_str() == Some("spef_rc") {
            let mut expected: Vec<PathBuf> = Vec::new();
            for (_, value) in at(&self.manifest, "corners")?.as_object().unwrap_or(&[]) {
                let p = self.file64(at(value, "spef")?)?;
                if !expected.contains(&p) {
                    expected.push(p);
                }
            }
            let mut annotated: Vec<PathBuf> = Vec::new();
            for x in sources {
                if at(x, "table")?.as_str() == Some("parasitics") {
                    let p = resolve_lenient(Path::new(text(x, "path")?));
                    if !annotated.contains(&p) {
                        annotated.push(p);
                    }
                }
            }
            require(
                annotated.len() == expected.len() && annotated.iter().all(|p| expected.contains(p)),
                "REPORT_PROVENANCE",
                "explicit SPEF sources differ from compiled parasitics",
            )?;
            for item in sources {
                if at(item, "table")?.as_str() == Some("parasitics") {
                    let p = resolve_lenient(Path::new(text(item, "path")?));
                    require(
                        at(item, "hash")?.as_str() == Some(self.xxh3(&p)?.as_str()),
                        "STALE_REPORT",
                        "SPEF changed after compilation",
                    )?;
                }
            }
        }
        let sha = self.sha(&meta_path)?;
        self.set_file(show(&meta_path), sha);
        drop(lock);
        let sections = env.sections.as_ref().ok_or_else(|| key_error("sections"))?;
        let max_rows = self.args.max_rows;
        match kind {
            "hold_fix" => {
                let section = sections.hold_fix.as_ref().ok_or_else(|| key_error(kind))?;
                require(
                    section.merged_total == section.merged.len() as i64
                        && section.merged.len() as i64 <= max_rows,
                    "REPORT_TRUNCATED",
                    "merged hold rows incomplete",
                )?;
                require(
                    section
                        .corners
                        .iter()
                        .all(|x| x.listed == x.shown && x.shown == x.endpoints.len() as i64),
                    "REPORT_TRUNCATED",
                    "corner hold rows incomplete",
                )?;
            }
            "drv" => {
                let section = sections.drv.as_ref().ok_or_else(|| key_error(kind))?;
                complete(section, kind, max_rows)?;
                require(
                    section
                        .rows
                        .iter()
                        .all(|r| r.slack.is_some_and(f64::is_finite)),
                    "REPORT",
                    "nonfinite DRV slack",
                )?;
                let metrics = env.metrics.as_ref().ok_or_else(|| key_error("metrics"))?;
                for (check, metric) in [
                    (DrvKind::Transition, "max_transition_violating"),
                    (DrvKind::Capacitance, "max_capacitance_violating"),
                    (DrvKind::Fanout, "max_fanout_violating"),
                ] {
                    let found = section
                        .rows
                        .iter()
                        .filter(|r| r.kind == check && r.slack.is_some_and(|s| s < 0.0))
                        .count();
                    require(
                        at(metrics, metric)?.number() == Some(found as f64),
                        "REPORT_TRUNCATED",
                        format!(
                            "DRV rows omit globally counted {} violations",
                            metric.trim_end_matches("_violating")
                        ),
                    )?;
                }
            }
            _ => {
                let section = sections.endpoints.as_ref().ok_or_else(|| key_error(kind))?;
                complete(section, kind, max_rows)?;
                require(
                    section.matched == section.total,
                    "REPORT_TRUNCATED",
                    "all endpoints required for setup windows",
                )?;
            }
        }
        Ok(env)
    }
}

fn complete<R>(section: &RowSection<R>, kind: &str, max_rows: i64) -> Res<()> {
    let rows = section.rows.len() as i64;
    require(
        section.shown.is_some()
            && section.shown == section.matched
            && section.matched == Some(rows)
            && rows <= max_rows,
        "REPORT_TRUNCATED",
        format!("{kind} rows incomplete"),
    )?;
    require(
        section.truncated.unwrap_or(0) == 0 && section.unevaluated.unwrap_or(0) == 0,
        "REPORT_TRUNCATED",
        "missing native checks",
    )
}

/// `read_json(path, cap)` of the planner: the budget failure carries `BUDGET`.
pub(crate) fn read_json(path: &Path, cap: u64) -> Res<Json> {
    let size = fs::metadata(path)
        .map_err(|e| Failure::new("IO", format!("{e}: '{}'", show(path))))?
        .len();
    require(
        size <= cap,
        "BUDGET",
        format!("JSON byte budget: {}", show(path)),
    )?;
    let bytes = fs::read(path)?;
    decode(&bytes, path)
}

/// `pin_views(inputs, corners)`: reads the pin tables of both corners.
pub(crate) fn pin_views(inputs: &Inputs<'_>, corners: &[String]) -> Res<Pins> {
    let mut views = Vec::new();
    for corner in corners {
        let mut view: HashMap<String, RawPin> = HashMap::default();
        let tables = at(inputs.corner(corner)?, "pin_tables")?
            .as_list()
            .unwrap_or(&[]);
        for record in tables {
            inputs.file64(at(record, "library")?)?;
            let path = inputs.file(record, 16 << 20)?;
            let table = crate::estimate::read_text(&path)?;
            require(
                !table.contains('"'),
                "PIN_TABLE",
                "quoted pin-table fields are not supported",
            )?;
            for line in table.lines() {
                inputs.tick(1)?;
                let row: Vec<&str> = if line.is_empty() {
                    Vec::new()
                } else {
                    line.split('\t').collect()
                };
                require(
                    row.len() == 8,
                    "PIN_TABLE",
                    "canonical eight-column pin TSV required",
                )?;
                let (cell, pin, direction) = (row[0], row[1], row[2]);
                let key = pin_key(cell, pin);
                require(
                    !view.contains_key(&key)
                        && matches!(direction, "Input" | "Output" | "Inout" | "Internal"),
                    "PIN_TABLE",
                    "duplicate or invalid pin",
                )?;
                let optional = |x: &str| -> Res<Option<f64>> {
                    if x == "none" {
                        Ok(None)
                    } else {
                        Ok(Some(number(field_float(x)?, false)?))
                    }
                };
                require(
                    matches!(row[6], "true" | "false"),
                    "PIN_TABLE",
                    "clock flag must be explicit",
                )?;
                let raw = RawPin {
                    cap: optional(row[3])?,
                    limit: optional(row[4])?,
                    fanout: number(field_float(row[5])?, false)?,
                    clock: row[6] == "true",
                    max_fanout: optional(row[7])?,
                    direction: Arc::from(direction),
                };
                view.insert(key, raw);
                require(
                    view.len() as i64 <= inputs.args.max_connections,
                    "BUDGET",
                    "pin-table budget",
                )?;
            }
        }
        views.push(view);
    }
    Ok(Pins {
        views,
        memo: RefCell::default(),
    })
}

/// `io_projection(inputs, sdc)`: the IO projection, which must equal the literal IO commands of
/// the SDC.
pub(crate) fn io_projection(inputs: &Inputs<'_>, sdc: &Path) -> Res<Io> {
    let io = inputs.document(at(&inputs.manifest, "io")?, 1 << 20)?;
    require(
        at(&io, "sdc_sha256")?.as_str() == Some(inputs.sha(sdc)?.as_str()),
        "STALE_INPUT",
        "IO projection belongs to another SDC",
    )?;
    let mut drivers: HashMap<String, (String, String)> = HashMap::default();
    let mut loads: HashMap<String, f64> = HashMap::default();
    let unsupported = |ok: bool, text: &str| require(ok, "SDC_IO_UNSUPPORTED", text);
    let sdc_text = crate::estimate::read_text(sdc)?;
    for line in stripped_lines(&sdc_text) {
        unsupported(
            !line.starts_with("set_units"),
            "IO policy requires default ns/pF units",
        )?;
        if line.starts_with("set_driving_cell") {
            let m = driving_cell(line);
            unsupported(m.is_some(), "nonliteral driving-cell projection")?;
            let Some((cell, pin, rise, fall, port)) = m else {
                continue;
            };
            unsupported(
                field_float(rise)? == 0.0 && field_float(fall)? == 0.0,
                "nonzero virtual-driver input transition",
            )?;
            unsupported(!drivers.contains_key(port), "duplicate input driver")?;
            drivers.insert(port.to_string(), (cell.to_string(), pin.to_string()));
        } else if line.starts_with("set_load") {
            let m = set_load(line, false);
            unsupported(m.is_some(), "nonliteral output-load projection")?;
            let Some((value, ports)) = m else { continue };
            for port in ports.split(py_space).filter(|p| !p.is_empty()) {
                unsupported(!loads.contains_key(port), "duplicate output load")?;
                loads.insert(port.to_string(), number(field_float(value)?, false)?);
            }
        }
    }
    // The projection's dicts must equal the literal ones (keys and values).
    let mut same = true;
    let mut io_drivers: HashMap<String, (String, String)> = HashMap::default();
    for (port, d) in at(&io, "drivers")?.as_object().unwrap_or(&[]) {
        let (Some(cell), Some(pin)) = (
            d.get("cell").and_then(Json::as_str),
            d.get("pin").and_then(Json::as_str),
        ) else {
            same = false;
            continue;
        };
        same &= d.as_object().is_some_and(|items| items.len() == 2);
        io_drivers.insert(port.clone(), (cell.to_string(), pin.to_string()));
    }
    same &= at(&io, "drivers")?.as_object().is_some() && io_drivers == drivers;
    let mut io_loads: HashMap<String, f64> = HashMap::default();
    for (port, v) in at(&io, "loads_pf")?.as_object().unwrap_or(&[]) {
        match v.number() {
            Some(x) => {
                io_loads.insert(port.clone(), x);
            }
            None => same = false,
        }
    }
    same &= at(&io, "loads_pf")?.as_object().is_some() && io_loads == loads;
    require(
        same,
        "SDC_IO_MISMATCH",
        "IO projection differs from original literal SDC",
    )?;
    Ok(Io {
        drivers: io_drivers,
        loads_pf: io_loads,
    })
}

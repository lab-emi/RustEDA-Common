// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! `rusteda estimate-spef`: the placement-based RC estimate of the Ibex recipe's
//! `estimate_spef.py` (schema `rusteda.rc-estimate-policy.v1`), as a native command whose
//! files are that estimator's byte for byte (RCLI-REQ-0026).
//!
//! Each signal net of a placed DEF becomes a rectilinear minimum spanning tree over its pin
//! locations (cell placement plus the oriented median `RECT` centre of the LEF pin, or the DEF
//! port location), rooted at its driver (Prim's algorithm, R. C. Prim, "Shortest connection
//! networks and some generalizations", Bell System Technical Journal 36(6), 1957). With the
//! policy's `macro_blockage`, an edge every monotone path of which crosses a `CLASS BLOCK` macro
//! is measured around the macros by a breadth-first search over a raster of the die. Each tree
//! edge is stretched by the policy's detour factor and split into segments of at most
//! `max_segment_um`, each a pi section of `r_ohm_per_um` / `c_pf_per_um`.
//!
//! The command reads its inputs with the reference estimator's line rules and writes
//! `estimate.spef`, `result.json` and `HEAD` (`rev1`) into a fresh workspace directory (only
//! `workspace.lock`, held exclusively), with the reference's number formats: `%.5e` and `%.9e`
//! for SPEF values, the shortest round-trip representation of a float in JSON (`repr`), decimal
//! rounding of the report's totals (`round`), and compensated summation of the totals (A.
//! Neumaier, "Rundungsfehleranalyse einiger Verfahren zur Summation endlicher Summen", ZAMM
//! 54(1), 1974, the summation of the reference's `sum`). Standard output is the reference's
//! one-line summary, not a `rusteda.v1` envelope; a failure prints `{"status": "failed",
//! "error": ...}` on standard error and exits with code 2. Inputs must be ASCII.

use std::collections::VecDeque;

use rustc_hash::FxHashMap as HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

/// The command's arguments (`--workspace` is the global `-w/--workspace`).
#[derive(clap::Args, Debug)]
pub struct EstimateArgs {
    /// Placed DEF (components, pins and nets).
    #[arg(long = "def", value_name = "FILE")]
    pub def: std::path::PathBuf,
    /// Cell LEF with the macro sizes and pin rectangles.
    #[arg(long = "lef", value_name = "FILE")]
    pub lef: std::path::PathBuf,
    /// RC estimate policy (`rusteda.rc-estimate-policy.v1`).
    #[arg(long = "policy", value_name = "FILE")]
    pub policy: std::path::PathBuf,
    /// Base revision of the workspace; only `empty` (a fresh directory) is accepted.
    #[arg(long = "base-rev", value_name = "REV")]
    pub base_rev: String,
}

/// An estimate failure, reported as the reference reports it.
#[derive(Debug)]
struct Fail(String);

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail(e.to_string())
    }
}

fn fail<T>(text: impl Into<String>) -> Result<T, Fail> {
    Err(Fail(text.into()))
}

/// Runs `rusteda estimate-spef` for the workspace `ws`.
pub fn run(ws: &Path, args: &EstimateArgs) -> ExitCode {
    match estimate(ws, args) {
        Ok(summary) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(summary.as_bytes());
            let _ = out.flush();
            ExitCode::from(0)
        }
        Err(Fail(text)) => {
            let mut line = String::from("{\"status\": \"failed\", \"error\": ");
            json_string(&mut line, &text);
            line.push_str("}\n");
            let _ = std::io::stderr().write_all(line.as_bytes());
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Text rules of the reference: whitespace is `str.isspace` of ASCII, words are `\w` of ASCII.

fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x1c..=0x1f)
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The text of `path` as the reference reads it: UTF-8 (only ASCII is accepted here), with
/// universal newlines (`\r\n` and `\r` read as `\n`).
fn read_text(path: &Path) -> Result<String, Fail> {
    let bytes = fs::read(path)?;
    if !bytes.is_ascii() {
        return fail(format!("{}: non-ASCII input", path.display()));
    }
    let text = String::from_utf8(bytes).map_err(|e| Fail(e.to_string()))?;
    Ok(if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text
    })
}

/// The lines of `text` with their `\n`, as iterating a text file yields them.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
}

/// `s.split()`: the whitespace-separated tokens.
fn tokens(s: &str) -> Vec<&str> {
    s.as_bytes()
        .split(|&b| space(b))
        .filter(|t| !t.is_empty())
        .map(|t| std::str::from_utf8(t).unwrap_or_default())
        .collect()
}

/// `s.lstrip()`.
fn lstrip(s: &str) -> &str {
    let n = s.bytes().take_while(|&b| space(b)).count();
    &s[n..]
}

/// `float(token)` for the decimal tokens of LEF and DEF.
fn py_float(t: &str) -> Result<f64, Fail> {
    t.parse::<f64>()
        .map_err(|_| Fail(format!("could not convert string to float: '{t}'")))
}

/// `int(digits)` of a `-?\d+` capture.
fn py_int(t: &str) -> Result<i64, Fail> {
    t.parse::<i64>()
        .map_err(|_| Fail(format!("invalid literal for int(): '{t}'")))
}

/// A cursor over the bytes of one line for the reference's fixed patterns.
struct Cur<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn at(s: &'a str, i: usize) -> Self {
        Cur { s: s.as_bytes(), i }
    }
    fn text(&self, a: usize, b: usize) -> &'a str {
        std::str::from_utf8(&self.s[a..b]).unwrap_or_default()
    }
    fn lit(&mut self, l: &str) -> bool {
        if self.s[self.i..].starts_with(l.as_bytes()) {
            self.i += l.len();
            true
        } else {
            false
        }
    }
    fn spaces(&mut self) {
        while self.i < self.s.len() && space(self.s[self.i]) {
            self.i += 1;
        }
    }
    /// `(\S+)` followed by what the pattern needs next: the maximal run (the only one a
    /// following space or end allows).
    fn nonspace(&mut self) -> Option<&'a str> {
        let a = self.i;
        while self.i < self.s.len() && !space(self.s[self.i]) {
            self.i += 1;
        }
        (self.i > a).then(|| self.text(a, self.i))
    }
    /// `(-?\d+)` followed by a non-digit.
    fn int(&mut self) -> Option<&'a str> {
        let a = self.i;
        if self.i < self.s.len() && self.s[self.i] == b'-' {
            self.i += 1;
        }
        let d = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i == d {
            self.i = a;
            return None;
        }
        Some(self.text(a, self.i))
    }
    /// `(\w+)`.
    fn word(&mut self) -> Option<&'a str> {
        let a = self.i;
        while self.i < self.s.len() && word(self.s[self.i]) {
            self.i += 1;
        }
        (self.i > a).then(|| self.text(a, self.i))
    }
}

/// `\s*- (\S+) (\S+) \+ (?:PLACED|FIXED|COVER) \( (-?\d+) (-?\d+) \) (\S+)` at the start.
fn component(line: &str) -> Option<(&str, &str, &str, &str, &str)> {
    let mut c = Cur::at(line, 0);
    c.spaces();
    if !c.lit("- ") {
        return None;
    }
    let name = c.nonspace()?;
    if !c.lit(" ") {
        return None;
    }
    let master = c.nonspace()?;
    if !c.lit(" + ") {
        return None;
    }
    if !(c.lit("PLACED") || c.lit("FIXED") || c.lit("COVER")) || !c.lit(" ( ") {
        return None;
    }
    let x = c.int()?;
    if !c.lit(" ") {
        return None;
    }
    let y = c.int()?;
    if !c.lit(" ) ") {
        return None;
    }
    let orient = c.nonspace()?;
    Some((name, master, x, y, orient))
}

/// `\s*- (\S+)` at the start.
fn port_name(line: &str) -> Option<&str> {
    let mut c = Cur::at(line, 0);
    c.spaces();
    if !c.lit("- ") {
        return None;
    }
    c.nonspace()
}

/// The leftmost match of `lead` followed by `rest` (a search).
fn search<'a, T>(
    line: &'a str,
    lead: &[&str],
    mut rest: impl FnMut(&mut Cur<'a>) -> Option<T>,
) -> Option<T> {
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        for l in lead {
            if bytes[i..].starts_with(l.as_bytes()) {
                let mut c = Cur::at(line, i + l.len());
                if let Some(v) = rest(&mut c) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// `(-?\d+) (-?\d+) \)` after `\( `.
fn pair<'a>(c: &mut Cur<'a>) -> Option<(&'a str, &'a str)> {
    let x = c.int()?;
    if !c.lit(" ") {
        return None;
    }
    let y = c.int()?;
    c.lit(" )").then_some((x, y))
}

/// `DIRECTION (\w+)` (search).
fn port_direction(line: &str) -> Option<&str> {
    search(line, &["DIRECTION "], |c| c.word())
}

/// `LAYER \S+ \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)` (search).
fn port_layer(line: &str) -> Option<[&str; 4]> {
    search(line, &["LAYER "], |c| {
        c.nonspace()?;
        if !c.lit(" ( ") {
            return None;
        }
        let (a, b) = pair(c)?;
        if !c.lit(" ( ") {
            return None;
        }
        let (d, e) = pair(c)?;
        Some([a, b, d, e])
    })
}

/// `(?:PLACED|FIXED) \( (-?\d+) (-?\d+) \)` (search).
fn port_placed(line: &str) -> Option<(&str, &str)> {
    search(line, &["PLACED", "FIXED"], |c| {
        if !c.lit(" ( ") {
            return None;
        }
        pair(c)
    })
}

/// `findall(r'\( (\S+) (\S+) \)')`.
fn net_pins<'a>(text: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let mut c = Cur::at(text, i);
            c.i += 1;
            let found = (|| {
                if !c.lit(" ") {
                    return None;
                }
                let a = c.nonspace()?;
                if !c.lit(" ") {
                    return None;
                }
                let b = c.nonspace()?;
                c.lit(" )").then_some((a, b))
            })();
            if let Some((a, b)) = found {
                out.push((a, b));
                i = c.i;
                continue;
            }
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs.

/// MACRO -> (width um, height um, pins in definition order with their median rectangle centre
/// and direction), signal pins only.
type Macros = HashMap<String, (f64, f64, HashMap<String, (f64, f64, String)>)>;

fn read_lef(path: &Path) -> Result<Macros, Fail> {
    let text = read_text(path)?;
    let mut macros: Macros = HashMap::default();
    let (mut mac, mut pin): (Option<String>, Option<String>) = (None, None);
    let (mut direction, mut usage) = (String::from("INPUT"), String::from("SIGNAL"));
    let mut rects: Vec<(f64, f64)> = Vec::new();
    for raw in lines(&text) {
        let t = tokens(raw);
        if t.is_empty() {
            continue;
        }
        if t[0] == "MACRO" {
            let name = t
                .get(1)
                .ok_or_else(|| Fail("list index out of range".into()))?;
            macros.insert(name.to_string(), (0.0, 0.0, HashMap::default()));
            mac = Some(name.to_string());
            continue;
        }
        let Some(m) = mac.clone() else { continue };
        let index = || Fail("list index out of range".into());
        if t[0] == "SIZE" && pin.is_none() {
            let (w, h) = (
                py_float(t.get(1).ok_or_else(index)?)?,
                py_float(t.get(3).ok_or_else(index)?)?,
            );
            let info = macros.get_mut(&m).ok_or_else(index)?;
            (info.0, info.1) = (w, h);
        } else if t[0] == "PIN" {
            pin = Some(t.get(1).ok_or_else(index)?.to_string());
            direction = "INPUT".into();
            usage = "SIGNAL".into();
            rects = Vec::new();
        } else if pin.is_some() && t[0] == "DIRECTION" {
            direction = t.get(1).ok_or_else(index)?.trim_end_matches(';').into();
        } else if pin.is_some() && t[0] == "USE" {
            usage = t.get(1).ok_or_else(index)?.trim_end_matches(';').into();
        } else if pin.is_some() && t[0] == "RECT" {
            if t.len() < 5 {
                return Err(index());
            }
            let v = [
                py_float(t[1])?,
                py_float(t[2])?,
                py_float(t[3])?,
                py_float(t[4])?,
            ];
            rects.push(((v[0] + v[2]) / 2.0, (v[1] + v[3]) / 2.0));
        } else if pin.is_some() && t[0] == "END" && t.len() > 1 && Some(t[1]) == pin.as_deref() {
            if usage != "POWER" && usage != "GROUND" && !rects.is_empty() {
                rects.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let (x, y) = rects[rects.len() / 2];
                let info = macros.get_mut(&m).ok_or_else(index)?;
                info.2.insert(t[1].to_string(), (x, y, direction.clone()));
            }
            pin = None;
        } else if t[0] == "END" && t.len() > 1 && t[1] == m {
            mac = None;
        }
    }
    Ok(macros)
}

/// CLASS BLOCK macro -> (width um, height um).
fn read_blocks(path: &Path) -> Result<HashMap<String, (f64, f64)>, Fail> {
    let text = read_text(path)?;
    let mut blocks = HashMap::default();
    let (mut mac, mut size, mut cls): (Option<String>, Option<(f64, f64)>, Option<String>) =
        (None, None, None);
    let index = || Fail("list index out of range".into());
    for raw in lines(&text) {
        let t = tokens(raw);
        if t.is_empty() {
            continue;
        }
        if t[0] == "MACRO" {
            mac = Some(t.get(1).ok_or_else(index)?.to_string());
            (size, cls) = (None, None);
        } else if mac.is_none() {
            continue;
        } else if t[0] == "CLASS" && cls.is_none() {
            cls = Some(
                t.get(1)
                    .ok_or_else(index)?
                    .trim_end_matches(';')
                    .to_string(),
            );
        } else if t[0] == "SIZE" && size.is_none() {
            size = Some((
                py_float(t.get(1).ok_or_else(index)?)?,
                py_float(t.get(3).ok_or_else(index)?)?,
            ));
        } else if t[0] == "END" && t.len() > 1 && Some(t[1]) == mac.as_deref() {
            if let (Some("BLOCK"), Some(s)) = (cls.as_deref(), size) {
                blocks.insert(t[1].to_string(), s);
            }
            mac = None;
        }
    }
    Ok(blocks)
}

/// A dict in insertion order with replacement in place.
#[derive(Debug)]
struct Ordered<V> {
    keys: Vec<String>,
    values: Vec<V>,
    index: HashMap<String, usize>,
}

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Ordered {
            keys: Vec::new(),
            values: Vec::new(),
            index: HashMap::default(),
        }
    }
}

impl<V> Ordered<V> {
    fn insert(&mut self, k: &str, v: V) -> usize {
        if let Some(&i) = self.index.get(k) {
            self.values[i] = v;
            i
        } else {
            self.index.insert(k.to_string(), self.keys.len());
            self.keys.push(k.to_string());
            self.values.push(v);
            self.keys.len() - 1
        }
    }
    fn get(&self, k: &str) -> Option<&V> {
        self.index.get(k).map(|&i| &self.values[i])
    }
}

/// A DEF port: direction, `LAYER` rectangle and placement.
#[derive(Debug, Clone)]
struct Port {
    direction: String,
    layer: Option<[i64; 4]>,
    placed: Option<(i64, i64)>,
}

struct Def<'a> {
    comps: Ordered<(&'a str, i64, i64, &'a str)>,
    ports: Ordered<Port>,
    nets: Vec<(&'a str, Vec<(&'a str, &'a str)>)>,
    scale: f64,
    design: String,
}

fn read_def(text: &str, max_nets: i64, max_pins: i64) -> Result<Def<'_>, Fail> {
    let mut comps = Ordered::default();
    let mut ports: Ordered<Port> = Ordered::default();
    let mut nets = Vec::new();
    let (mut scale, mut design) = (None, None);
    let mut section = None;
    let mut cur: Option<(&str, Vec<(&str, &str)>)> = None;
    let mut port: Option<usize> = None;
    let index = || Fail("list index out of range".into());
    for line in lines(text) {
        let Some(sec) = section else {
            if line.starts_with("DESIGN ") {
                design = Some(tokens(line).get(1).ok_or_else(index)?.to_string());
            } else if line.starts_with("UNITS DISTANCE MICRONS") {
                scale = Some(py_float(tokens(line).get(3).ok_or_else(index)?)?);
            } else if line.starts_with("COMPONENTS") {
                section = Some('C');
            } else if line.starts_with("PINS") {
                section = Some('P');
            } else if line.starts_with("NETS") {
                section = Some('N');
            }
            continue;
        };
        if line.starts_with("END ") {
            if sec == 'N' {
                if let Some(c) = cur.take() {
                    nets.push(c);
                }
            }
            (section, cur) = (None, None);
            continue;
        }
        match sec {
            'C' => {
                if let Some((name, master, x, y, orient)) = component(line) {
                    comps.insert(name, (master, py_int(x)?, py_int(y)?, orient));
                }
            }
            'P' => {
                if let Some(name) = port_name(line) {
                    port = Some(ports.insert(
                        name,
                        Port {
                            direction: "INPUT".into(),
                            layer: None,
                            placed: None,
                        },
                    ));
                }
                if let Some(p) = port {
                    if let Some(d) = port_direction(line) {
                        ports.values[p].direction = d.to_string();
                    }
                    if let Some(r) = port_layer(line) {
                        ports.values[p].layer =
                            Some([py_int(r[0])?, py_int(r[1])?, py_int(r[2])?, py_int(r[3])?]);
                    }
                    if let Some((x, y)) = port_placed(line) {
                        ports.values[p].placed = Some((py_int(x)?, py_int(y)?));
                    }
                }
            }
            _ => {
                if let Some(rest) = line.strip_prefix("- ") {
                    if let Some(c) = cur.take() {
                        nets.push(c);
                    }
                    let (head, tail) = split_once_space(rest);
                    let head = head.ok_or_else(index)?;
                    let mut pins = Vec::new();
                    if let Some(tail) = tail {
                        net_pins(tail.split('+').next().unwrap_or(""), &mut pins);
                    }
                    cur = Some((head, pins));
                    if nets.len() as i64 >= max_nets {
                        return fail("BUDGET: net count");
                    }
                } else if let Some(c) = cur.as_mut() {
                    if lstrip(line).starts_with('(')
                        && !line.contains("ROUTED")
                        && !line.contains("NEW")
                    {
                        net_pins(line.split('+').next().unwrap_or(""), &mut c.1);
                    }
                }
                if let Some(c) = &cur {
                    if c.1.len() as i64 > max_pins {
                        return fail(format!("BUDGET: pins of net {}", c.0));
                    }
                }
            }
        }
    }
    let (Some(scale), Some(design)) = (scale, design) else {
        return fail("DEF units or design name missing");
    };
    Ok(Def {
        comps,
        ports,
        nets,
        scale,
        design,
    })
}

/// `s.split(None, 1)`: the first token and the rest after the whitespace that follows it.
fn split_once_space(s: &str) -> (Option<&str>, Option<&str>) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && space(b[i]) {
        i += 1;
    }
    if i == b.len() {
        return (None, None);
    }
    let a = i;
    while i < b.len() && !space(b[i]) {
        i += 1;
    }
    let head = &s[a..i];
    while i < b.len() && space(b[i]) {
        i += 1;
    }
    (Some(head), (i < b.len()).then(|| &s[i..]))
}

fn read_die(text: &str) -> Result<[i64; 4], Fail> {
    for line in lines(text) {
        let mut c = Cur::at(line, 0);
        if c.lit("DIEAREA ( ") {
            if let Some((a, b)) = pair(&mut c) {
                if c.lit(" ( ") {
                    if let Some((d, e)) = pair(&mut c) {
                        return Ok([py_int(a)?, py_int(b)?, py_int(d)?, py_int(e)?]);
                    }
                }
            }
        }
        if line.starts_with("COMPONENTS") {
            break;
        }
    }
    fail("DEF DIEAREA missing")
}

// ---------------------------------------------------------------------------------------------
// Macro blockages.

/// Python's `int(x)` of a float: truncation towards zero.
fn trunc(x: f64) -> i64 {
    x.trunc() as i64
}

fn ceil(x: f64) -> i64 {
    x.ceil() as i64
}

type CellIx = (i64, i64);

struct Blockages {
    g: f64,
    max_searches: i64,
    searches: i64,
    cache: HashMap<(CellIx, CellIx), Option<f64>>,
    detoured: i64,
    extra_um: f64,
    x0: f64,
    y0: f64,
    nx: i64,
    ny: i64,
    rects: Vec<(f64, f64, f64, f64)>,
    blocked: Vec<bool>,
}

impl Blockages {
    fn new(
        def: &Def<'_>,
        blocks: &HashMap<String, (f64, f64)>,
        die: [i64; 4],
        grid: f64,
        max_searches: i64,
    ) -> Result<Self, Fail> {
        let scale = def.scale;
        let x0 = die[0] as f64 / scale;
        let y0 = die[1] as f64 / scale;
        let nx = ceil((die[2] - die[0]) as f64 / scale / grid).max(1);
        let ny = ceil((die[3] - die[1]) as f64 / scale / grid).max(1);
        if nx.saturating_mul(ny) > 4_000_000 {
            return fail("BUDGET: blockage raster");
        }
        let mut rects = Vec::new();
        for (master, x, y, orient) in &def.comps.values {
            let Some(&(mut w, mut h)) = blocks.get(*master) else {
                continue;
            };
            if matches!(*orient, "W" | "E" | "FW" | "FE") {
                (w, h) = (h, w);
            }
            let (x, y) = (*x as f64, *y as f64);
            rects.push((x / scale, y / scale, x / scale + w, y / scale + h));
        }
        let mut blocked = vec![false; (nx * ny) as usize];
        for &(lx, ly, ux, uy) in &rects {
            for j in trunc((ly - y0) / grid).max(0)..ceil((uy - y0) / grid).min(ny) {
                let cy = y0 + (j as f64 + 0.5) * grid;
                if !(ly < cy && cy < uy) {
                    continue;
                }
                for i in trunc((lx - x0) / grid).max(0)..ceil((ux - x0) / grid).min(nx) {
                    let cx = x0 + (i as f64 + 0.5) * grid;
                    if lx < cx && cx < ux {
                        blocked[(j * nx + i) as usize] = true;
                    }
                }
            }
        }
        Ok(Blockages {
            g: grid,
            max_searches,
            searches: 0,
            cache: HashMap::default(),
            detoured: 0,
            extra_um: 0.0,
            x0,
            y0,
            nx,
            ny,
            rects,
            blocked,
        })
    }

    fn is_blocked(&self, (i, j): CellIx) -> bool {
        self.blocked[(j * self.nx + i) as usize]
    }

    fn cell(&self, p: (f64, f64)) -> CellIx {
        let i = trunc((p.0 - self.x0) / self.g).max(0).min(self.nx - 1);
        let j = trunc((p.1 - self.y0) / self.g).max(0).min(self.ny - 1);
        (i, j)
    }

    fn inside(&self, (a, b): CellIx) -> bool {
        0 <= a && a < self.nx && 0 <= b && b < self.ny
    }

    /// The nearest unblocked cell of `c` and its distance in cells.
    fn free(&self, c: CellIx) -> Result<(CellIx, i64), Fail> {
        if !self.is_blocked(c) {
            return Ok((c, 0));
        }
        let mut seen = rustc_hash::FxHashSet::default();
        seen.insert(c);
        let mut todo = VecDeque::from([(c, 0i64)]);
        while let Some(((i, j), d)) = todo.pop_front() {
            for n in [(i + 1, j), (i - 1, j), (i, j + 1), (i, j - 1)] {
                if self.inside(n) && !seen.contains(&n) {
                    if !self.is_blocked(n) {
                        return Ok((n, d + 1));
                    }
                    seen.insert(n);
                    todo.push_back((n, d + 1));
                }
            }
        }
        fail("blockage raster has no free cell")
    }

    /// Whether a raster path from `a` to `b` moves only towards `b`.
    fn monotone(&self, a: CellIx, b: CellIx) -> bool {
        let si = if b.0 >= a.0 { 1 } else { -1 };
        let sj = if b.1 >= a.1 { 1 } else { -1 };
        let w = (b.0 - a.0).abs() + 1;
        let h = (b.1 - a.1).abs() + 1;
        let mut row = vec![false; w as usize];
        for dj in 0..h {
            let j = a.1 + sj * dj;
            let mut prev = false;
            for di in 0..w {
                let i = a.0 + si * di;
                let ok =
                    !self.is_blocked((i, j)) && ((di == 0 && dj == 0) || row[di as usize] || prev);
                row[di as usize] = ok;
                prev = ok;
            }
        }
        row[(w - 1) as usize]
    }

    fn bfs(&self, a: CellIx, b: CellIx) -> Result<i64, Fail> {
        let mut dist: HashMap<CellIx, i64> = HashMap::default();
        dist.insert(a, 0);
        let mut todo = VecDeque::from([a]);
        while let Some(c) = todo.pop_front() {
            let dc = dist[&c];
            if c == b {
                return Ok(dc);
            }
            let (i, j) = c;
            for n in [(i + 1, j), (i - 1, j), (i, j + 1), (i, j - 1)] {
                if self.inside(n) && !dist.contains_key(&n) && !self.is_blocked(n) {
                    dist.insert(n, dc + 1);
                    todo.push_back(n);
                }
            }
        }
        fail("no estimated path around blockages")
    }

    /// The rectilinear wire length from `p` to `q` (um) around hard macros.
    fn length(&mut self, p: (f64, f64), q: (f64, f64)) -> Result<f64, Fail> {
        let manhattan = (p.0 - q.0).abs() + (p.1 - q.1).abs();
        let (lx, ux) = (p.0.min(q.0), p.0.max(q.0));
        let (ly, uy) = (p.1.min(q.1), p.1.max(q.1));
        // Python's min/max return the first of two equal values; for these coordinates the
        // value is the same either way.
        if !self
            .rects
            .iter()
            .any(|r| r.0 < ux && lx < r.2 && r.1 < uy && ly < r.3)
        {
            return Ok(manhattan);
        }
        let (a, da) = self.free(self.cell(p))?;
        let (b, db) = self.free(self.cell(q))?;
        let key = if a <= b { (a, b) } else { (b, a) };
        let steps = match self.cache.get(&key) {
            Some(&s) => s,
            None => {
                let s = if self.monotone(a, b) {
                    None
                } else {
                    self.searches += 1;
                    if self.searches > self.max_searches {
                        return fail("BUDGET: blockage searches");
                    }
                    Some(self.bfs(a, b)? as f64 * self.g)
                };
                self.cache.insert(key, s);
                s
            }
        };
        let Some(steps) = steps else {
            return Ok(manhattan);
        };
        let detour = steps + (da + db) as f64 * self.g;
        let around = if detour > manhattan {
            detour
        } else {
            manhattan
        };
        self.detoured += 1;
        self.extra_um += around - manhattan;
        Ok(around)
    }
}

/// Prim's rectilinear minimum spanning tree rooted at point 0: each round takes the unused point
/// of the smallest distance to the tree (the lowest index among equals), and a point's distance
/// and parent change only on a strictly smaller distance. Returns `(parent, child)` edges.
fn mst(points: &[(f64, f64)]) -> Vec<(usize, usize)> {
    let n = points.len();
    if n < 3 {
        return if n == 2 { vec![(0, 1)] } else { Vec::new() };
    }
    let mut best = vec![f64::INFINITY; n];
    let mut parent = vec![0usize; n];
    best[0] = 0.0;
    let mut edges = Vec::with_capacity(n - 1);
    let mut todo: Vec<usize> = (0..n).collect();
    while !todo.is_empty() {
        let mut k = 0;
        let mut low = best[todo[0]];
        for (m, &j) in todo.iter().enumerate().skip(1) {
            if best[j] < low {
                k = m;
                low = best[j];
            }
        }
        let i = todo.remove(k);
        if i != 0 {
            edges.push((parent[i], i));
        }
        let (xi, yi) = points[i];
        for &j in &todo {
            let (x, y) = points[j];
            let d = (xi - x).abs() + (yi - y).abs();
            if d < best[j] {
                best[j] = d;
                parent[j] = i;
            }
        }
    }
    edges
}

// ---------------------------------------------------------------------------------------------
// Python number and JSON text.

/// Appends `f'{v:.{precision}e}'` to `out`.
fn push_e(out: &mut String, v: f64, precision: usize) {
    use std::fmt::Write as _;
    let start = out.len();
    let _ = write!(out, "{v:.precision$e}");
    let Some(at) = out[start..].rfind('e').map(|k| start + k) else {
        return;
    };
    let e: i32 = out[at + 1..].parse().unwrap_or(0);
    out.truncate(at + 1);
    out.push(if e < 0 { '-' } else { '+' });
    let e = e.unsigned_abs();
    if e < 10 {
        out.push('0');
    }
    let _ = write!(out, "{e}");
}

/// `f'{v:.{precision}e}'`.
#[cfg(test)]
fn fmt_e(v: f64, precision: usize) -> String {
    let mut s = String::new();
    push_e(&mut s, v, precision);
    s
}

/// `repr(v)` of a float (shortest round-trip digits; exponent form below 1e-4 and from 1e16).
fn repr(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let s = format!("{:e}", v.abs());
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let point: i32 = exp.parse::<i32>().unwrap_or(0) + 1;
    let n = digits.len() as i32;
    let body = if !(-4 < point && point <= 16) {
        let mut m = digits[..1].to_string();
        if n > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let e = point - 1;
        format!(
            "{m}e{}{:02}",
            if e < 0 { '-' } else { '+' },
            e.unsigned_abs()
        )
    } else if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point >= n {
        format!("{digits}{}.0", "0".repeat((point - n) as usize))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    if v < 0.0 { format!("-{body}") } else { body }
}

/// `round(v, digits)` of a float: the double nearest the correctly rounded decimal.
fn py_round(v: f64, digits: usize) -> f64 {
    if !v.is_finite() {
        return v;
    }
    format!("{v:.digits$}").parse().unwrap_or(v)
}

/// Python's `sum` of floats from an integer start: compensated (Neumaier) summation.
fn py_sum(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut it = values.into_iter();
    let first = it.next()?;
    let (mut f, mut c) = (0.0 + first, 0.0f64);
    for x in it {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    Some(f)
}

/// A JSON value as Python's `json` module reads and writes it (objects keep their order).
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Object(items) => items.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }
    /// `float(value)`.
    fn float(&self) -> Result<f64, Fail> {
        match self {
            Json::Int(i) => Ok(*i as f64),
            Json::Float(f) => Ok(*f),
            Json::Bool(b) => Ok(f64::from(u8::from(*b))),
            _ => fail("float() argument must be a string or a real number"),
        }
    }
    /// `int(value)`.
    fn int(&self) -> Result<i64, Fail> {
        match self {
            Json::Int(i) => i64::try_from(*i).map_err(|_| Fail("integer out of range".into())),
            Json::Float(f) if f.is_finite() => Ok(f.trunc() as i64),
            Json::Bool(b) => Ok(i64::from(*b)),
            _ => fail("int() argument must be a string or a real number"),
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn err<T>(&self) -> Result<T, Fail> {
        fail(format!("invalid JSON at byte {}", self.i))
    }
    fn lit(&mut self, l: &str) -> bool {
        if self.s[self.i..].starts_with(l.as_bytes()) {
            self.i += l.len();
            true
        } else {
            false
        }
    }
    fn value(&mut self, depth: usize) -> Result<Json, Fail> {
        if depth > 64 {
            return self.err();
        }
        self.ws();
        let Some(&b) = self.s.get(self.i) else {
            return self.err();
        };
        match b {
            b'{' => {
                self.i += 1;
                let mut items: Vec<(String, Json)> = Vec::new();
                self.ws();
                if self.lit("}") {
                    return Ok(Json::Object(items));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err();
                    }
                    let k = self.string()?;
                    self.ws();
                    if !self.lit(":") {
                        return self.err();
                    }
                    let v = self.value(depth + 1)?;
                    // A repeated key keeps its first position and takes the last value.
                    if let Some(slot) = items.iter_mut().find(|(key, _)| *key == k) {
                        slot.1 = v;
                    } else {
                        items.push((k, v));
                    }
                    self.ws();
                    if self.lit(",") {
                        continue;
                    }
                    if self.lit("}") {
                        return Ok(Json::Object(items));
                    }
                    return self.err();
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.lit("]") {
                    return Ok(Json::List(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.lit(",") {
                        continue;
                    }
                    if self.lit("]") {
                        return Ok(Json::List(items));
                    }
                    return self.err();
                }
            }
            b'"' => Ok(Json::Str(self.string()?)),
            b't' if self.lit("true") => Ok(Json::Bool(true)),
            b'f' if self.lit("false") => Ok(Json::Bool(false)),
            b'n' if self.lit("null") => Ok(Json::Null),
            b'N' if self.lit("NaN") => Ok(Json::Float(f64::NAN)),
            b'I' if self.lit("Infinity") => Ok(Json::Float(f64::INFINITY)),
            b'-' if self.lit("-Infinity") => Ok(Json::Float(f64::NEG_INFINITY)),
            _ => self.number(),
        }
    }
    fn number(&mut self) -> Result<Json, Fail> {
        let a = self.i;
        let digits = |p: &mut Self| {
            let d = p.i;
            while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                p.i += 1;
            }
            p.i - d
        };
        self.lit("-");
        if self.lit("0") {
        } else if digits(self) == 0 {
            return self.err();
        }
        let mut float = false;
        if self.s.get(self.i) == Some(&b'.') {
            let save = self.i;
            self.i += 1;
            if digits(self) == 0 {
                self.i = save;
            } else {
                float = true;
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                self.i = save;
            } else {
                float = true;
            }
        }
        let text = std::str::from_utf8(&self.s[a..self.i]).unwrap_or_default();
        if float {
            text.parse().map(Json::Float).or_else(|_| self.err())
        } else {
            text.parse().map(Json::Int).or_else(|_| self.err())
        }
    }
    fn hex4(&mut self) -> Result<u32, Fail> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .ok_or(Fail("invalid \\u escape".into()))?;
        let v = u32::from_str_radix(std::str::from_utf8(h).unwrap_or("x"), 16)
            .map_err(|_| Fail("invalid \\u escape".into()))?;
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, Fail> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&b) = self.s.get(self.i) else {
                return self.err();
            };
            self.i += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else {
                        return self.err();
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut c = self.hex4()?;
                            if (0xd800..0xdc00).contains(&c) && self.s[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let low = self.hex4()?;
                                if (0xdc00..0xe000).contains(&low) {
                                    c = 0x10000 + ((c - 0xd800) << 10) + (low - 0xdc00);
                                } else {
                                    self.i = save;
                                }
                            }
                            out.push(char::from_u32(c).ok_or(Fail("lone surrogate".into()))?);
                        }
                        _ => return self.err(),
                    }
                }
                0x00..=0x1f => return self.err(),
                _ => {
                    // Copy one UTF-8 character.
                    let start = self.i - 1;
                    let len = match b {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let ch = std::str::from_utf8(self.s.get(start..start + len).unwrap_or(&[]))
                        .map_err(|_| Fail("invalid UTF-8".into()))?;
                    out.push_str(ch);
                    self.i = start + len;
                }
            }
        }
    }
}

fn parse_json(text: &str) -> Result<Json, Fail> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return p.err();
    }
    Ok(v)
}

/// `json.dumps` of a string (`ensure_ascii`).
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `json.dumps(value, indent=2)` (`indent` Some) or `json.dumps(value)` (None).
fn dump(out: &mut String, v: &Json, indent: Option<usize>, level: usize) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => out.push_str(&i.to_string()),
        Json::Float(f) => out.push_str(&repr(*f)),
        Json::Str(s) => json_string(out, s),
        Json::List(items) if items.is_empty() => out.push_str("[]"),
        Json::Object(items) if items.is_empty() => out.push_str("{}"),
        Json::List(items) => {
            out.push('[');
            for (k, item) in items.iter().enumerate() {
                separator(out, indent, level + 1, k == 0);
                dump(out, item, indent, level + 1);
            }
            close(out, indent, level);
            out.push(']');
        }
        Json::Object(items) => {
            out.push('{');
            for (k, (key, item)) in items.iter().enumerate() {
                separator(out, indent, level + 1, k == 0);
                json_string(out, key);
                out.push_str(": ");
                dump(out, item, indent, level + 1);
            }
            close(out, indent, level);
            out.push('}');
        }
    }
}

fn separator(out: &mut String, indent: Option<usize>, level: usize, first: bool) {
    match indent {
        Some(n) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
        None if !first => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn obj(items: Vec<(&str, Json)>) -> Json {
    Json::Object(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// `str(Path(p))`: repeated separators and `.` components removed, no trailing separator.
fn path_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    let lead = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = s
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let joined = format!("{lead}{}", parts.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha_file(path: &Path) -> Result<String, Fail> {
    Ok(sha256_hex(&fs::read(path)?))
}

/// The SPEF form of a name: every character but an ASCII letter or digit, `_`, `[` and `]`
/// gets a backslash.
fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']') {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

fn orient(o: &str, x: f64, y: f64, w: f64, h: f64) -> Result<(f64, f64), Fail> {
    Ok(match o {
        "N" => (x, y),
        "S" => (w - x, h - y),
        "FN" => (w - x, y),
        "FS" => (x, h - y),
        "W" => (h - y, x),
        "E" => (y, w - x),
        "FW" => (y, x),
        "FE" => (h - y, w - x),
        _ => return fail(format!("'{o}'")),
    })
}

// ---------------------------------------------------------------------------------------------
// The estimate.

fn estimate(ws: &Path, args: &EstimateArgs) -> Result<String, Fail> {
    fs::create_dir_all(ws)?;
    let lock = OpenOptions::new()
        .append(true)
        .create(true)
        .open(ws.join("workspace.lock"))?;
    lock.try_lock()
        .map_err(|_| Fail("[Errno 11] Resource temporarily unavailable".into()))?;
    let mut others = false;
    for entry in fs::read_dir(ws)? {
        if entry?.file_name() != "workspace.lock" {
            others = true;
        }
    }
    if args.base_rev != "empty" || others {
        return fail("REVISION_CONFLICT: estimate needs a fresh empty workspace");
    }
    let out_path = ws.join("estimate.spef");
    let report_path = ws.join("result.json");
    let policy = parse_json(&fs::read_to_string(&args.policy)?)?;
    if policy.get("schema") != Some(&Json::Str("rusteda.rc-estimate-policy.v1".into())) {
        return fail("policy schema");
    }
    let need = |k: &str| policy.get(k).ok_or_else(|| Fail(format!("'{k}'")));
    let r_um = need("r_ohm_per_um")?.float()?;
    let c_um = need("c_pf_per_um")?.float()?;
    let detour = need("detour")?.float()?;
    let seg = need("max_segment_um")?.float()?;
    if !(0.0 < r_um && r_um < 100.0 && 0.0 < c_um && c_um < 1e-3)
        || !((1.0..=3.0).contains(&detour) && (1.0..=500.0).contains(&seg))
    {
        return fail("policy values outside qualified bounds");
    }
    let limits = need("limits")?;
    let limit = |k: &str| limits.get(k).ok_or_else(|| Fail(format!("'{k}'")))?.int();
    let macros = read_lef(&args.lef)?;
    let def_text = read_text(&args.def)?;
    let def = read_def(&def_text, limit("max_nets")?, limit("max_pins_per_net")?)?;
    let mut blockages = match policy.get("macro_blockage") {
        None | Some(Json::Null) => None,
        Some(spec) => {
            let g = |k: &str| spec.get(k).ok_or_else(|| Fail(format!("'{k}'")));
            let grid = g("grid_um")?.float()?;
            let searches = g("max_searches")?.int()?;
            let keys: std::collections::BTreeSet<&str> = match spec {
                Json::Object(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
                _ => Default::default(),
            };
            if !((1.0..=50.0).contains(&grid) && (1..=1_000_000).contains(&searches))
                || keys != ["grid_um", "max_searches"].into_iter().collect()
            {
                return fail("policy macro_blockage outside qualified bounds");
            }
            Some(Blockages::new(
                &def,
                &read_blocks(&args.lef)?,
                read_die(&def_text)?,
                grid,
                searches,
            )?)
        }
    };
    let max_nodes = limit("max_nodes")?;
    let scale = def.scale;

    let mut inst_index: HashMap<&str, usize> = HashMap::default();
    let mut instances: Vec<&str> = Vec::new();
    for (_, conns) in &def.nets {
        for &(c, _) in conns {
            if c != "PIN" && !inst_index.contains_key(c) {
                inst_index.insert(c, def.nets.len() + 1 + instances.len());
                instances.push(c);
            }
        }
    }
    let mut out = String::with_capacity(def_text.len());
    for line in [
        "*SPEF \"IEEE 1481-1999\"".to_string(),
        format!("*DESIGN \"{}\"", def.design),
        "*DATE \"1970-01-01 00:00:00 (fixed for byte reproducibility)\"".into(),
        "*VENDOR \"RustEDA\"".into(),
        "*PROGRAM \"estimate_spef.py\"".into(),
        "*VERSION \"1\"".into(),
        "*DESIGN_FLOW \"NAME_SCOPE LOCAL\" \"PIN_CAP NONE\"".into(),
        "*DIVIDER /".into(),
        "*DELIMITER :".into(),
        "*BUS_DELIMITER []".into(),
        "*T_UNIT 1 NS".into(),
        "*C_UNIT 1 PF".into(),
        "*R_UNIT 1 OHM".into(),
        "*L_UNIT 1 HENRY".into(),
        String::new(),
        "*NAME_MAP".into(),
    ] {
        out.push_str(&line);
        out.push('\n');
    }
    use std::fmt::Write as _;
    for (i, (n, _)) in def.nets.iter().enumerate() {
        let _ = writeln!(out, "*{} {}", i + 1, escape(n));
    }
    for n in &instances {
        let _ = writeln!(out, "*{} {}", inst_index[n], escape(n));
    }
    out.push_str("\n*PORTS\n");
    for (p, d) in def.ports.keys.iter().zip(&def.ports.values) {
        let code = match d.direction.as_str() {
            "OUTPUT" => "O",
            "INPUT" => "I",
            _ => "B",
        };
        let _ = writeln!(out, "{} {code}", escape(p));
    }

    let mut nodes_total: i64 = 0;
    let mut missing: i64 = 0;
    let mut stats: Vec<(f64, f64)> = Vec::new();
    // Per (master, pin, orientation): the oriented offset, CONN code, driver flag and escaped
    // pin; per instance: its origin and label prefix (the values of the per-pin expressions).
    let mut offsets: HashMap<(&str, &str, &str), (f64, f64, &str, bool, String)> =
        HashMap::default();
    let mut origins: HashMap<&str, (f64, f64, &str, &str, String)> = HashMap::default();
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut conn: Vec<(String, &str, Option<&str>)> = Vec::new();
    let mut caps: Vec<f64> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut res: Vec<(usize, usize, f64)> = Vec::new();
    let mut rendered = String::new();
    let mut ends: Vec<usize> = Vec::new();
    for (ni, (_, conns)) in def.nets.iter().enumerate() {
        pts.clear();
        conn.clear();
        let mut root = None;
        for &(c, p) in conns {
            if c == "PIN" {
                let Some(port) = def.ports.get(p) else {
                    missing += 1;
                    continue;
                };
                let Some((px, py)) = port.placed else {
                    missing += 1;
                    continue;
                };
                let (mut x, mut y) = (px as f64, py as f64);
                if let Some(l) = port.layer {
                    x = px as f64 + (l[0] + l[2]) as f64 / 2.0;
                    y = py as f64 + (l[1] + l[3]) as f64 / 2.0;
                }
                let driver = port.direction == "INPUT";
                if driver && root.is_none() {
                    root = Some(conn.len());
                }
                let code = if port.direction == "OUTPUT" { "O" } else { "I" };
                pts.push((x / scale, y / scale));
                conn.push((escape(p), code, None));
                continue;
            }
            let origin = match origins.get(c) {
                Some(o) => o,
                None => {
                    let &(master, ox, oy, o) =
                        def.comps.get(c).ok_or_else(|| Fail(format!("'{c}'")))?;
                    let label = format!("*{}:", inst_index[c]);
                    origins.insert(c, (ox as f64 / scale, oy as f64 / scale, master, o, label));
                    &origins[c]
                }
            };
            let (cx, cy, master, o) = (origin.0, origin.1, origin.2, origin.3);
            let key = (master, p, o);
            let hit = match offsets.get(&key) {
                Some(h) => h,
                None => {
                    let (w, h, pins) = macros
                        .get(master)
                        .ok_or_else(|| Fail(format!("'{master}'")))?;
                    let Some((px, py, direction)) = pins.get(p) else {
                        missing += 1;
                        continue;
                    };
                    let (dx, dy) = orient(o, *px, *py, *w, *h)?;
                    let code = match direction.as_str() {
                        "OUTPUT" => "O",
                        "INOUT" => "B",
                        _ => "I",
                    };
                    offsets.insert(key, (dx, dy, code, direction == "OUTPUT", escape(p)));
                    &offsets[&key]
                }
            };
            if hit.3 && root.is_none() {
                root = Some(conn.len());
            }
            pts.push((cx + hit.0, cy + hit.1));
            conn.push((format!("{}{}", origin.4, hit.4), hit.2, Some(master)));
        }
        if conn.is_empty() {
            continue;
        }
        if let Some(r) = root.filter(|&r| r > 0) {
            let p = pts.remove(r);
            pts.insert(0, p);
            let c = conn.remove(r);
            conn.insert(0, c);
        }
        caps.clear();
        caps.resize(conn.len(), 0.0);
        res.clear();
        labels.clear();
        labels.extend(conn.iter().map(|c| c.0.clone()));
        let mut node = conn.len();
        let mut length = 0.0f64;
        for (a, b) in mst(&pts) {
            let (pa, pb) = (pts[a], pts[b]);
            let dist = match blockages.as_mut() {
                None => ((pa.0 - pb.0).abs() + (pa.1 - pb.1).abs()) * detour,
                Some(bl) => bl.length(pa, pb)? * detour,
            };
            length += dist;
            let k = ceil(dist / seg).max(1);
            let piece = dist / k as f64;
            let share = c_um * piece / 2.0;
            // `max(r_um*piece, 1e-3)`: the first argument unless the second is larger.
            let r = if 1e-3 > r_um * piece {
                1e-3
            } else {
                r_um * piece
            };
            let mut prev = a;
            for s in 0..k {
                let nxt = if s == k - 1 {
                    b
                } else {
                    caps.push(0.0);
                    labels.push(format!("*{}:{node}", ni + 1));
                    node += 1;
                    node - 1
                };
                caps[prev] += share;
                caps[nxt] += share;
                res.push((prev, nxt, r));
                prev = nxt;
            }
        }
        nodes_total += caps.len() as i64;
        if nodes_total > max_nodes {
            return fail("BUDGET: estimated RC nodes");
        }
        // The rendered capacitances, one after another in `rendered` (ends in `ends`).
        rendered.clear();
        ends.clear();
        for &v in &caps {
            if v == 0.0 {
                rendered.push('0');
            } else {
                push_e(&mut rendered, v, 5);
            }
            ends.push(rendered.len());
        }
        let text_of = |k: usize| &rendered[if k == 0 { 0 } else { ends[k - 1] }..ends[k]];
        let total = py_sum((0..caps.len()).map(|k| text_of(k).parse::<f64>().unwrap_or(0.0)))
            .unwrap_or(0.0);
        let _ = write!(out, "\n*D_NET *{} ", ni + 1);
        push_e(&mut out, total, 9);
        out.push_str("\n*CONN\n");
        for (label, code, master) in &conn {
            match master {
                None => {
                    let _ = writeln!(out, "*P {label} {code}");
                }
                Some(m) => {
                    let _ = writeln!(out, "*I {label} {code} *D {}", escape(m));
                }
            }
        }
        out.push_str("*CAP\n");
        for (i, label) in labels.iter().enumerate() {
            let _ = writeln!(out, "{} {label} {}", i + 1, text_of(i));
        }
        out.push_str("*RES\n");
        for (i, (a, b, r)) in res.iter().enumerate() {
            let _ = write!(out, "{} {} {} ", i + 1, labels[*a], labels[*b]);
            push_e(&mut out, *r, 5);
            out.push('\n');
        }
        out.push_str("*END\n");
        stats.push((length, total));
    }
    fs::write(&out_path, out.as_bytes())?;
    let float_or_zero = |v: Option<f64>, d: usize| match v {
        Some(v) => Json::Float(py_round(v, d)),
        None => Json::Int(0),
    };
    let input = |p: &Path| -> Result<Json, Fail> {
        Ok(obj(vec![
            ("path", Json::Str(path_str(p))),
            ("sha256", Json::Str(sha_file(p)?)),
        ]))
    };
    let blockage = blockages.as_ref().map_or(Json::Null, |b| {
        obj(vec![
            ("macros", Json::Int(b.rects.len() as i128)),
            ("detoured_edges", Json::Int(i128::from(b.detoured))),
            ("searches", Json::Int(i128::from(b.searches))),
            ("extra_wire_um", Json::Float(py_round(b.extra_um, 3))),
        ])
    });
    let report = obj(vec![
        ("schema", Json::Str("rusteda.rc-estimate-result.v1".into())),
        ("scope", Json::Str("placement_estimate_not_signoff".into())),
        (
            "inputs",
            obj(vec![
                ("def", input(&args.def)?),
                ("lef", input(&args.lef)?),
                ("policy", input(&args.policy)?),
            ]),
        ),
        (
            "output",
            obj(vec![
                ("path", Json::Str(path_str(&out_path))),
                ("sha256", Json::Str(sha256_hex(out.as_bytes()))),
            ]),
        ),
        ("nets", Json::Int(stats.len() as i128)),
        ("rc_nodes", Json::Int(i128::from(nodes_total))),
        ("unplaced_connections", Json::Int(i128::from(missing))),
        (
            "total_wire_um",
            float_or_zero(py_sum(stats.iter().map(|s| s.0)), 3),
        ),
        (
            "total_cap_pf",
            float_or_zero(py_sum(stats.iter().map(|s| s.1)), 6),
        ),
        ("policy", policy.clone()),
        ("macro_blockage", blockage),
    ]);
    let mut text = String::new();
    dump(&mut text, &report, Some(2), 0);
    text.push('\n');
    fs::write(&report_path, text.as_bytes())?;
    fs::write(ws.join("HEAD"), b"rev1\n")?;
    let Json::Object(items) = &report else {
        return fail("report");
    };
    let keys = [
        "nets",
        "rc_nodes",
        "unplaced_connections",
        "total_wire_um",
        "total_cap_pf",
        "macro_blockage",
    ];
    let summary = Json::Object(
        keys.iter()
            .filter_map(|k| items.iter().find(|(key, _)| key == k).cloned())
            .collect(),
    );
    let mut line = String::new();
    dump(&mut line, &summary, None, 0);
    line.push('\n');
    drop(lock);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The reference's number text: `%.5e`, `repr` and `round` (values checked against Python).
    #[test]
    fn python_number_text() {
        assert_eq!(fmt_e(1.25e-5, 5), "1.25000e-05");
        assert_eq!(fmt_e(123456.5, 5), "1.23456e+05");
        assert_eq!(fmt_e(1e300, 5), "1.00000e+300");
        assert_eq!(fmt_e(0.15, 9), "1.500000000e-01");
        for (v, s) in [
            (0.25, "0.25"),
            (1e-5, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (9999999999999998.0, "9999999999999998.0"),
            (123456.5, "123456.5"),
            (3.3, "3.3"),
            (0.00019, "0.00019"),
            (50.0, "50.0"),
            (-2.5, "-2.5"),
            (0.0, "0.0"),
            (1.2345678901234567e-7, "1.2345678901234566e-07"),
        ] {
            assert_eq!(repr(v), s, "{v}");
        }
        assert_eq!(py_round(1.0625, 3), 1.062);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.675, 2), 2.67);
    }

    // Python's `sum` of floats is compensated: 0.1 added ten times is exactly 1.0.
    #[test]
    fn compensated_sum() {
        assert_eq!(py_sum(std::iter::repeat_n(0.1, 10)), Some(1.0));
        assert_eq!(py_sum([1e100, 1.0, -1e100]), Some(1.0));
        assert_eq!(py_sum(std::iter::empty()), None);
    }

    // The reference's line patterns, including a search that skips a failed first occurrence.
    #[test]
    fn line_patterns() {
        assert_eq!(
            component("- g0 INVX1 + PLACED ( 510720 -534240 ) FS ;\n"),
            Some(("g0", "INVX1", "510720", "-534240", "FS"))
        );
        assert_eq!(component("- g0 INVX1 + UNPLACED ;\n"), None);
        assert_eq!(
            port_direction("x DIRECTION  y DIRECTION OUTPUT +"),
            Some("OUTPUT")
        );
        assert_eq!(
            port_layer("+ LAYER Metal3 ( 0 0 ) ( 380 380 ) + FIXED ( 0 503860 ) N ;"),
            Some(["0", "0", "380", "380"])
        );
        assert_eq!(
            port_placed("+ FIXED ( 0 503860 ) N ;"),
            Some(("0", "503860"))
        );
        let mut pins = Vec::new();
        net_pins("( PIN clk_i ) (x y) ( a B ) ( c  d ) ( e f )", &mut pins);
        assert_eq!(pins, [("PIN", "clk_i"), ("a", "B"), ("e", "f")]);
        assert_eq!(
            split_once_space("net ( a b )\n"),
            (Some("net"), Some("( a b )\n"))
        );
        assert_eq!(split_once_space("net\n"), (Some("net"), None));
    }

    // JSON as Python writes it: key order kept, ASCII escapes, `repr` floats, integer ints.
    #[test]
    fn python_json_text() {
        let v =
            parse_json(r#"{"b": 1, "a": [0.5, 2e-05, "\u00b5m"], "c": {}, "d": null}"#).unwrap();
        let mut s = String::new();
        dump(&mut s, &v, None, 0);
        assert_eq!(
            s,
            r#"{"b": 1, "a": [0.5, 2e-05, "\u00b5m"], "c": {}, "d": null}"#
        );
        let mut s = String::new();
        dump(&mut s, &v, Some(2), 0);
        assert_eq!(
            s,
            "{\n  \"b\": 1,\n  \"a\": [\n    0.5,\n    2e-05,\n    \"\\u00b5m\"\n  ],\n  \"c\": {},\n  \"d\": null\n}"
        );
    }

    #[test]
    fn prim_tree_breaks_ties_by_index() {
        let pts = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        assert_eq!(mst(&pts), vec![(0, 1), (0, 2), (1, 3)]);
        assert_eq!(path_str(Path::new("/a//b/./c/")), "/a/b/c");
    }
}

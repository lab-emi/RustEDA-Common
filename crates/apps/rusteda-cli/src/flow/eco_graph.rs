// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The netlist graph of an ECO round and the planner of its ideal-wire operations
//! (`native_ecos.py`: `Graph`, `Planner`): drivers and loads per net bit with their pin models,
//! bounded buffer trees for the reset and fanout rounds and the split of the primary clock.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::context::{Failure, Res, at, key_error, require, text};
use super::eco_inputs::{
    Budgets, DrvKind, GraphDoc, HoldEndpoint, Inputs, Io, Module, PinModel, Pins, Report, Scan,
    Work, bit_labels, handle, json_number, number,
};
use super::py::Bit;
use crate::estimate::{Json, obj, py_sum};

/// `sum(values)` of floats (compensated), 0 for none.
pub(crate) fn fsum(values: impl IntoIterator<Item = f64>) -> f64 {
    py_sum(values).unwrap_or(0.0)
}

/// `max(a, b)` and `min(a, b)` as the reference's: the first of equal values.
pub(crate) fn first_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

pub(crate) fn first_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// A driver or load of a net bit: an instance pin with its pin model, a top-level input port
/// with its driving cell, or a top-level output port with its load.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) instance: Option<Arc<str>>,
    /// The pin of an instance; the port label of a port driver.
    pub(crate) pin: Option<Arc<str>>,
    /// The label of an output port load.
    pub(crate) port: Option<Arc<str>>,
    pub(crate) cell: Option<Arc<str>>,
    pub(crate) cap: Option<f64>,
    pub(crate) limit: Option<f64>,
    pub(crate) fanout: f64,
    pub(crate) max_fanout: f64,
    pub(crate) clock: bool,
}

impl Row {
    /// `pin` of a port, `instance/pin` otherwise.
    pub(crate) fn label(&self) -> String {
        match &self.instance {
            None => self.pin.as_deref().unwrap_or("").to_string(),
            Some(i) => format!("{i}/{}", self.pin.as_deref().unwrap_or("")),
        }
    }
    /// `(instance or '', pin or port)`.
    pub(crate) fn key(&self) -> (&str, &str) {
        (
            self.instance.as_deref().unwrap_or(""),
            self.pin
                .as_deref()
                .filter(|p| !p.is_empty())
                .or(self.port.as_deref())
                .unwrap_or(""),
        )
    }
    /// The name that locates the row: the pin of an instance, the label of a port.
    pub(crate) fn place_pin(&self) -> &str {
        match &self.instance {
            Some(_) => self.pin.as_deref().unwrap_or(""),
            None => self.port.as_deref().unwrap_or(""),
        }
    }
    pub(crate) fn sink(&self) -> Option<(String, String)> {
        self.instance
            .as_ref()
            .map(|i| (i.to_string(), self.pin.as_deref().unwrap_or("").to_string()))
    }
    fn of(instance: &Arc<str>, pin: &Arc<str>, cell: &Arc<str>, m: &PinModel) -> Row {
        Row {
            instance: Some(instance.clone()),
            pin: Some(pin.clone()),
            port: None,
            cell: Some(cell.clone()),
            cap: m.cap,
            limit: m.limit,
            fanout: m.fanout,
            max_fanout: m.max_fanout,
            clock: m.clock,
        }
    }
}

/// The bounds of an output that drives a group: its capacitance limit, own capacitance and
/// fanout limit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Output {
    pub(crate) limit: Option<f64>,
    pub(crate) cap: Option<f64>,
    pub(crate) max_fanout: f64,
}

impl From<&Row> for Output {
    fn from(r: &Row) -> Output {
        Output {
            limit: r.limit,
            cap: r.cap,
            max_fanout: r.max_fanout,
        }
    }
}

impl From<&PinModel> for Output {
    fn from(m: &PinModel) -> Output {
        Output {
            limit: m.limit,
            cap: m.cap,
            max_fanout: m.max_fanout,
        }
    }
}

/// A dict keyed by net bit, in insertion order.
#[derive(Debug)]
pub(crate) struct BitMap<V> {
    pub(crate) items: Vec<(i64, V)>,
    index: HashMap<i64, usize>,
}

impl<V> Default for BitMap<V> {
    fn default() -> Self {
        BitMap {
            items: Vec::new(),
            index: HashMap::default(),
        }
    }
}

impl<V> BitMap<V> {
    pub(crate) fn get(&self, bit: i64) -> Option<&V> {
        self.index.get(&bit).map(|&i| &self.items[i].1)
    }
    pub(crate) fn contains(&self, bit: i64) -> bool {
        self.index.contains_key(&bit)
    }
    fn entry(&mut self, bit: i64, make: impl FnOnce() -> V) -> &mut V {
        let i = match self.index.get(&bit) {
            Some(&i) => i,
            None => {
                self.index.insert(bit, self.items.len());
                self.items.push((bit, make()));
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }
}

/// The graph of the top module.
#[derive(Debug)]
pub(crate) struct Graph {
    pub(crate) doc: Arc<GraphDoc>,
    top: usize,
    pub(crate) pins: Rc<Pins>,
    cell_index: HashMap<String, usize>,
    /// Per cell of the top module: its pin labels with an integer bit, in connection order.
    cell_pins: Vec<Vec<(Arc<str>, i64)>>,
    pub(crate) loads: BitMap<Vec<Row>>,
    pub(crate) drivers: BitMap<Row>,
    pub(crate) clock_bits: HashSet<i64>,
    /// Instance and net names in use (new ECO names join it).
    pub(crate) names: RefCell<HashSet<String>>,
    /// Net label -> bit, in the order of the net names.
    pub(crate) net_bits: Vec<(String, Bit)>,
    net_index: HashMap<String, usize>,
}

fn need_dict<'a, T>(v: &'a Option<T>, k: &str) -> Res<&'a T> {
    v.as_ref().ok_or_else(|| key_error(k))
}

impl Graph {
    pub(crate) fn top(&self) -> &Module {
        &self.doc.modules.0[self.top].1
    }

    /// The cells of the top module in order: (instance, type).
    pub(crate) fn cells(&self) -> impl Iterator<Item = (&str, &str)> {
        self.top()
            .cells
            .iter()
            .flat_map(|c| c.iter())
            .map(|(name, cell)| (name, cell.kind.as_str()))
    }

    /// `g.cells[inst]['type']`.
    pub(crate) fn cell_type(&self, inst: &str) -> Option<&str> {
        let cells = self.top().cells.as_ref()?;
        self.cell_index
            .get(inst)
            .map(|&i| cells.0[i].1.kind.as_str())
    }

    /// `g.pin_bits.get((inst, pin))`.
    pub(crate) fn pin_bit(&self, inst: &str, pin: &str) -> Option<i64> {
        let &i = self.cell_index.get(inst)?;
        // A later pin of the same label replaces an earlier one, as in the reference's dict.
        self.cell_pins[i]
            .iter()
            .rev()
            .find(|(label, _)| &**label == pin)
            .map(|&(_, bit)| bit)
    }

    /// The `(pin, bit)` items of `g.pin_bits` of one instance, in order.
    pub(crate) fn pins_of(&self, inst: &str) -> &[(Arc<str>, i64)] {
        self.cell_index
            .get(inst)
            .map_or(&[], |&i| &self.cell_pins[i])
    }

    /// `g.net_bits.get(name)`.
    pub(crate) fn net_bit(&self, name: &str) -> Option<Bit> {
        self.net_index.get(name).map(|&i| self.net_bits[i].1)
    }

    /// `bit(name)`: the integer bit of a scalar net label.
    pub(crate) fn bit(&self, name: &str) -> Res<i64> {
        self.net_bit(name)
            .and_then(Bit::net)
            .ok_or_else(|| Failure::new("NETLIST", format!("unresolved scalar net: {name}")))
    }

    /// `{bit: n for n, bit in g.net_bits.items() if isinstance(bit, int)}`: the last label of
    /// every bit.
    pub(crate) fn bit_names(&self) -> HashMap<i64, &str> {
        let mut names = HashMap::default();
        for (n, bit) in &self.net_bits {
            if let Bit::Net(b) = bit {
                names.insert(*b, n.as_str());
            }
        }
        names
    }

    pub(crate) fn build(
        work: &Work,
        args: &Budgets,
        doc: Arc<GraphDoc>,
        top_name: &str,
        pins: Rc<Pins>,
        io: &Io,
    ) -> Res<Graph> {
        let top = doc.modules.0.iter().position(|(k, _)| k == top_name);
        require(top.is_some(), "NETLIST", "top module missing")?;
        let top = top.unwrap_or(0);
        let module = &doc.modules.0[top].1;
        let cells = need_dict(&module.cells, "cells")?;
        require(
            cells.len() as i64 <= args.max_cells,
            "BUDGET",
            "cell budget exceeded",
        )?;
        let module_index: HashMap<&str, usize> = doc
            .modules
            .0
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (k.as_str(), i))
            .collect();
        let mut loads: BitMap<Vec<Row>> = BitMap::default();
        let mut drivers: BitMap<Row> = BitMap::default();
        let mut clock_bits = HashSet::default();
        let mut names = HashSet::default();
        let mut cell_index = HashMap::default();
        let mut cell_pins = Vec::with_capacity(cells.len());
        let mut count = 0i64;
        // The pin labels of a port depend only on its cell type: labelled once per (type, port).
        let mut port_labels: HashMap<(usize, &str), Rc<Vec<Arc<str>>>> = HashMap::default();
        let mut kinds: HashMap<&str, Arc<str>> = HashMap::default();
        let mut checked: HashMap<(usize, usize, u8), PinModel> = HashMap::default();
        let mut label_ids: HashMap<Arc<str>, usize> = HashMap::default();
        for (ci, (inst, cell)) in cells.0.iter().enumerate() {
            names.insert(inst.clone());
            cell_index.insert(inst.clone(), ci);
            let kind = cell.kind.as_str();
            let Some(&mi) = module_index.get(kind) else {
                return Err(key_error(kind));
            };
            let kind_name = kinds.entry(kind).or_insert_with(|| Arc::from(kind)).clone();
            let instance: Arc<str> = Arc::from(inst.as_str());
            let mut own = Vec::new();
            for (port, bits) in cell.connections.iter() {
                let labels = match port_labels.get(&(mi, port)) {
                    Some(l) => l.clone(),
                    None => {
                        let ports = need_dict(&doc.modules.0[mi].1.ports, "ports")?;
                        let definition = ports.get(port).ok_or_else(|| key_error(port))?;
                        let labels: Rc<Vec<Arc<str>>> = Rc::new(
                            bit_labels(port, definition)?
                                .map(|(label, _)| Arc::from(label.as_str()))
                                .collect(),
                        );
                        port_labels.insert((mi, port), labels.clone());
                        labels
                    }
                };
                require(
                    bits.len() == labels.len(),
                    "NETLIST",
                    "cell port width mismatch",
                )?;
                let mut direction: Option<&str> = None;
                for (pin, bit) in labels.iter().zip(bits) {
                    // One work tick and one connection per bit.
                    work.tick(1)?;
                    count += 1;
                    require(
                        count <= args.max_connections,
                        "BUDGET",
                        "connection budget exceeded",
                    )?;
                    let Bit::Net(bit) = *bit else { continue };
                    let dir = match direction {
                        Some(d) => d,
                        None => {
                            let d = cell
                                .port_directions
                                .get(port)
                                .ok_or_else(|| key_error(port))?
                                .as_str();
                            direction = Some(d);
                            d
                        }
                    };
                    // The pin model and its direction check depend only on (type, pin,
                    // direction).
                    let next = label_ids.len();
                    let pin_id = *label_ids.entry(pin.clone()).or_insert(next);
                    let way = match dir {
                        "input" => 0u8,
                        "output" => 1,
                        _ => 2,
                    };
                    let model = match checked.get(&(mi, pin_id, way)) {
                        Some(m) => m.clone(),
                        None => {
                            let m = pins.get(kind, pin)?;
                            require(
                                dir.to_lowercase() == m.direction.to_lowercase(),
                                "NETLIST",
                                "JSON/Liberty direction mismatch",
                            )?;
                            if way != 2 {
                                checked.insert((mi, pin_id, way), m.clone());
                            }
                            m
                        }
                    };
                    let row = Row::of(&instance, pin, &kind_name, &model);
                    own.push((pin.clone(), bit));
                    match way {
                        0 => loads.entry(bit, Vec::new).push(row),
                        1 => {
                            require(!drivers.contains(bit), "NETLIST", "multiple signal drivers")?;
                            drivers.entry(bit, || row);
                        }
                        _ => {
                            return Err(Failure::new("NETLIST", "inout signal ECO unsupported"));
                        }
                    }
                    if model.clock {
                        clock_bits.insert(bit);
                    }
                }
            }
            cell_pins.push(own);
        }
        let netnames = need_dict(&module.netnames, "netnames")?;
        require(
            netnames.len() as i64 <= args.max_connections,
            "BUDGET",
            "net-name budget exceeded",
        )?;
        let mut net_bits: Vec<(String, Bit)> = Vec::with_capacity(netnames.len());
        let mut net_index: HashMap<String, usize> = HashMap::default();
        for (name, definition) in netnames.iter() {
            names.insert(name.to_string());
            for (label, bit) in bit_labels(name, definition)? {
                match net_index.get(&label) {
                    Some(&i) => net_bits[i].1 = bit,
                    None => {
                        net_index.insert(label.clone(), net_bits.len());
                        net_bits.push((label, bit));
                    }
                }
            }
        }
        for (name, definition) in need_dict(&module.ports, "ports")?.iter() {
            for (label, bit) in bit_labels(name, definition)? {
                let Bit::Net(bit) = bit else { continue };
                let direction = definition
                    .direction
                    .as_deref()
                    .ok_or_else(|| key_error("direction"))?;
                if direction == "input" {
                    let d = io.drivers.get(&label);
                    require(
                        d.is_some() && !drivers.contains(bit),
                        "SDC_IO_MISMATCH",
                        "missing input driving cell",
                    )?;
                    let Some((cell, pin)) = d else { continue };
                    let model = pins.get(cell, pin)?;
                    require(
                        &*model.direction == "Output",
                        "SDC_IO_MISMATCH",
                        "driving-cell pin is not output",
                    )?;
                    let row = Row {
                        instance: None,
                        pin: Some(Arc::from(label.as_str())),
                        port: None,
                        cell: Some(Arc::from(cell.as_str())),
                        cap: model.cap,
                        limit: model.limit,
                        fanout: model.fanout,
                        max_fanout: model.max_fanout,
                        clock: model.clock,
                    };
                    drivers.entry(bit, || row);
                } else if direction == "output" {
                    let load = io.loads_pf.get(&label);
                    require(load.is_some(), "SDC_IO_MISMATCH", "missing output load")?;
                    let row = Row {
                        instance: None,
                        pin: None,
                        port: Some(Arc::from(label.as_str())),
                        cell: None,
                        cap: Some(number(load.copied().unwrap_or(0.0), false)?),
                        limit: None,
                        fanout: 0.0,
                        max_fanout: f64::INFINITY,
                        clock: false,
                    };
                    loads.entry(bit, Vec::new).push(row);
                } else {
                    return Err(Failure::new("NETLIST", "top inout unsupported"));
                }
            }
        }
        Ok(Graph {
            doc,
            top,
            pins,
            cell_index,
            cell_pins,
            loads,
            drivers,
            clock_bits,
            names: RefCell::new(names),
            net_bits,
            net_index,
        })
    }

    pub(crate) fn loads_of(&self, bit: i64) -> &[Row] {
        self.loads.get(bit).map_or(&[], |v| v.as_slice())
    }

    /// `sink_binding(row)`: the instance, pin and bit of a reported endpoint, whose master, net
    /// and driver must be the netlist's.
    pub(crate) fn sink_binding(&self, row: &HoldEndpoint) -> Res<(String, String, i64)> {
        let Some((inst, pin)) = row.endpoint.rsplit_once('/') else {
            return Err(Failure::new(
                "INPUT",
                "not enough values to unpack (expected 2, got 1)",
            ));
        };
        let bit = self.pin_bit(inst, pin);
        require(
            bit.is_some() && self.cell_type(inst) == row.cell.as_deref(),
            "STALE_REPORT",
            "endpoint master/pin mismatch",
        )?;
        let bit = bit.unwrap_or(0);
        let driver = self.drivers.get(bit);
        require(driver.is_some(), "STALE_REPORT", "endpoint driver missing")?;
        let actual = driver.map(Row::label).unwrap_or_default();
        // CTS regroup_prefixes may move a sink to another buffer of the same fanout/reset tree
        // level: same source signal, so the pin binding holds.
        let reported = row.net_driver.as_deref().unwrap_or("");
        let level = tree_level(&actual);
        let regrouped = level.is_some() && level == tree_level(reported);
        let net_bit = match row.net.as_deref() {
            Some(net) if !regrouped => Some(self.bit(net)?),
            _ => None,
        };
        require(
            regrouped || Some(bit) == net_bit,
            "STALE_REPORT",
            format!("endpoint net mismatch: {}", row.endpoint),
        )?;
        require(
            regrouped || row.net_driver.as_deref() == Some(actual.as_str()),
            "STALE_REPORT",
            format!("endpoint driver mismatch: {}", row.endpoint),
        )?;
        Ok((inst.to_string(), pin.to_string(), bit))
    }
}

/// `native_(fanout|reset)_N`: the scanner after it, and the matched text.
fn tree_prefix(s: &str) -> Option<(Scan<'_>, usize)> {
    let mut c = Scan::new(s);
    if !c.lit("native_fanout_") && !c.lit("native_reset_") {
        return None;
    }
    c.digits()?;
    let at = s.len() - c.rest().len();
    Some((c, at))
}

/// `(native_(?:fanout|reset)_[0-9]+)_([0-9]+)_[0-9]+` as a full match: the tree and its depth.
pub(crate) fn tree_member(s: &str) -> Option<(&str, &str)> {
    let (mut c, at) = tree_prefix(s)?;
    if !c.lit("_") {
        return None;
    }
    let depth = c.digits()?;
    if !c.lit("_") {
        return None;
    }
    c.digits()?;
    c.end().then_some((&s[..at], depth))
}

/// `(native_(?:fanout|reset)_[0-9]+_[0-9]+)_[0-9]+/[A-Za-z0-9_]+` as a full match: the level.
fn tree_level(s: &str) -> Option<&str> {
    let (mut c, _) = tree_prefix(s)?;
    if !c.lit("_") {
        return None;
    }
    c.digits()?;
    let at = s.len() - c.rest().len();
    if !c.lit("_") {
        return None;
    }
    c.digits()?;
    if !c.lit("/") {
        return None;
    }
    c.run(|ch| ch.is_ascii_alphanumeric() || ch == '_')?;
    c.end().then_some(&s[..at])
}

/// `PREFIX_[0-9]+` as a full match.
pub(crate) fn numbered(s: &str, prefix: &str) -> bool {
    let mut c = Scan::new(s);
    c.lit(prefix) && c.lit("_") && c.digits().is_some() && c.end()
}

/// One operation of an ECO plan (`rustroad.eco-plan.v1`).
#[derive(Debug, Clone)]
pub(crate) enum Op {
    BufferSinks {
        cell: String,
        instance: String,
        sinks: Vec<(String, String)>,
        ports: Vec<String>,
    },
    InsertBuffer {
        cell: String,
        instance: String,
        sink: (String, String),
    },
    InsertDiode {
        cell: String,
        instance: String,
        net: String,
    },
    ResizeCell {
        instance: String,
        cell: String,
    },
}

fn sink_json(sink: &(String, String)) -> Json {
    obj(vec![
        ("instance", Json::Str(sink.0.clone())),
        ("pin", Json::Str(sink.1.clone())),
    ])
}

impl Op {
    pub(crate) fn to_json(&self) -> Json {
        match self {
            Op::BufferSinks {
                cell,
                instance,
                sinks,
                ports,
            } => obj(vec![
                ("op", Json::str("buffer_sinks")),
                ("cell", Json::str(cell)),
                ("instance", Json::str(instance)),
                ("net", Json::Str(format!("{instance}_n"))),
                ("sinks", Json::List(sinks.iter().map(sink_json).collect())),
                ("ports", Json::List(ports.iter().map(Json::str).collect())),
            ]),
            Op::InsertBuffer {
                cell,
                instance,
                sink,
            } => obj(vec![
                ("op", Json::str("insert_buffer")),
                ("cell", Json::str(cell)),
                ("instance", Json::str(instance)),
                ("net", Json::Str(format!("{instance}_n"))),
                ("sink", sink_json(sink)),
            ]),
            Op::InsertDiode {
                cell,
                instance,
                net,
            } => obj(vec![
                ("op", Json::str("insert_diode")),
                ("cell", Json::str(cell)),
                ("instance", Json::str(instance)),
                ("net", Json::str(net)),
            ]),
            Op::ResizeCell { instance, cell } => obj(vec![
                ("op", Json::str("resize_cell")),
                ("instance", Json::str(instance)),
                ("cell", Json::str(cell)),
            ]),
        }
    }
}

/// A selected buffer of the policy.
#[derive(Debug, Clone)]
pub(crate) struct Buffer {
    pub(crate) cell: String,
    pub(crate) input: String,
    pub(crate) output: String,
}

/// `HEAVY`: rustroad's `MAX_NET_WEIGHT` (milli-units).
pub(crate) const HEAVY: i64 = 16000;

/// The planner of one round.
pub(crate) struct Planner<'a> {
    pub(crate) i: &'a Inputs<'a>,
    pub(crate) p: &'a Json,
    pub(crate) g: &'a Graph,
    pub(crate) ops: Vec<Op>,
    pub(crate) checks: Vec<Json>,
    pub(crate) excluded: Vec<Json>,
    /// Net -> placement weight of this round's repairs.
    pub(crate) weights: Vec<(String, i64)>,
    pub(crate) repaired: HashSet<String>,
    pub(crate) bus_rewrite: bool,
    fanout: i64,
    fraction: f64,
    pub(crate) prefix: String,
    /// The selected buffers in policy order.
    pub(crate) buffers: Vec<Buffer>,
    pub(crate) clocks: HashSet<i64>,
    pub(crate) feedback_nets: HashSet<String>,
}

impl std::fmt::Debug for Planner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planner")
            .field("prefix", &self.prefix)
            .field("operations", &self.ops.len())
            .finish()
    }
}

impl<'a> Planner<'a> {
    pub(crate) fn new(inputs: &'a Inputs<'a>, policy: &'a Json, graph: &'a Graph) -> Res<Self> {
        let bus_rewrite = at(&inputs.manifest, "profiles")?
            .get("cell_buses")
            .is_some();
        let fanout = at(policy, "target_fanout")?.int()?;
        let fraction = json_number(at(policy, "cap_fraction")?, true)?;
        require(
            (2..=9).contains(&fanout) && fraction <= 1.0,
            "POLICY",
            "qualified fanout/cap bounds",
        )?;
        let prefix = match policy.get("prefix") {
            None => "native_eco".to_string(),
            Some(p) => handle(p.as_str().unwrap_or(""))?.to_string(),
        };
        let listed = at(policy, "buffers")?.as_list().unwrap_or(&[]);
        let mut buffers: Vec<Buffer> = Vec::new();
        for b in listed {
            let buffer = Buffer {
                cell: text(b, "cell")?.to_string(),
                input: b
                    .get("input")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string(),
                output: b
                    .get("output")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            match buffers.iter_mut().find(|x| x.cell == buffer.cell) {
                Some(slot) => *slot = buffer,
                None => buffers.push(buffer),
            }
        }
        require(
            buffers.len() == listed.len() && listed.len() <= 32,
            "POLICY",
            "buffer selection count",
        )?;
        let roots = at(policy, "clock_roots")?.as_list().unwrap_or(&[]);
        let primary = text(policy, "primary_clock")?;
        require(
            !roots.is_empty() && roots.iter().any(|r| r.as_str() == Some(primary)),
            "POLICY",
            "primary and gated roots must be explicit",
        )?;
        let mut clocks = graph.clock_bits.clone();
        for n in roots {
            clocks.insert(graph.bit(n.as_str().unwrap_or(""))?);
        }
        Ok(Planner {
            i: inputs,
            p: policy,
            g: graph,
            ops: Vec::new(),
            checks: Vec::new(),
            excluded: Vec::new(),
            weights: Vec::new(),
            repaired: HashSet::default(),
            bus_rewrite,
            fanout,
            fraction,
            prefix,
            buffers,
            clocks,
            feedback_nets: HashSet::default(),
        })
    }

    /// `prefix_of(operation)`.
    pub(crate) fn prefix_of(operation: &str) -> String {
        format!("native_{}", operation.replace('-', "_"))
    }

    pub(crate) fn buffer(&self, cell: &str) -> Option<&Buffer> {
        self.buffers.iter().find(|b| b.cell == cell)
    }

    /// `model(cell)`: a selected buffer with its input and output pin models.
    pub(crate) fn model(&self, cell: &str) -> Res<(&Buffer, PinModel, PinModel)> {
        let b = self.buffer(cell).ok_or_else(|| {
            Failure::new("POLICY", format!("buffer not explicitly selected: {cell}"))
        })?;
        let a = self.g.pins.get(cell, &b.input)?;
        let z = self.g.pins.get(cell, &b.output)?;
        require(
            &*a.direction == "Input"
                && &*z.direction == "Output"
                && a.cap.is_some()
                && z.limit.is_some(),
            "PIN_TABLE",
            "selected buffer lacks cap/direction bounds",
        )?;
        Ok((b, a, z))
    }

    /// `planner.weights[net] = HEAVY`.
    pub(crate) fn weigh(&mut self, net: String) {
        match self.weights.iter_mut().find(|(n, _)| *n == net) {
            Some(slot) => slot.1 = HEAVY,
            None => self.weights.push((net, HEAVY)),
        }
    }

    /// `fits(group, output)`: whether the output may drive the group within the fanout target
    /// and the capacitance fraction.
    pub(crate) fn fits(&self, group: &[Row], output: Output) -> Res<bool> {
        self.i.tick(group.len() as i64 + 1)?;
        require(
            group.iter().all(|x| x.cap.is_some()),
            "PIN_TABLE",
            "selected load has no capacitance",
        )?;
        if group.len() as i64 > self.fanout {
            return Ok(false);
        }
        let fanout = fsum(group.iter().map(|x| x.fanout));
        let within = fanout <= first_min(self.fanout as f64, output.max_fanout);
        if !within {
            return Ok(false);
        }
        Ok(match output.limit {
            None => true,
            Some(limit) => {
                let own = output.cap.filter(|c| *c != 0.0).unwrap_or(0.0);
                fsum(group.iter().filter_map(|x| x.cap)) + own <= limit * self.fraction
            }
        })
    }

    /// `append(op)`: adds an operation whose new instance and net names are free handles.
    pub(crate) fn append(&mut self, op: Op) -> Res<()> {
        require(
            (self.ops.len() as i64) < self.i.args.max_operations,
            "BUDGET",
            "ECO operation budget exceeded",
        )?;
        let (instance, net) = match &op {
            Op::BufferSinks { instance, .. } | Op::InsertBuffer { instance, .. } => {
                (instance.clone(), format!("{instance}_n"))
            }
            // Diodes and resizes name no new net: they join the plan directly.
            Op::InsertDiode { .. } | Op::ResizeCell { .. } => {
                return Err(Failure::new("INPUT", "operation without a new net"));
            }
        };
        for name in [instance, net] {
            handle(&name)?;
            let mut names = self.g.names.borrow_mut();
            require(
                !names.contains(&name),
                "HANDLE",
                format!("ECO name collision: {name}"),
            )?;
            names.insert(name);
        }
        self.ops.push(op);
        Ok(())
    }

    /// `port_diodes(geo)`: one diode per primary input bit; `place` seeds it at its port.
    pub(crate) fn port_diodes(
        &mut self,
        mut place: impl FnMut(&str, &str, &str) -> Res<()>,
    ) -> Res<()> {
        let cfg = at(self.p, "port_diode")?;
        let ok = cfg.as_object().is_some_and(|items| {
            items.len() == 2 && cfg.get("cell").is_some() && cfg.get("pin").is_some()
        });
        require(ok, "POLICY", "port diode policy")?;
        let cell = handle(cfg.get("cell").and_then(Json::as_str).unwrap_or(""))?.to_string();
        handle(cfg.get("pin").and_then(Json::as_str).unwrap_or(""))?;
        let mut count = 0usize;
        let ports = self
            .g
            .top()
            .ports
            .as_ref()
            .ok_or_else(|| key_error("ports"))?;
        for (port, definition) in ports.iter() {
            let direction = definition
                .direction
                .as_deref()
                .ok_or_else(|| key_error("direction"))?;
            if direction != "input" {
                continue;
            }
            for (label, bit) in bit_labels(port, definition)? {
                if bit.net().is_none() {
                    continue;
                }
                let name = format!("{}_port_diode_{count}", self.prefix);
                handle(&name)?;
                require(
                    (self.ops.len() as i64) < self.i.args.max_operations,
                    "BUDGET",
                    "ECO operation budget exceeded",
                )?;
                {
                    let mut names = self.g.names.borrow_mut();
                    require(
                        !names.contains(&name),
                        "HANDLE",
                        format!("ECO name collision: {name}"),
                    )?;
                    names.insert(name.clone());
                }
                self.ops.push(Op::InsertDiode {
                    cell: cell.clone(),
                    instance: name.clone(),
                    net: label.clone(),
                });
                place(&name, &cell, &label)?;
                count += 1;
                self.i.tick(1)?;
            }
        }
        require(
            count > 0,
            "POLICY",
            "port diode policy found no primary input bit",
        )?;
        self.checks.push(obj(vec![
            ("port_diodes", Json::Int(count as i128)),
            ("cell", Json::Str(cell)),
        ]));
        Ok(())
    }

    /// `buffer_group(cell, group, name)`: one buffer in front of the group; returns its input
    /// as the load that replaces the group.
    fn buffer_group(&mut self, cell: &str, group: &[Row], name: String) -> Res<Row> {
        let (b, a, z) = self.model(cell)?;
        let input: Arc<str> = Arc::from(b.input.as_str());
        require(
            self.fits(group, (&z).into())?,
            "CAPACITY",
            "buffer leaf exceeds corner cap/fanout bound",
        )?;
        require(
            self.bus_rewrite
                || group
                    .iter()
                    .all(|x| x.pin.as_deref().is_none_or(|p| !p.contains('['))),
            "UNSUPPORTED_VECTOR_SINK",
            "grouped bus ECO requires explicit cell-bus metadata",
        )?;
        self.append(Op::BufferSinks {
            cell: cell.to_string(),
            instance: name.clone(),
            sinks: group.iter().filter_map(Row::sink).collect(),
            ports: group
                .iter()
                .filter(|x| x.instance.is_none())
                .map(|x| x.port.as_deref().unwrap_or("").to_string())
                .collect(),
        })?;
        let own = z.cap.filter(|c| *c != 0.0).unwrap_or(0.0);
        self.checks.push(obj(vec![
            ("instance", Json::Str(name.clone())),
            ("cell", Json::str(cell)),
            (
                "cap_pf",
                Json::Float(fsum(group.iter().filter_map(|x| x.cap)) + own),
            ),
            ("fanout", Json::Float(fsum(group.iter().map(|x| x.fanout)))),
            ("cap_limit_pf", Json::opt_float(z.limit)),
        ]));
        Ok(Row {
            instance: Some(Arc::from(name.as_str())),
            pin: Some(input),
            port: None,
            cell: Some(Arc::from(cell)),
            cap: a.cap,
            limit: a.limit,
            fanout: a.fanout,
            max_fanout: a.max_fanout,
            clock: a.clock,
        })
    }

    /// The first candidate buffer whose output fits the items.
    fn choose(&self, candidates: &[String], items: &[Row]) -> Res<Option<String>> {
        for c in candidates {
            let z = self.model(c)?.2;
            if self.fits(items, (&z).into())? {
                return Ok(Some(c.clone()));
            }
        }
        Ok(None)
    }

    /// `tree(bit, label, forced, pin_filter)`: a bounded buffer tree over the loads of a net.
    fn tree(
        &mut self,
        bit: i64,
        label: &str,
        forced: Option<(&str, &str)>,
        pin_filter: Option<&str>,
    ) -> Res<()> {
        let root = self.g.drivers.get(bit);
        require(root.is_some(), "NETLIST", "repair root has no driver")?;
        let Some(root) = root.cloned() else {
            return Ok(());
        };
        let mut current: Vec<Row> = self.g.loads_of(bit).to_vec();
        current.sort_by(|a, b| a.key().cmp(&b.key()));
        let mut fixed: Vec<Row> = Vec::new();
        if let Some(filter) = pin_filter.filter(|f| !f.is_empty()) {
            let (keep, rest): (Vec<Row>, Vec<Row>) = current
                .into_iter()
                .partition(|x| x.pin.as_deref() == Some(filter));
            (current, fixed) = (keep, rest);
        }
        require(
            !current.is_empty(),
            "POLICY",
            "selected repair has no matching sinks",
        )?;
        let mut depth = 0i64;
        loop {
            let all: Vec<Row> = fixed.iter().chain(current.iter()).cloned().collect();
            if self.fits(&all, (&root).into())? {
                break;
            }
            require(
                depth < self.i.args.max_depth,
                "BUDGET",
                "buffer tree depth budget",
            )?;
            let candidates: Vec<String> = match forced {
                Some((leaf, trunk)) => vec![if depth == 0 { leaf } else { trunk }.to_string()],
                None => self.buffers.iter().map(|b| b.cell.clone()).collect(),
            };
            let mut following: Vec<Row> = Vec::new();
            let mut group: Vec<Row> = Vec::new();
            let count = current.len();
            for item in current {
                if !group.is_empty() {
                    let mut probe = group.clone();
                    probe.push(item.clone());
                    if self.choose(&candidates, &probe)?.is_none() {
                        let cell = self.choose(&candidates, &group)?.ok_or_else(|| {
                            Failure::new("POLICY", "buffer not explicitly selected: None")
                        })?;
                        let name = format!("{label}_{depth}_{}", following.len());
                        following.push(self.buffer_group(&cell, &group, name)?);
                        group.clear();
                    }
                }
                group.push(item);
                require(
                    self.choose(&candidates, &group)?.is_some(),
                    "CAPACITY",
                    "single load exceeds every selected buffer",
                )?;
            }
            if !group.is_empty() {
                let cell = self.choose(&candidates, &group)?.ok_or_else(|| {
                    Failure::new("POLICY", "buffer not explicitly selected: None")
                })?;
                let name = format!("{label}_{depth}_{}", following.len());
                following.push(self.buffer_group(&cell, &group, name)?);
            }
            require(
                following.len() < count || depth == 0,
                "BUDGET",
                "buffer tree fails to contract",
            )?;
            current = following;
            depth += 1;
        }
        let all: Vec<&Row> = fixed.iter().chain(current.iter()).collect();
        let own = root.cap.filter(|c| *c != 0.0).unwrap_or(0.0);
        self.checks.push(obj(vec![
            ("root", Json::str(label)),
            ("depth", Json::Int(depth.into())),
            ("fanout", Json::Float(fsum(all.iter().map(|x| x.fanout)))),
            (
                "cap_pf",
                Json::Float(fsum(all.iter().filter_map(|x| x.cap)) + own),
            ),
            ("cap_limit_pf", Json::opt_float(root.limit)),
        ]));
        Ok(())
    }

    /// The reset round: a forced leaf/trunk tree over the reset pins of the reset net.
    pub(crate) fn reset(&mut self) -> Res<()> {
        let cfg = at(self.p, "reset")?;
        let bit = self.g.bit(text(cfg, "net")?)?;
        require(
            !self.clocks.contains(&bit),
            "POLICY",
            "reset root is a clock net",
        )?;
        let prefix = self.prefix.clone();
        self.tree(
            bit,
            &prefix,
            Some((text(cfg, "leaf")?, text(cfg, "trunk")?)),
            Some(text(cfg, "pin")?),
        )
    }

    /// The fanout round: a tree over every net with a reported fanout or capacitance violation.
    pub(crate) fn fanout_repair(&mut self, reports: &[Arc<Report>]) -> Res<()> {
        let mut requested: Vec<&str> = Vec::new();
        for report in reports {
            let rows = &report
                .sections
                .as_ref()
                .and_then(|s| s.drv.as_ref())
                .ok_or_else(|| key_error("drv"))?
                .rows;
            for r in rows {
                if matches!(r.kind, DrvKind::Fanout | DrvKind::Capacitance)
                    && r.slack.is_some_and(|s| s < 0.0)
                {
                    requested.push(&r.net);
                }
            }
        }
        requested.sort_unstable();
        requested.dedup();
        require(
            requested.len() as i64 <= self.i.args.max_cells,
            "BUDGET",
            "reported repair root budget",
        )?;
        let mut seen: HashSet<i64> = HashSet::default();
        for (index, name) in requested.into_iter().enumerate() {
            let bit = self.g.bit(name)?;
            if seen.contains(&bit) || self.clocks.contains(&bit) {
                let reason = if seen.contains(&bit) {
                    "alias"
                } else {
                    "explicit clock/CTS scope"
                };
                self.excluded.push(obj(vec![
                    ("net", Json::str(name)),
                    ("reason", Json::str(reason)),
                ]));
                continue;
            }
            seen.insert(bit);
            let label = format!("{}_{index}", self.prefix);
            self.tree(bit, &label, None, None)?;
        }
        Ok(())
    }

    /// The primary-clock round: the loads of the primary clock behind balanced branch buffers.
    pub(crate) fn primary(&mut self) -> Res<()> {
        let bit = self.g.bit(text(self.p, "primary_clock")?)?;
        let cell = text(self.p, "primary_buffer")?.to_string();
        let group: Vec<Row> = self.g.loads_of(bit).to_vec();
        require(
            group.iter().all(|x| x.cap.is_some()),
            "PIN_TABLE",
            "primary clock load lacks capacitance",
        )?;
        let count = (group.len() as i64 + self.fanout - 1) / self.fanout;
        let driver = self.g.drivers.get(bit).cloned();
        if count <= 1 {
            let driver = driver.as_ref().ok_or_else(|| key_error(&bit.to_string()))?;
            if self.fits(&group, driver.into())? {
                return Ok(());
            }
        }
        require(
            1 <= count && count <= self.i.args.max_operations,
            "BUDGET",
            "primary-clock branch budget",
        )?;
        let mut groups: Vec<Vec<Row>> = vec![Vec::new(); count as usize];
        let output: Output = (&self.model(&cell)?.2).into();
        let mut sorted = group;
        sorted.sort_by(|a, b| {
            let (ca, cb) = (-a.cap.unwrap_or(0.0), -b.cap.unwrap_or(0.0));
            ca.partial_cmp(&cb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    (
                        a.instance.as_deref().unwrap_or(""),
                        a.pin.as_deref().unwrap_or(""),
                    )
                        .cmp(&(
                            b.instance.as_deref().unwrap_or(""),
                            b.pin.as_deref().unwrap_or(""),
                        ))
                })
        });
        for item in sorted {
            let mut possible = Vec::new();
            for (j, g) in groups.iter().enumerate() {
                let mut probe = g.clone();
                probe.push(item.clone());
                if self.fits(&probe, output)? {
                    possible.push(j);
                }
            }
            require(
                !possible.is_empty(),
                "CAPACITY",
                "primary clock split exceeds selected buffer",
            )?;
            let mut at_group = possible[0];
            let key = |j: usize| {
                (
                    fsum(groups[j].iter().filter_map(|x| x.cap)),
                    groups[j].len(),
                )
            };
            for &j in &possible[1..] {
                let (a, b) = (key(j), key(at_group));
                if a.0 < b.0 || (a.0 == b.0 && a.1 < b.1) {
                    at_group = j;
                }
            }
            groups[at_group].push(item);
        }
        let mut roots = Vec::with_capacity(groups.len());
        for (j, g) in groups.iter().enumerate() {
            let name = format!("{}_{j}", self.prefix);
            roots.push(self.buffer_group(&cell, g, name)?);
        }
        let driver = driver.as_ref().ok_or_else(|| key_error(&bit.to_string()))?;
        require(
            self.fits(&roots, driver.into())?,
            "CAPACITY",
            "primary source capacity exceeded",
        )
    }
}

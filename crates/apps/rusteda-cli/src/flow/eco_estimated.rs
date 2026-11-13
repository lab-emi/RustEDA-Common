// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The estimated-RC operations of an ECO round (`native_ecos.py`: `Geometry`, `electrical`,
//! `clock_balance`, `estimated_hold`): pin locations of the timed placement, wire-aware DRV
//! repair with geometric clustering, launch-clock balancing, shared launch chains and endpoint
//! delay chains, and the placement seed of the next round.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::context::{Failure, Res, at, key_error, require, text};
use super::eco_graph::{
    Buffer, Op, Planner, Row, first_max, first_min, fsum, numbered, tree_member,
};
use super::eco_inputs::{DrvKind, HoldEndpoint, HoldFix, Inputs, Report, json_number, number};
use super::liberty::{self, Group, Library, Table};
use super::py::{py_ceil, py_floor, zero_padded};
use crate::estimate::{
    Blockages, Def, Json, Macros, lef_blocks, lef_macros, mst, obj, orient, py_round, read_def,
    read_die, text_of,
};

type Point = (f64, f64);

/// Wire load allowance of one chain stage: stages are placed at the same pin.
const CHAIN_WIRE_PF: f64 = 0.01;
/// A centre point inside a placed `CLASS BLOCK` macro moves this far beyond its bottom or top
/// edge (rows run along both).
const OUTSIDE_UM: f64 = 2.0;

/// The files of the timed placement, read in the reference's order.
pub(crate) struct PlacementFiles {
    rc: Arc<Json>,
    lef_text: String,
    pub(crate) def_text: String,
}

impl std::fmt::Debug for PlacementFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementFiles").finish()
    }
}

fn loaded_text(inputs: &Inputs<'_>, record: &Json, cap: u64) -> Res<String> {
    // Read and hashed once: the record names exactly the text that is parsed.
    let (path, loaded) = inputs.loaded(record, cap)?;
    Ok(text_of(loaded.bytes.clone(), &path)?)
}

impl PlacementFiles {
    pub(crate) fn read(inputs: &Inputs<'_>) -> Res<PlacementFiles> {
        let place = at(&inputs.manifest, "placement")?;
        inputs.file(at(at(&inputs.manifest, "tools")?, "estimator")?, 1 << 20)?;
        let rc = inputs.document(at(&inputs.manifest, "rc_policy")?, 1 << 20)?;
        require(
            rc.get("schema").and_then(Json::as_str) == Some("rusteda.rc-estimate-policy.v1"),
            "SCHEMA",
            "RC estimate policy schema",
        )?;
        let lef = at(at(&inputs.manifest, "profiles")?, "lef")?;
        let lef_text = loaded_text(inputs, lef, 64 << 20)?;
        let def_text = loaded_text(inputs, at(place, "def")?, 256 << 20)?;
        super::trace("geometry: files read");
        Ok(PlacementFiles {
            rc,
            lef_text,
            def_text,
        })
    }
}

/// Pin locations (um) of the timed placement and the seed of the next one.
pub(crate) struct Geometry<'d, 'a> {
    i: &'a Inputs<'a>,
    pub(crate) rc: Arc<Json>,
    pub(crate) macros: Macros,
    def: Def<'d>,
    blockages: RefCell<Option<Blockages>>,
    base: Arc<Json>,
    dbu: f64,
    /// New cells in the order of their placement: (instance, cell, centre).
    new: Vec<(String, String, Point)>,
    new_index: HashMap<String, usize>,
}

impl std::fmt::Debug for Geometry<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Geometry")
            .field("new", &self.new.len())
            .finish()
    }
}

impl<'d, 'a> Geometry<'d, 'a> {
    pub(crate) fn new(inputs: &'a Inputs<'a>, files: &'d PlacementFiles) -> Res<Self> {
        let rc = files.rc.clone();
        let macros = lef_macros(&files.lef_text)?;
        let limits = at(&rc, "limits")?;
        let def = read_def(
            &files.def_text,
            at(limits, "max_nets")?.int()?,
            at(limits, "max_pins_per_net")?.int()?,
        )?;
        let blockages = match rc.get("macro_blockage") {
            None | Some(Json::Null) => None,
            Some(spec) => {
                let grid = at(spec, "grid_um")?.float()?;
                let searches = at(spec, "max_searches")?.int()?;
                let keys_ok = spec.as_object().is_some_and(|items| {
                    items.len() == 2
                        && spec.get("grid_um").is_some()
                        && spec.get("max_searches").is_some()
                });
                require(
                    (1.0..=50.0).contains(&grid) && (1..=1_000_000).contains(&searches) && keys_ok,
                    "INPUT",
                    "policy macro_blockage outside qualified bounds",
                )?;
                Some(Blockages::new(
                    &def,
                    &lef_blocks(&files.lef_text)?,
                    read_die(&files.def_text)?,
                    grid,
                    searches,
                )?)
            }
        };
        let place = at(&inputs.manifest, "placement")?;
        let base = inputs.document(at(place, "seed")?, 64 << 20)?;
        require(
            base.get("schema").and_then(Json::as_str) == Some("rustroad.placement-seed.v1"),
            "SCHEMA",
            "placement seed schema",
        )?;
        let dbu = at(at(&base, "source")?, "units_per_micron")?.float()?;
        super::trace("geometry: built");
        Ok(Geometry {
            i: inputs,
            rc,
            macros,
            def,
            blockages: RefCell::new(blockages),
            base,
            dbu,
            new: Vec::new(),
            new_index: HashMap::default(),
        })
    }

    /// Location of an instance pin, or of a top-level port when `instance` is None.
    pub(crate) fn pin(&self, instance: Option<&str>, pin: &str) -> Res<Point> {
        let scale = self.def.scale;
        let Some(instance) = instance else {
            let port = self.def.ports.get(pin);
            let placed = port.and_then(|p| p.placed);
            require(
                placed.is_some(),
                "PLACEMENT",
                format!("unplaced port {pin}"),
            )?;
            let (x, y) = placed.unwrap_or((0, 0));
            let (mut x, mut y) = (x as f64, y as f64);
            if let Some(l) = port.and_then(|p| p.layer) {
                x += (l[0] + l[2]) as f64 / 2.0;
                y += (l[1] + l[3]) as f64 / 2.0;
            }
            return Ok((x / scale, y / scale));
        };
        if let Some(&k) = self.new_index.get(instance) {
            return Ok(self.new[k].2);
        }
        let comp = self.def.comps.get(instance);
        require(
            comp.is_some(),
            "PLACEMENT",
            format!("unplaced instance {instance}"),
        )?;
        let &(master, ox, oy, orientation) = comp.ok_or_else(|| key_error(instance))?;
        let (w, h, pins) = self.macros.get(master).ok_or_else(|| key_error(master))?;
        let at_pin = pins.get(pin);
        require(
            at_pin.is_some(),
            "PLACEMENT",
            format!("pin without LEF geometry {master}/{pin}"),
        )?;
        let (px, py, _) = at_pin.ok_or_else(|| key_error(pin))?;
        let (dx, dy) = orient(orientation, *px, *py, *w, *h)?;
        Ok((ox as f64 / scale + dx, oy as f64 / scale + dy))
    }

    /// The location of a driver or load.
    pub(crate) fn at_row(&self, x: &Row) -> Res<Point> {
        self.pin(x.instance.as_deref(), x.place_pin())
    }

    /// Seed a new cell centred on `at` (um); legalization snaps it to a row.
    pub(crate) fn place(&mut self, instance: &str, cell: &str, at: Point) -> Res<()> {
        require(
            self.macros.contains_key(cell) && !self.new_index.contains_key(instance),
            "PLACEMENT",
            "new cell geometry",
        )?;
        self.new_index.insert(instance.to_string(), self.new.len());
        self.new.push((instance.to_string(), cell.to_string(), at));
        Ok(())
    }

    /// A centre (um) of `points` moved out of any macro containing it: the bottom- or
    /// top-edge projection with the shorter estimated tree to the points.
    pub(crate) fn outside(&self, at: Point, points: &[Point]) -> Res<Point> {
        let rects: Vec<(f64, f64, f64, f64)> = match self.blockages.borrow().as_ref() {
            None => return Ok(at),
            Some(b) => b.rects.clone(),
        };
        let (x, y) = at;
        for (lx, ly, ux, uy) in rects {
            if lx < x && x < ux && ly < y && y < uy {
                let options = [(x, ly - OUTSIDE_UM), (x, uy + OUTSIDE_UM)];
                let mut keys = Vec::with_capacity(2);
                for q in options {
                    let mut all = Vec::with_capacity(points.len() + 1);
                    all.push(q);
                    all.extend_from_slice(points);
                    keys.push((self.wire_pf(&all)?.1, q.1));
                }
                let second =
                    keys[1].0 < keys[0].0 || (keys[1].0 == keys[0].0 && keys[1].1 < keys[0].1);
                return Ok(options[usize::from(second)]);
            }
        }
        Ok(at)
    }

    /// Estimated wire capacitance (pF) and length (um) of a net over `points`, as the estimator
    /// models it.
    pub(crate) fn wire_pf(&self, points: &[Point]) -> Res<(f64, f64)> {
        require(
            points.len() >= 2,
            "INPUT",
            "cannot unpack non-iterable float object",
        )?;
        self.i.tick((points.len() * points.len()) as i64)?;
        let edges = mst(points);
        let mut lengths = Vec::with_capacity(edges.len());
        match self.blockages.borrow_mut().as_mut() {
            None => {
                for (a, b) in edges {
                    lengths.push(
                        (points[a].0 - points[b].0).abs() + (points[a].1 - points[b].1).abs(),
                    );
                }
            }
            Some(blockages) => {
                for (a, b) in edges {
                    lengths.push(blockages.length(points[a], points[b])?);
                }
            }
        }
        let length = fsum(lengths);
        let detour = at(&self.rc, "detour")?.float()?;
        let c = at(&self.rc, "c_pf_per_um")?.float()?;
        Ok((length * detour * c, length * detour))
    }

    /// Writes the seed of the next placement: the timed seed plus every new cell; with
    /// `clock_order`, also the timed clock leaf order and the regrouped tree partition.
    pub(crate) fn write_seed(&self, ops: &[Op], path: &Path, clock_order: bool) -> Res<Json> {
        let base_cells = at(&self.base, "cells")?
            .as_object()
            .ok_or_else(|| Failure::new("INPUT", "seed cells"))?;
        let mut cells: Vec<(String, Json)> = base_cells.to_vec();
        let mut known: HashSet<&str> = base_cells.iter().map(|(k, _)| k.as_str()).collect();
        for (name, cell, (x, y)) in &self.new {
            let (w, h, _) = self.macros.get(cell).ok_or_else(|| key_error(cell))?;
            require(
                !known.contains(name.as_str()),
                "PLACEMENT",
                format!("new cell already seeded: {name}"),
            )?;
            known.insert(name);
            let dbu = |v: f64| Json::Int(((v * self.dbu).round_ties_even() as i64).into());
            cells.push((
                name.clone(),
                Json::List(vec![dbu(x - w / 2.0), dbu(y - h / 2.0)]),
            ));
        }
        let seeded = cells.len();
        let mut source = at(&self.base, "source")?.clone();
        source.set("eco_cells", Json::Int(self.new.len() as i128));
        let mut seed = vec![
            ("schema", Json::str("rustroad.placement-seed.v1")),
            ("source", source),
            ("cells", Json::Object(cells)),
        ];
        let empty = Json::List(Vec::new());
        let sinks = self.base.get("clock_sinks").unwrap_or(&empty);
        let listed = sinks.as_list();
        let unique = listed.is_some_and(|items| {
            let mut seen = HashSet::default();
            items
                .iter()
                .all(|s| s.as_str().is_some_and(|name| seen.insert(name)))
        });
        require(unique, "SCHEMA", "seed clock sinks")?;
        let mut clock_sinks = 0usize;
        if clock_order && sinks.truthy() {
            clock_sinks = listed.map_or(0, <[Json]>::len);
            seed.push(("clock_sinks", sinks.clone()));
        }
        let none = Json::Object(Vec::new());
        let regroup = self.base.get("regroup").unwrap_or(&none);
        let partition = regroup.as_object();
        let valid = partition.is_some_and(|items| {
            items.iter().all(|(_, v)| {
                v.as_list().is_some_and(|sinks| {
                    sinks
                        .iter()
                        .all(|s| s.as_list().is_some_and(|pair| pair.len() == 2))
                })
            })
        });
        require(valid, "SCHEMA", "seed regroup partition")?;
        let mut regrouped = 0usize;
        if clock_order && regroup.truthy() {
            // The first chain stage inserted in front of a regrouped sink pin is that tree
            // net's sink now (later stages sit behind it).
            let mut first: HashMap<(&str, &str), (String, String)> = HashMap::default();
            for op in ops {
                if let Op::InsertBuffer {
                    cell,
                    instance,
                    sink,
                } = op
                {
                    let pins = &self.macros.get(cell).ok_or_else(|| key_error(cell))?.2;
                    let mut inputs = pins.iter().filter(|(_, pin)| pin.2 == "INPUT");
                    let input = inputs.next();
                    require(
                        input.is_some() && inputs.next().is_none(),
                        "PLACEMENT",
                        format!("chain cell input pin: {cell}"),
                    )?;
                    let input = input.map(|(n, _)| n.clone()).unwrap_or_default();
                    first
                        .entry((sink.0.as_str(), sink.1.as_str()))
                        .or_insert((instance.clone(), input));
                }
            }
            let mut sorted: Vec<&(String, Json)> = partition.unwrap_or(&[]).iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            regrouped = sorted.len();
            let map = sorted
                .into_iter()
                .map(|(b, sinks)| {
                    let sinks = sinks
                        .as_list()
                        .unwrap_or(&[])
                        .iter()
                        .map(|s| {
                            let pair = s.as_list().unwrap_or(&[]);
                            let key = (pair[0].as_str(), pair[1].as_str());
                            match key {
                                (Some(i), Some(p)) => match first.get(&(i, p)) {
                                    Some((inst, pin)) => {
                                        Json::List(vec![Json::str(inst), Json::str(pin)])
                                    }
                                    None => s.clone(),
                                },
                                _ => s.clone(),
                            }
                        })
                        .collect();
                    (b.clone(), Json::List(sinks))
                })
                .collect();
            seed.push(("regroup", Json::Object(map)));
        }
        std::fs::write(path, obj(seed).dumps(Some(1)) + "\n")?;
        Ok(obj(vec![
            ("new_cells", Json::Int(self.new.len() as i128)),
            ("seeded_cells", Json::Int(seeded as i128)),
            ("clock_sinks", Json::Int(clock_sinks as i128)),
            ("regrouped_buffers", Json::Int(regrouped as i128)),
        ]))
    }
}

/// `min((x['instance'] or '', x.get('pin') or x.get('port')) for x in part)`.
fn least_key<'r>(part: &[&'r Row]) -> Option<(&'r str, &'r str)> {
    part.iter().map(|x| x.key()).min()
}

fn mean(points: &[Point]) -> Point {
    let n = points.len() as f64;
    (
        fsum(points.iter().map(|p| p.0)) / n,
        fsum(points.iter().map(|p| p.1)) / n,
    )
}

/// `(max(xs)-min(xs), max(ys)-min(ys))`.
fn extent(points: &[Point]) -> (f64, f64) {
    let fold = |pick: fn(&Point) -> f64| {
        let first = pick(&points[0]);
        points.iter().fold((first, first), |(lo, hi), p| {
            (first_min(lo, pick(p)), first_max(hi, pick(p)))
        })
    };
    let (x, y) = (fold(|p| p.0), fold(|p| p.1));
    (x.1 - x.0, y.1 - y.0)
}

/// The order of a bisection along the longer side: indices by (coordinate, index), and the
/// first half's size.
fn bisect(points: &[Point]) -> (Vec<usize>, usize) {
    let (dx, dy) = extent(points);
    let along = |k: usize| if dx >= dy { points[k].0 } else { points[k].1 };
    let mut order: Vec<usize> = (0..points.len()).collect();
    order.sort_by(|&a, &b| {
        along(a)
            .partial_cmp(&along(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    (order, points.len() / 2)
}

fn round_json(v: f64, digits: usize) -> Json {
    Json::Float(py_round(v, digits))
}

fn strings(v: &Json, k: &str) -> Res<Vec<String>> {
    Ok(at(v, k)?
        .as_list()
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect())
}

fn drv_rows(report: &Report) -> Res<&[super::eco_inputs::DrvRow]> {
    Ok(&report
        .sections
        .as_ref()
        .and_then(|s| s.drv.as_ref())
        .ok_or_else(|| key_error("drv"))?
        .rows)
}

fn hold_fix(report: &Report) -> Res<&HoldFix> {
    report
        .sections
        .as_ref()
        .and_then(|s| s.hold_fix.as_ref())
        .ok_or_else(|| key_error("hold_fix"))
}

fn endpoint_rows(report: &Report) -> Res<&[super::eco_inputs::EndpointRow]> {
    Ok(&report
        .sections
        .as_ref()
        .and_then(|s| s.endpoints.as_ref())
        .ok_or_else(|| key_error("endpoints"))?
        .rows)
}

#[derive(Default)]
struct NetInfo {
    load: f64,
    slew_pins: HashSet<String>,
}

fn buffer_sinks(cell: &str, instance: &str, rows: &[&Row]) -> Op {
    Op::BufferSinks {
        cell: cell.to_string(),
        instance: instance.to_string(),
        sinks: rows.iter().filter_map(|x| x.sink()).collect(),
        ports: rows
            .iter()
            .filter(|x| x.instance.is_none())
            .map(|x| x.port.as_deref().unwrap_or("").to_string())
            .collect(),
    }
}

/// Wire-aware DRV repair on estimated-RC reports.
pub(crate) fn electrical(
    planner: &mut Planner<'_>,
    reports: &[Arc<Report>],
    geo: &mut Geometry<'_, '_>,
    hold_round: bool,
) -> Res<Json> {
    let cfg = at(planner.p, "electrical")?;
    let g = planner.g;
    const FIELDS: [&str; 9] = [
        "driver_cells",
        "sram_buffer",
        "sram_clock_buffer",
        "sram_masters",
        "load_fraction",
        "max_wire_um",
        "antenna_cells",
        "drv_margin",
        "hold_drv_margin",
    ];
    let keys: Vec<&str> = cfg
        .as_object()
        .unwrap_or(&[])
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    require(
        FIELDS.iter().all(|f| keys.contains(f))
            && keys
                .iter()
                .all(|k| FIELDS.contains(k) || *k == "cap_feedback" || *k == "clock_balance"),
        "POLICY",
        "electrical policy fields",
    )?;
    let fraction = json_number(at(cfg, "load_fraction")?, true)?;
    require(fraction <= 1.0, "POLICY", "load fraction")?;
    let margin = json_number(
        at(
            cfg,
            if hold_round {
                "hold_drv_margin"
            } else {
                "drv_margin"
            },
        )?,
        false,
    )?;
    let hold_margin = json_number(at(cfg, "hold_drv_margin")?, false)?;
    let drv_margin = json_number(at(cfg, "drv_margin")?, false)?;
    require(
        0.0 <= hold_margin && hold_margin <= drv_margin && drv_margin < 0.5,
        "POLICY",
        "DRV margins",
    )?;
    let max_wire = json_number(at(cfg, "max_wire_um")?, true)?;
    let mut drivers: Vec<(f64, String, f64)> = Vec::new();
    for cell in strings(cfg, "driver_cells")? {
        let (_, a, z) = planner.model(&cell)?;
        drivers.push((z.limit.unwrap_or(0.0), cell, a.cap.unwrap_or(0.0)));
    }
    drivers.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    let choose = |load: f64| -> Res<String> {
        drivers
            .iter()
            .find(|(limit, _, _)| limit * fraction >= load)
            .map(|(_, c, _)| c.clone())
            .ok_or_else(|| {
                Failure::new(
                    "CAPACITY",
                    format!("estimated load {load:.3} pF exceeds every selected driver"),
                )
            })
    };
    // Post-route capacitance feedback binds the hold round to an extracted witness; that
    // variant stays with the reference planner.
    require(
        !(hold_round && cfg.get("cap_feedback").is_some()),
        "UNSUPPORTED",
        "electrical.cap_feedback is planned by the reference planner (native_ecos.py)",
    )?;
    super::trace("electrical: start");
    let sram_masters = strings(cfg, "sram_masters")?;
    let antenna_cells = strings(cfg, "antenna_cells")?;
    let sram_buffer = text(cfg, "sram_buffer")?.to_string();
    let sram_clock_buffer = text(cfg, "sram_clock_buffer")?.to_string();
    let is_sram = |x: &Row| {
        x.cell
            .as_deref()
            .is_some_and(|c| sram_masters.iter().any(|m| m == c))
    };

    let mut nets: HashMap<String, NetInfo> = HashMap::default();
    for env in reports {
        for r in drv_rows(env)? {
            // The next placement moves pins: a transition or load within the margin of its
            // limit is repaired like a violation.
            let near = matches!(r.kind, DrvKind::Capacitance | DrvKind::Transition) && {
                let (Some(value), Some(limit)) = (r.value, r.limit) else {
                    return Err(Failure::new("INPUT", "DRV row without value or limit"));
                };
                value > limit * (1.0 - margin)
            };
            if r.slack.unwrap_or(0.0) >= 0.0 && !near {
                continue;
            }
            let info = nets.entry(r.net.clone()).or_default();
            match r.kind {
                DrvKind::Capacitance => {
                    let value = r
                        .value
                        .ok_or_else(|| Failure::new("INPUT", "DRV row without value"))?;
                    info.load = first_max(first_max(info.load, value), 0.0);
                }
                DrvKind::Transition => {
                    info.slew_pins.insert(r.pin.clone());
                }
                _ => {}
            }
            planner.i.tick(1)?;
        }
    }
    // The STA DRV rows omit input-port driving cells. Check every nonclock port net with the
    // same estimate and margin; a near-limit one is isolated at its port below.
    let mut labels: Vec<(&str, i64)> = g
        .net_bits
        .iter()
        .filter_map(|(n, b)| b.net().map(|bit| (n.as_str(), bit)))
        .collect();
    labels.sort_unstable();
    for (label, bit) in labels {
        let Some(d) = g.drivers.get(bit) else {
            continue;
        };
        let limit = d.limit.filter(|l| *l != 0.0);
        if d.instance.is_some()
            || d.pin.as_deref() != Some(label)
            || planner.clocks.contains(&bit)
            || limit.is_none()
        {
            continue;
        }
        let limit = limit.unwrap_or(0.0);
        let loads = g.loads_of(bit);
        if loads.is_empty() {
            continue;
        }
        planner.i.tick(loads.len() as i64)?;
        let mut points = vec![geo.pin(None, label)?];
        for x in loads {
            points.push(geo.at_row(x)?);
        }
        let (wire, length) = geo.wire_pf(&points)?;
        let load = wire + caps(loads.iter())?;
        if load <= limit * (1.0 - margin) {
            continue;
        }
        let info = nets.entry(label.to_string()).or_default();
        info.load = first_max(info.load, load);
        planner.checks.push(obj(vec![
            ("net", Json::str(label)),
            ("action", Json::str("port_drv")),
            ("driver_cell", Json::str(d.cell.as_deref().unwrap_or(""))),
            ("estimated_load_pf", round_json(load, 6)),
            ("limit_pf", Json::Float(limit)),
            ("margin", Json::Float(margin)),
            ("estimated_wire_um", round_json(length, 1)),
        ]));
    }
    planner.feedback_nets = HashSet::default();
    let mut trees: BTreeMap<(String, i64), f64> = BTreeMap::new();
    let mut count = 0usize;
    let mut names: Vec<String> = nets.keys().cloned().collect();
    names.sort_unstable();
    let primary_prefix = Planner::prefix_of("primary-clock");
    for name in &names {
        let info = &nets[name];
        let Some(bit) = g.net_bit(name).and_then(super::py::Bit::net) else {
            planner.excluded.push(obj(vec![
                ("net", Json::str(name)),
                (
                    "reason",
                    Json::str("not an editable net of the pre-CTS netlist"),
                ),
            ]));
            continue;
        };
        let d = g.drivers.get(bit);
        let loads = g.loads_of(bit);
        if planner.clocks.contains(&bit) {
            // Only a primary-clock ECO branch (never the CTS tree) may buffer its violating
            // SRAM clock pins, each at the pin.
            let mut fixed: Vec<Json> = Vec::new();
            let branch =
                d.is_some_and(|d| numbered(d.instance.as_deref().unwrap_or(""), &primary_prefix));
            if branch {
                for x in loads {
                    if x.instance.is_some() && is_sram(x) && info.slew_pins.contains(&x.label()) {
                        let inst = format!("{}_{count}", planner.prefix);
                        count += 1;
                        planner.append(buffer_sinks(&sram_clock_buffer, &inst, &[x]))?;
                        geo.place(&inst, &sram_clock_buffer, geo.at_row(x)?)?;
                        fixed.push(Json::str(x.pin.as_deref().unwrap_or("")));
                        planner.weigh(format!("{inst}_n"));
                    }
                }
            }
            if fixed.is_empty() {
                planner.excluded.push(obj(vec![
                    ("net", Json::str(name)),
                    ("reason", Json::str("explicit clock/CTS scope")),
                ]));
            } else {
                planner.checks.push(obj(vec![
                    ("net", Json::str(name)),
                    ("action", Json::str("sram_clock_pin_buffer")),
                    ("pins", Json::List(fixed)),
                    ("cell", Json::str(&sram_clock_buffer)),
                ]));
            }
            continue;
        }
        require(
            d.is_some() && !loads.is_empty(),
            "NETLIST",
            format!("violating net lacks driver or load: {name}"),
        )?;
        let Some(d) = d else { continue };
        let label = d.label();
        let tree = tree_member(d.instance.as_deref().unwrap_or(""));
        let origin = geo.pin(d.instance.as_deref(), d.pin.as_deref().unwrap_or(""))?;
        let mut here = vec![origin];
        for x in loads {
            here.push(geo.at_row(x)?);
        }
        let (wire, mut length) = geo.wire_pf(&here)?;
        let mut load = first_max(info.load, wire + caps(loads.iter())?);
        if let Some((tree, depth)) = tree {
            let depth: i64 = depth
                .parse()
                .map_err(|_| Failure::new("INPUT", "tree depth out of range"))?;
            let slot = trees.entry((tree.to_string(), depth)).or_insert(0.0);
            *slot = first_max(*slot, load);
            continue;
        }
        let mut moved: HashSet<String> = HashSet::default();
        let mut added: Vec<Row> = Vec::new();
        for x in loads {
            if x.instance.is_none() {
                continue;
            }
            let pin = x.label();
            if is_sram(x) && info.slew_pins.contains(&pin) {
                let inst = format!("{}_{count}", planner.prefix);
                count += 1;
                let (b, a, _) = planner.model(&sram_buffer)?;
                let input: Arc<str> = Arc::from(b.input.as_str());
                planner.append(buffer_sinks(&sram_buffer, &inst, &[x]))?;
                geo.place(&inst, &sram_buffer, geo.at_row(x)?)?;
                moved.insert(pin.clone());
                planner.weigh(format!("{inst}_n"));
                planner.repaired.insert(name.clone());
                // The new pin buffer is now a load of this net, at the SRAM pin.
                added.push(Row {
                    instance: Some(Arc::from(inst.as_str())),
                    pin: Some(input),
                    port: None,
                    cell: Some(Arc::from(sram_buffer.as_str())),
                    cap: a.cap,
                    limit: None,
                    fanout: 0.0,
                    max_fanout: f64::INFINITY,
                    clock: false,
                });
                planner.checks.push(obj(vec![
                    ("net", Json::str(name)),
                    ("action", Json::str("sram_pin_buffer")),
                    ("pin", Json::Str(pin)),
                    ("cell", Json::str(&sram_buffer)),
                ]));
            }
        }
        let mut rest: Vec<&Row> = loads
            .iter()
            .filter(|x| {
                !(x.instance.is_some() && moved.contains(&x.label()))
                    && !x
                        .cell
                        .as_deref()
                        .is_some_and(|c| antenna_cells.iter().any(|a| a == c))
            })
            .collect();
        rest.extend(added.iter());
        let mut other_slew = info.slew_pins.iter().any(|p| !moved.contains(p));
        if !added.is_empty() {
            let mut pts = vec![here[0]];
            for x in &rest {
                pts.push(geo.at_row(x)?);
            }
            let (wire, len) = geo.wire_pf(&pts)?;
            length = len;
            load = first_max(load, wire + caps(rest.iter().copied())?);
            // Isolate the driver too when the remaining net (to the pin buffers) exceeds its
            // own limit.
            let limit = d.limit.filter(|l| *l != 0.0).unwrap_or(f64::INFINITY);
            other_slew = other_slew || load > limit * fraction;
        }
        if rest.is_empty() || !(info.load > 0.0 || other_slew) {
            continue;
        }
        let mut groups: Vec<Vec<&Row>> = vec![rest.clone()];
        if length > max_wire && rest.len() > 1 {
            // Recursive bisection along the longer side until each cluster's own wire fits.
            let mut pending = vec![rest.clone()];
            let mut done: Vec<Vec<&Row>> = Vec::new();
            while let Some(part) = pending.pop() {
                planner.i.tick(part.len() as i64)?;
                let mut pts = Vec::with_capacity(part.len());
                for x in &part {
                    pts.push(geo.at_row(x)?);
                }
                if part.len() == 1 || geo.wire_pf(&pts)?.1 <= max_wire / 2.0 {
                    done.push(part);
                    continue;
                }
                let (order, half) = bisect(&pts);
                pending.push(order[..half].iter().map(|&k| part[k]).collect());
                pending.push(order[half..].iter().map(|&k| part[k]).collect());
            }
            require(done.len() <= 64, "BUDGET", "cluster count of one net")?;
            done.sort_by(|a, b| least_key(a).cmp(&least_key(b)));
            groups = done;
        }
        let single = groups.len() == 1;
        let mut roots: Vec<(String, String, Point)> = Vec::new();
        for part in &groups {
            let mut pts = Vec::with_capacity(part.len());
            for x in part {
                pts.push(geo.at_row(x)?);
            }
            let at_point = if single {
                here[0]
            } else {
                geo.outside(mean(&pts), &pts)?
            };
            let mut all = vec![at_point];
            all.extend_from_slice(&pts);
            let (cw, _) = geo.wire_pf(&all)?;
            let part_load = cw + caps(part.iter().copied())?;
            let cell = choose(part_load)?;
            let inst = format!("{}_{count}", planner.prefix);
            count += 1;
            planner.append(buffer_sinks(&cell, &inst, part))?;
            geo.place(&inst, &cell, at_point)?;
            roots.push((inst, cell, at_point));
        }
        let clustered = roots.len() > 1;
        let leaves = roots.len();
        let input_cap = |planner: &Planner<'_>, cell: &str| -> Res<f64> {
            Ok(planner.model(cell)?.1.cap.unwrap_or(0.0))
        };
        let input_pin = |planner: &Planner<'_>, cell: &str| -> Res<String> {
            planner
                .buffer(cell)
                .map(|b: &Buffer| b.input.clone())
                .ok_or_else(|| key_error(cell))
        };
        // The trunk over the cluster buffers obeys the same wire bound: while the driver's
        // tour of them is too long, bisect them again and drive each group from a buffer at
        // its centre (one group when bisection leaves them all apart), so the count strictly
        // falls.
        loop {
            if roots.len() <= 1 {
                break;
            }
            let mut tour = vec![here[0]];
            tour.extend(roots.iter().map(|r| r.2));
            if geo.wire_pf(&tour)?.1 <= max_wire {
                break;
            }
            let mut pending: Vec<Vec<usize>> = vec![(0..roots.len()).collect()];
            let mut done: Vec<Vec<usize>> = Vec::new();
            while let Some(part) = pending.pop() {
                planner.i.tick(part.len() as i64)?;
                let pts: Vec<Point> = part.iter().map(|&k| roots[k].2).collect();
                if part.len() == 1 || geo.wire_pf(&pts)?.1 <= max_wire / 2.0 {
                    done.push(part);
                    continue;
                }
                let (order, half) = bisect(&pts);
                pending.push(order[..half].iter().map(|&k| part[k]).collect());
                pending.push(order[half..].iter().map(|&k| part[k]).collect());
            }
            if done.len() >= roots.len() {
                done = vec![(0..roots.len()).collect()];
            }
            done.sort_by(|a, b| {
                let low = |part: &Vec<usize>| part.iter().map(|&k| roots[k].0.as_str()).min();
                low(a).cmp(&low(b))
            });
            let mut level: Vec<(String, String, Point)> = Vec::new();
            for part in done {
                if part.len() == 1 {
                    level.push(roots[part[0]].clone());
                    continue;
                }
                let pts: Vec<Point> = part.iter().map(|&k| roots[k].2).collect();
                let at_point = geo.outside(mean(&pts), &pts)?;
                let mut all = vec![at_point];
                all.extend_from_slice(&pts);
                let (cw, _) = geo.wire_pf(&all)?;
                let mut sum = Vec::with_capacity(part.len());
                for &k in &part {
                    sum.push(input_cap(planner, &roots[k].1)?);
                }
                let cell = choose(cw + fsum(sum))?;
                let inst = format!("{}_{count}", planner.prefix);
                count += 1;
                let mut sinks = Vec::with_capacity(part.len());
                for &k in &part {
                    sinks.push((roots[k].0.clone(), input_pin(planner, &roots[k].1)?));
                }
                planner.append(Op::BufferSinks {
                    cell: cell.clone(),
                    instance: inst.clone(),
                    sinks,
                    ports: Vec::new(),
                })?;
                geo.place(&inst, &cell, at_point)?;
                level.push((inst, cell, at_point));
            }
            roots = level;
        }
        if clustered {
            let mut all = vec![here[0]];
            all.extend(roots.iter().map(|r| r.2));
            let (cw, _) = geo.wire_pf(&all)?;
            let mut sum = Vec::with_capacity(roots.len());
            for r in &roots {
                sum.push(input_cap(planner, &r.1)?);
            }
            let cell = choose(cw + fsum(sum))?;
            let inst = format!("{}_{count}", planner.prefix);
            count += 1;
            let mut sinks = Vec::with_capacity(roots.len());
            for r in &roots {
                sinks.push((r.0.clone(), input_pin(planner, &r.1)?));
            }
            planner.append(Op::BufferSinks {
                cell: cell.clone(),
                instance: inst.clone(),
                sinks,
                ports: Vec::new(),
            })?;
            geo.place(&inst, &cell, here[0])?;
        }
        planner.weigh(name.clone());
        planner.repaired.insert(name.clone());
        planner.checks.push(obj(vec![
            ("net", Json::str(name)),
            (
                "action",
                Json::str(if clustered { "cluster" } else { "isolate" }),
            ),
            ("driver", Json::Str(label)),
            ("estimated_load_pf", round_json(load, 6)),
            ("estimated_wire_um", round_json(length, 1)),
            ("clusters", Json::Int(leaves as i128)),
            (
                "trunk_buffers",
                Json::Int(if clustered { roots.len() as i128 } else { 0 }),
            ),
        ]));
    }
    // A regrouped tree's buffers follow their sinks, which later ECOs and CTS move: the net
    // feeding a tree's top level is sized for a tour of the whole tree's sink box (1.5 x
    // half-perimeter, driver included), not for today's buffer positions.
    let bit_names = g.bit_names();
    let mut members: BTreeMap<&str, Vec<(i64, &str)>> = BTreeMap::new();
    for (inst, _) in g.cells() {
        if let Some((tree, depth)) = tree_member(inst) {
            let depth: i64 = depth
                .parse()
                .map_err(|_| Failure::new("INPUT", "tree depth out of range"))?;
            members.entry(tree).or_default().push((depth, inst));
        }
    }
    for (tree, levels) in &members {
        let top = levels.iter().map(|l| l.0).max().unwrap_or(0);
        let mut bits: Vec<Option<i64>> = Vec::new();
        for &(depth, i) in levels {
            if depth != top {
                continue;
            }
            let Some(b) = g.cell_type(i).and_then(|t| planner.buffer(t)) else {
                continue;
            };
            let bit = g.pin_bit(i, &b.input);
            if !bits.contains(&bit) {
                bits.push(bit);
            }
        }
        if bits.len() != 1 || bits[0].is_none() {
            continue;
        }
        let bit = bits[0].unwrap_or(0);
        let d = g.drivers.get(bit);
        let name = bit_names.get(&bit).copied();
        let (Some(d), Some(name)) = (d, name) else {
            continue;
        };
        if planner.clocks.contains(&bit) || nets.contains_key(name) {
            continue;
        }
        let mut leaves: Vec<&Row> = Vec::new();
        for &(depth, i) in levels {
            if depth != 0 {
                continue;
            }
            let kind = g.cell_type(i).unwrap_or("");
            let b = planner.buffer(kind).ok_or_else(|| key_error(kind))?;
            if let Some(out) = g.pin_bit(i, &b.output) {
                leaves.extend(g.loads_of(out));
            }
        }
        let mut pts = vec![geo.pin(d.instance.as_deref(), d.pin.as_deref().unwrap_or(""))?];
        for x in &leaves {
            pts.push(geo.at_row(x)?);
        }
        planner.i.tick(pts.len() as i64)?;
        // max(x)-min(x)+max(y)-min(y), left to right.
        let span = {
            let xs = pts.iter().map(|p| p.0);
            let ys = pts.iter().map(|p| p.1);
            let max_x = xs.clone().reduce(first_max).unwrap_or(0.0);
            let min_x = xs.reduce(first_min).unwrap_or(0.0);
            let max_y = ys.clone().reduce(first_max).unwrap_or(0.0);
            let min_y = ys.reduce(first_min).unwrap_or(0.0);
            max_x - min_x + max_y - min_y
        };
        let loads = g.loads_of(bit);
        let detour = at(&geo.rc, "detour")?.float()?;
        let c_um = at(&geo.rc, "c_pf_per_um")?.float()?;
        let load = 1.5 * span * detour * c_um + caps(loads.iter())?;
        let limit = d.limit.filter(|l| *l != 0.0).unwrap_or(f64::INFINITY);
        if load <= limit * fraction {
            continue;
        }
        let cell = choose(load)?;
        let inst = format!("{}_{count}", planner.prefix);
        count += 1;
        let all: Vec<&Row> = loads.iter().collect();
        planner.append(buffer_sinks(&cell, &inst, &all))?;
        geo.place(&inst, &cell, pts[0])?;
        planner.weigh(name.to_string());
        planner.repaired.insert(name.to_string());
        planner.checks.push(obj(vec![
            ("net", Json::str(name)),
            ("action", Json::str("tree_feed")),
            ("tree", Json::str(*tree)),
            ("driver", Json::Str(d.label())),
            ("bound_load_pf", round_json(load, 6)),
            ("sink_box_um", round_json(span, 1)),
            ("cell", Json::Str(cell)),
        ]));
    }
    let mut level_members: HashMap<(&str, &str), Vec<&str>> = HashMap::default();
    for (i, _) in g.cells() {
        let mut parts = i.rsplitn(3, '_');
        let (Some(last), Some(middle), Some(head)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if digits(middle) && digits(last) {
            level_members.entry((head, middle)).or_default().push(i);
        }
    }
    for ((tree, depth), &load) in &trees {
        let cell = choose(load)?;
        let depth_text = depth.to_string();
        let mut level: Vec<&str> = level_members
            .get(&(tree.as_str(), depth_text.as_str()))
            .cloned()
            .unwrap_or_default();
        level.sort_unstable();
        require(
            !level.is_empty(),
            "NETLIST",
            format!("tree level has no buffers: {tree}"),
        )?;
        for inst in &level {
            planner.repaired.insert(format!("{inst}_n"));
        }
        for inst in &level {
            let current = g.cell_type(inst).unwrap_or("");
            let own = planner.model(current)?.2.limit;
            let wanted = planner.model(&cell)?.2.limit;
            if own < wanted {
                require(
                    (planner.ops.len() as i64) < planner.i.args.max_operations,
                    "BUDGET",
                    "ECO operation budget exceeded",
                )?;
                planner.ops.push(Op::ResizeCell {
                    instance: inst.to_string(),
                    cell: cell.clone(),
                });
            }
        }
        planner.checks.push(obj(vec![
            ("tree", Json::str(tree)),
            ("depth", Json::Int((*depth).into())),
            ("action", Json::str("resize_level")),
            ("cell", Json::Str(cell)),
            ("estimated_load_pf", round_json(load, 6)),
            ("buffers", Json::Int(level.len() as i128)),
        ]));
    }
    Ok(obj(vec![
        ("electrical_nets", Json::Int(nets.len() as i128)),
        ("resized_levels", Json::Int(trees.len() as i128)),
        ("cap_feedback_nets", Json::Int(0)),
        (
            "basis",
            Json::str("placement RC estimate; re-time after placement"),
        ),
    ]))
}

/// `sum(x['cap'] for x in rows)`.
fn caps<'r>(rows: impl Iterator<Item = &'r Row>) -> Res<f64> {
    let mut values = Vec::new();
    for x in rows {
        values.push(x.cap.ok_or_else(|| {
            Failure::new(
                "INPUT",
                "unsupported operand type(s) for +: 'float' and 'NoneType'",
            )
        })?);
    }
    Ok(fsum(values))
}

/// The corner libraries of the manifest's pin tables, in path order.
fn corner_libraries(inputs: &Inputs<'_>, corner: &str) -> Res<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for x in at(inputs.corner(corner)?, "pin_tables")?
        .as_list()
        .unwrap_or(&[])
    {
        let p = inputs.file64(at(x, "library")?)?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// The libraries of every corner with the wanted cells, in the order of the corners and of the
/// library paths; the files are read beside each other.
fn corner_library_cells(
    inputs: &Inputs<'_>,
    corners: &[String],
    wanted: &[&str],
) -> Res<Vec<(String, Library)>> {
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    for corner in corners {
        for source in corner_libraries(inputs, corner)? {
            sources.push((corner.clone(), source));
        }
    }
    let read: Vec<Res<Library>> = std::thread::scope(|scope| {
        let handles: Vec<_> = sources
            .iter()
            .map(|(_, source)| scope.spawn(move || Library::read(source, Some(wanted))))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(Failure::new("INTERNAL", "Liberty reader")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(sources.len());
    for ((corner, _), lib) in sources.into_iter().zip(read) {
        out.push((corner, lib?));
    }
    Ok(out)
}

/// The single input and output pin of an identity cell, whose output function is its input.
fn identity_pins<'g>(cell: &'g Group, what: &str) -> Res<(&'g Group, &'g Group)> {
    let pins: Vec<&Group> = cell.sub("pin").collect();
    let a: Vec<&&Group> = pins
        .iter()
        .filter(|p| p.attr("direction") == Some("input"))
        .collect();
    let z: Vec<&&Group> = pins
        .iter()
        .filter(|p| p.attr("direction") == Some("output"))
        .collect();
    let strip = |s: &str| {
        let blank = |c: char| c.is_ascii() && crate::estimate::space(c as u8);
        s.trim_matches(blank)
            .trim_matches(['(', ')'])
            .trim_matches(blank)
            .to_string()
    };
    let ok = a.len() == 1
        && z.len() == 1
        && z[0].attr("function").map(strip).as_deref() == Some(a[0].name());
    require(ok, "LIBERTY", what)?;
    Ok((a[0], z[0]))
}

fn table_of(arc: &Group, kind: &str, lib: &Library) -> Res<Table> {
    let group = arc
        .sub(kind)
        .next()
        .ok_or_else(|| Failure::new("LIBERTY", "list index out of range"))?;
    Table::new(group, lib)
}

/// The delay bounds of one delay cell in one corner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bound {
    min: f64,
    max: f64,
    chain: f64,
    area: f64,
}

/// Delay cell -> corner -> bounds, in the order of the policy and of the corners.
pub(crate) type Bounds = Vec<(String, Vec<(String, Bound)>)>;

fn bounds_json(bounds: &Bounds) -> Json {
    Json::Object(
        bounds
            .iter()
            .map(|(name, per)| {
                let corners = per
                    .iter()
                    .map(|(c, b)| {
                        (
                            c.clone(),
                            obj(vec![
                                ("min", Json::Float(b.min)),
                                ("max", Json::Float(b.max)),
                                ("chain", Json::Float(b.chain)),
                                ("area", Json::Float(b.area)),
                            ]),
                        )
                    })
                    .collect();
                (name.clone(), Json::Object(corners))
            })
            .collect(),
    )
}

/// `delay_bounds(inputs, policy, corners)`: table extrema of the selected delay cells.
fn delay_bounds(inputs: &Inputs<'_>, policy: &Json, corners: &[String]) -> Res<Bounds> {
    inputs.tool("liberty_reader")?;
    let mut names: Vec<String> = Vec::new();
    for n in strings(policy, "delay_cells")? {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    require(
        (1..=16).contains(&names.len()),
        "POLICY",
        "delay cell selection budget",
    )?;
    // A set orders several names arbitrarily in the reference; one name is the qualified case.
    let wanted: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut output: Bounds = names.iter().map(|n| (n.clone(), Vec::new())).collect();
    {
        for (corner, lib) in &corner_library_cells(inputs, corners, &wanted)? {
            for (name, per) in &mut output {
                let Some(cell) = lib.cell(name) else { continue };
                require(
                    !per.iter().any(|(c, _)| c == corner),
                    "LIBERTY",
                    "duplicate selected delay master",
                )?;
                let (a, z) = identity_pins(cell, "delay cell is not a single-input identity")?;
                let arcs: Vec<&Group> = z.sub("timing").collect();
                require(
                    arcs.len() == 1
                        && arcs[0].attr("related_pin") == Some(a.name())
                        && arcs[0].attr("timing_sense") == Some("positive_unate"),
                    "LIBERTY",
                    "delay arc identity mismatch",
                )?;
                let tables = [
                    table_of(arcs[0], "cell_rise", lib)?,
                    table_of(arcs[0], "cell_fall", lib)?,
                ];
                // A chain stage that drives only the next stage (or one endpoint pin) sees at
                // most its own input pin plus CHAIN_WIRE_PF. Delay rises with load, so the
                // table column at the first load index at or above that bound, maximized over
                // every input slew row, bounds the stage. Without such a column it is the
                // table max.
                let cap = a
                    .attr("capacitance")
                    .ok_or_else(|| Failure::new("LIBERTY", "pin capacitance missing"))?;
                let cap_unit = lib.cap_unit_pf()?;
                let light = liberty::float(cap)? * cap_unit + CHAIN_WIRE_PF;
                let all = |t: &Table, pick: fn(f64, f64) -> f64| {
                    t.values
                        .iter()
                        .filter_map(|row| row.iter().copied().reduce(pick))
                        .reduce(pick)
                        .ok_or_else(|| Failure::new("LIBERTY", "empty delay table"))
                };
                let mut light_max = Vec::with_capacity(2);
                for t in &tables {
                    require(t.slew_by_load(), "LIBERTY", "delay table axes")?;
                    let column = t.index_2.iter().position(|v| v * cap_unit >= light);
                    light_max.push(match column {
                        None => all(t, first_max)?,
                        Some(j) => t
                            .values
                            .iter()
                            .map(|row| row[j])
                            .reduce(first_max)
                            .ok_or_else(|| Failure::new("LIBERTY", "empty delay table"))?,
                    });
                }
                let unit = lib.time_unit_ns()?;
                let area = cell
                    .attr("area")
                    .ok_or_else(|| Failure::new("LIBERTY", "cell area missing"))?;
                per.push((
                    corner.clone(),
                    Bound {
                        min: first_min(all(&tables[0], first_min)?, all(&tables[1], first_min)?)
                            * unit,
                        max: first_max(all(&tables[0], first_max)?, all(&tables[1], first_max)?)
                            * unit,
                        chain: first_max(light_max[0], light_max[1]) * unit,
                        area: number(liberty::float(area)?, false)?,
                    },
                ));
            }
        }
    }
    require(
        output.iter().all(|(_, per)| {
            per.len() == corners.len() && corners.iter().all(|c| per.iter().any(|(k, _)| k == c))
        }),
        "LIBERTY",
        "missing delay model in a corner",
    )?;
    Ok(output)
}

fn bound<'b>(b: &'b [(String, Bound)], corner: &str) -> Res<&'b Bound> {
    b.iter()
        .find(|(c, _)| c == corner)
        .map(|(_, v)| v)
        .ok_or_else(|| key_error(corner))
}

/// Upper bound (ns) on the setup cost of `stages` delay stages on one path: each drives a light
/// load, except a launch chain's last stage, which drives the original sinks and is bounded by
/// the whole table.
fn chain_setup(
    b: &[(String, Bound)],
    corners: &[String],
    late: f64,
    stages: i64,
    heavy_last: bool,
) -> Res<f64> {
    let mut worst: Option<f64> = None;
    for c in corners {
        let x = bound(b, c)?;
        let heavy = if heavy_last && stages != 0 {
            x.max - x.chain
        } else {
            0.0
        };
        let v = stages as f64 * x.chain + heavy;
        worst = Some(worst.map_or(v, |w| first_max(w, v)));
    }
    Ok(late * worst.unwrap_or(0.0))
}

/// Typical delay (ns) of one stage of a chain of `cell`, per corner: each stage drives the next
/// stage's input plus `wire_pf`, with the input slew the stage itself produces at that load
/// (fixed point of the transition tables), worse of rise and fall.
fn balance_stage_ns(
    inputs: &Inputs<'_>,
    cell: &str,
    corners: &[String],
    wire_pf: f64,
) -> Res<Vec<(String, f64)>> {
    inputs.tool("liberty_reader")?;
    let mut out: Vec<(String, f64)> = Vec::new();
    {
        for (corner, lib) in &corner_library_cells(inputs, corners, &[cell])? {
            let Some(group) = lib.cell(cell) else {
                continue;
            };
            require(
                !out.iter().any(|(c, _)| c == corner),
                "LIBERTY",
                "duplicate clock balance master",
            )?;
            let (a, z) = identity_pins(group, "clock balance cell is not a single-input identity")?;
            let arcs: Vec<&Group> = z.sub("timing").collect();
            require(
                arcs.len() == 1 && arcs[0].attr("timing_sense") == Some("positive_unate"),
                "LIBERTY",
                "clock balance arc",
            )?;
            let rise = table_of(arcs[0], "cell_rise", lib)?;
            let fall = table_of(arcs[0], "cell_fall", lib)?;
            let rise_slew = table_of(arcs[0], "rise_transition", lib)?;
            let fall_slew = table_of(arcs[0], "fall_transition", lib)?;
            let (tu, cu) = (lib.time_unit_ns()?, lib.cap_unit_pf()?);
            let cap = a
                .attr("capacitance")
                .ok_or_else(|| Failure::new("LIBERTY", "pin capacitance missing"))?;
            let load = (liberty::float(cap)? * cu + wire_pf) / cu;
            let mut slew = *rise_slew
                .index_1
                .first()
                .ok_or_else(|| Failure::new("LIBERTY", "list index out of range"))?;
            for _ in 0..8 {
                slew = first_max(rise_slew.lookup(slew, load)?, fall_slew.lookup(slew, load)?);
            }
            let delay = first_max(rise.lookup(slew, load)?, fall.lookup(slew, load)?) * tu;
            out.push((corner.clone(), delay));
        }
    }
    require(
        out.len() == corners.len() && out.iter().all(|(_, v)| *v > 0.0),
        "LIBERTY",
        "missing clock balance model in a corner",
    )?;
    Ok(out)
}

fn corner_map(items: impl IntoIterator<Item = (String, f64)>, digits: Option<usize>) -> Json {
    Json::Object(
        items
            .into_iter()
            .map(|(c, v)| {
                (
                    c,
                    Json::Float(match digits {
                        Some(d) => py_round(v, d),
                        None => v,
                    }),
                )
            })
            .collect(),
    )
}

/// Delay the clocks of ungated launch flip-flops towards the gated tree: each primary branch's
/// flip-flops that launch a listed hold deficit get one shared chain of the balance cell on
/// their clock pins, as long as the group's worst launch deficit plus the margin and no longer
/// than the smallest hold slack of the group's own pins allows, in every corner.
pub(crate) fn clock_balance(
    planner: &mut Planner<'_>,
    holds: &[Arc<Report>],
    setups: &[Arc<Report>],
    corners: &[String],
    geo: &mut Geometry<'_, '_>,
) -> Res<Json> {
    let cfg = at(at(planner.p, "electrical")?, "clock_balance")?;
    let g = planner.g;
    let fields = ["cell", "max_stages", "margin_ns", "wire_pf"];
    require(
        cfg.as_object()
            .is_some_and(|items| items.len() == 4 && fields.iter().all(|f| cfg.get(f).is_some())),
        "POLICY",
        "clock balance fields",
    )?;
    let cell = text(cfg, "cell")?.to_string();
    require(
        strings(planner.p, "delay_cells")?.contains(&cell) && geo.macros.contains_key(&cell),
        "POLICY",
        "clock balance cell must be a selected delay cell",
    )?;
    let cap = at(cfg, "max_stages")?.int()?;
    require((1..=64).contains(&cap), "POLICY", "clock balance stage cap")?;
    let margin = json_number(at(cfg, "margin_ns")?, false)?;
    let wire = json_number(at(cfg, "wire_pf")?, false)?;
    let target = json_number(at(planner.p, "hold_margin_ns")?, false)?;
    require(
        (0.0..1.0).contains(&margin) && (0.0..0.05).contains(&wire),
        "POLICY",
        "clock balance margin/wire bounds",
    )?;
    let hold = hold_fix(&holds[0])?;
    for x in holds {
        require(
            Arc::ptr_eq(x, &holds[0]) || hold_fix(x)? == hold,
            "REPORT",
            "inconsistent merged hold reports",
        )?;
    }
    super::trace("clock balance: start");
    let stage = balance_stage_ns(planner.i, &cell, corners, wire)?;
    super::trace("clock balance: stage delays");
    let stage_of = |c: &str| -> Res<f64> {
        stage
            .iter()
            .find(|(k, _)| k == c)
            .map(|(_, v)| *v)
            .ok_or_else(|| key_error(c))
    };
    let mut need: HashMap<(&str, &str), f64> = HashMap::default();
    for co in &hold.corners {
        for d in &co.endpoints {
            planner.i.tick(1)?;
            let st = d.stages();
            let deficit = target - d.hold_slack_ns;
            if st
                .first()
                .is_some_and(|s| s.arc.as_deref() == Some("launch"))
                && deficit > 0.0
            {
                let slot = need
                    .entry((d.startpoint.as_str(), co.corner.as_str()))
                    .or_insert(0.0);
                *slot = first_max(*slot, deficit);
            }
        }
    }
    let mut allow: HashMap<(&str, &str), f64> = HashMap::default();
    let mut kinds: HashMap<&str, HashSet<&str>> = HashMap::default();
    for (env, c) in setups.iter().zip(corners) {
        for r in endpoint_rows(env)? {
            let Some((inst, _)) = r.endpoint.rsplit_once('/') else {
                continue;
            };
            kinds.entry(inst).or_default().insert(r.kind.as_str());
            if let Some(slack) = r.hold_slack_ns {
                let slot = allow.entry((inst, c.as_str())).or_insert(f64::INFINITY);
                *slot = first_min(*slot, slack - target);
            }
        }
    }
    let names = g.bit_names();
    let branch = Planner::prefix_of("primary-clock");
    let mut done: Vec<(i64, usize)> = Vec::new();
    let mut count = 0usize;
    let mut order: Vec<&(i64, Row)> = g.drivers.items.iter().collect();
    order.sort_by_key(|(bit, _)| *bit);
    for (bit, d) in order {
        if !numbered(d.instance.as_deref().unwrap_or(""), &branch) {
            continue;
        }
        let mut regs: Vec<(&Row, String)> = Vec::new();
        for x in g.loads_of(*bit) {
            let Some(inst) = x.instance.as_deref() else {
                continue;
            };
            if !x.clock {
                continue;
            }
            let pin = x.label();
            if !corners
                .iter()
                .any(|c| need.contains_key(&(pin.as_str(), c.as_str())))
            {
                continue;
            }
            let plain = kinds.get(inst).is_none_or(|k| {
                k.iter()
                    .all(|kind| matches!(*kind, "flip_flop" | "async_recovery_removal"))
            });
            if plain {
                regs.push((x, pin));
            }
        }
        if regs.is_empty() {
            continue;
        }
        let mut want = Vec::with_capacity(corners.len());
        let mut room = Vec::with_capacity(corners.len());
        for c in corners {
            let worst = regs
                .iter()
                .map(|(_, pin)| {
                    need.get(&(pin.as_str(), c.as_str()))
                        .copied()
                        .unwrap_or(0.0)
                })
                .reduce(first_max)
                .unwrap_or(0.0);
            want.push(worst + margin);
            let least = regs
                .iter()
                .map(|(x, _)| {
                    allow
                        .get(&(x.instance.as_deref().unwrap_or(""), c.as_str()))
                        .copied()
                        .unwrap_or(f64::INFINITY)
                })
                .reduce(first_min)
                .unwrap_or(f64::INFINITY);
            room.push(least - margin);
        }
        let mut most = i64::MIN;
        let mut least = i64::MAX;
        for (k, c) in corners.iter().enumerate() {
            let s = stage_of(c)?;
            let overflow = || Failure::new("INPUT", "cannot convert float infinity to integer");
            most = most.max(py_ceil(want[k] / s).ok_or_else(overflow)?);
            least = least.min(py_floor(room[k] / s).ok_or_else(overflow)?);
        }
        let n = cap.min(most).min(least);
        let mut registers: Vec<&str> = regs.iter().map(|(_, pin)| pin.as_str()).collect();
        registers.sort_unstable();
        let per = |values: &[f64], shift: f64| {
            corner_map(
                corners
                    .iter()
                    .zip(values)
                    .map(|(c, v)| (c.clone(), v + shift)),
                Some(4),
            )
        };
        let mut record = vec![
            ("action", Json::str("clock_balance")),
            ("net", names.get(bit).map_or(Json::Null, |n| Json::str(*n))),
            (
                "registers",
                Json::List(registers.iter().map(|r| Json::str(*r)).collect()),
            ),
            ("cell", Json::str(&cell)),
            ("stages", Json::Int(n.max(0).into())),
            ("stage_ns", corner_map(stage.iter().cloned(), Some(4))),
            ("deficit_ns", per(&want, -margin)),
            ("room_ns", per(&room, margin)),
        ];
        if n < 1 {
            record.push((
                "reason",
                Json::str("own hold slack leaves no room for one stage"),
            ));
            planner.excluded.push(obj(record));
            continue;
        }
        let mut pts = Vec::with_capacity(regs.len());
        for (x, _) in &regs {
            pts.push(geo.at_row(x)?);
        }
        let at_point = mean(&pts);
        let rows: Vec<&Row> = regs.iter().map(|(x, _)| *x).collect();
        for level in 0..n {
            let name = format!("{}_clkbal_{count}_{level}", planner.prefix);
            planner.append(buffer_sinks(&cell, &name, &rows))?;
            geo.place(&name, &cell, at_point)?;
        }
        count += 1;
        planner.checks.push(obj(record));
        done.push((n.max(0), regs.len()));
    }
    Ok(obj(vec![
        ("groups", Json::Int(done.len() as i128)),
        ("stages", Json::Int(done.iter().map(|d| d.0 as i128).sum())),
        (
            "registers",
            Json::Int(done.iter().map(|d| d.1 as i128).sum()),
        ),
        ("stage_ns", corner_map(stage.iter().cloned(), None)),
    ]))
}

/// Hold repair on estimated-RC reports: shared launch chains, then endpoint chains.
pub(crate) fn estimated_hold(
    planner: &mut Planner<'_>,
    reports: &[Arc<Report>],
    setups: &[Arc<Report>],
    corners: &[String],
    geo: &mut Geometry<'_, '_>,
) -> Res<Json> {
    let (inputs, policy, g) = (planner.i, planner.p, planner.g);
    super::trace("hold: start");
    let bounds = delay_bounds(inputs, policy, corners)?;
    super::trace("hold: delay bounds");
    let hold = hold_fix(&reports[0])?;
    let rows = &hold.merged;
    for x in reports {
        require(
            Arc::ptr_eq(x, &reports[0]) || hold_fix(x)? == hold,
            "REPORT",
            "inconsistent merged hold reports",
        )?;
    }
    let scope = reports[0]
        .scope
        .as_ref()
        .ok_or_else(|| key_error("scope"))?;
    let (early, late) = match scope.get("global_timing_derate") {
        None => (1.0, 1.0),
        Some(f) => {
            let items = f.as_list().unwrap_or(&[]);
            require(
                items.len() == 2,
                "INPUT",
                "two global timing derates required",
            )?;
            (json_number(&items[0], true)?, json_number(&items[1], true)?)
        }
    };
    let target = json_number(at(policy, "hold_margin_ns")?, false)?;
    let setup_margin = json_number(at(policy, "setup_margin_ns")?, false)?;
    require(
        hold.target_margin_ns >= target,
        "REPORT",
        "hold report target insufficient",
    )?;
    let mut sorted_cells: Vec<&(String, Vec<(String, Bound)>)> = bounds.iter().collect();
    sorted_cells.sort_by(|a, b| a.0.cmp(&b.0));
    let (cell, b) = (&sorted_cells[0].0, &sorted_cells[0].1);
    let mut worst_setup: Option<f64> = None;
    for x in setups {
        let least = endpoint_rows(x)?
            .iter()
            .filter_map(|r| r.setup_slack_ns)
            .reduce(first_min)
            .ok_or_else(|| Failure::new("INPUT", "min() iterable argument is empty"))?;
        worst_setup = Some(worst_setup.map_or(least, |w| first_min(w, least)));
    }
    let worst_setup =
        worst_setup.ok_or_else(|| Failure::new("INPUT", "min() iterable argument is empty"))?;
    require(worst_setup.is_finite(), "REPORT", "global setup bound")?;
    let mut details: HashMap<&str, &HoldEndpoint> = HashMap::default();
    // (net, corner) -> half the largest listed arc delay on a repaired net.
    let mut delta: HashMap<(&str, &str), f64> = HashMap::default();
    for co in &hold.corners {
        for detail in &co.endpoints {
            inputs.tick(1)?;
            details.entry(detail.endpoint.as_str()).or_insert(detail);
            let mut on: Vec<(&str, f64)> = Vec::new();
            for s in detail.stages() {
                let Some(net) = s.net.as_deref() else {
                    continue;
                };
                if planner.repaired.contains(net) {
                    let incr = s
                        .incr_ns
                        .ok_or_else(|| Failure::new("INPUT", "stage without incr_ns"))?;
                    let add = first_max(0.0, incr);
                    match on.iter_mut().find(|(n, _)| *n == net) {
                        Some(slot) => slot.1 += add,
                        None => on.push((net, 0.0 + add)),
                    }
                }
            }
            // A resized or isolated driver kept over half its arc delay at these loads.
            for (net, v) in on {
                let slot = delta.entry((net, co.corner.as_str())).or_insert(0.0);
                *slot = first_max(*slot, 0.5 * v);
            }
        }
    }
    // This round's electrical repairs (isolated, buffered or resized drivers) can only shorten
    // the arcs on their nets, and any path through them may become an endpoint's worst. A
    // repaired net may lose half its largest listed arc delay (else the largest such loss);
    // each endpoint is charged the largest such sum along one combinational path
    // (longest-path label correction over the repaired nets' forward cones, work-bounded).
    let ends: HashSet<&str> = endpoint_rows(&setups[0])?
        .iter()
        .map(|r| r.endpoint.as_str())
        .collect();
    let sequential: HashSet<&str> = ends
        .iter()
        .filter_map(|e| e.rsplit_once('/').map(|(inst, _)| inst))
        .collect();
    let mut outs: HashMap<&str, Vec<i64>> = HashMap::default();
    for (bit, d) in &g.drivers.items {
        if let Some(inst) = d.instance.as_deref() {
            outs.entry(inst).or_default().push(*bit);
        }
    }
    let mut faster: HashMap<(String, &str), f64> = HashMap::default();
    let mut work = 0i64;
    let mut repaired: Vec<&str> = planner.repaired.iter().map(String::as_str).collect();
    repaired.sort_unstable();
    for c in corners {
        let fallback = delta
            .iter()
            .filter(|((_, k), _)| *k == c.as_str())
            .map(|(_, v)| *v)
            .reduce(first_max)
            .unwrap_or(0.0);
        let mut weight: HashMap<i64, f64> = HashMap::default();
        for n in &repaired {
            let Some(bit) = g.net_bit(n).and_then(super::py::Bit::net) else {
                continue;
            };
            let w = delta.get(&(*n, c.as_str())).copied().unwrap_or(
                if planner.feedback_nets.contains(*n) {
                    0.0
                } else {
                    fallback
                },
            );
            weight.insert(bit, w);
        }
        let mut acc = weight.clone();
        let mut stack: Vec<i64> = acc.keys().copied().collect();
        stack.sort_unstable();
        while let Some(bit) = stack.pop() {
            let v = acc[&bit];
            for x in g.loads_of(bit) {
                work += 1;
                require(
                    work <= inputs.args.max_work,
                    "BUDGET",
                    "repaired-net cone walk",
                )?;
                let label = match &x.instance {
                    None => x.port.as_deref().unwrap_or("").to_string(),
                    Some(_) => x.label(),
                };
                if ends.contains(label.as_str()) {
                    let slot = faster.entry((label, c.as_str())).or_insert(0.0);
                    *slot = first_max(*slot, v);
                }
                let Some(inst) = x.instance.as_deref() else {
                    continue;
                };
                if sequential.contains(inst) || x.clock {
                    continue;
                }
                for &out in outs.get(inst).map_or(&[][..], Vec::as_slice) {
                    let next = v + weight.get(&out).copied().unwrap_or(0.0);
                    if next > acc.get(&out).copied().unwrap_or(-1.0) + 1e-9 {
                        acc.insert(out, next);
                        stack.push(out);
                    }
                }
            }
        }
    }
    let charge =
        |end: &str, c: &str| -> f64 { faster.get(&(end.to_string(), c)).copied().unwrap_or(0.0) };
    // Endpoint -> its effective slack per corner (in the report's corner order).
    let mut need: Vec<(&str, Vec<(&str, f64)>)> = Vec::new();
    let mut need_index: HashMap<&str, usize> = HashMap::default();
    let mut groups: Vec<((&str, Option<&str>), Vec<&str>)> = Vec::new();
    let mut group_index: HashMap<(&str, Option<&str>), usize> = HashMap::default();
    for row in rows {
        let slacks = &row.hold_slack_by_corner_ns;
        let complete = slacks.len() == corners.len()
            && corners.iter().all(|c| slacks.get(c).is_some())
            && slacks.iter().all(|(_, v)| v.is_some_and(f64::is_finite));
        require(complete, "REPORT", "missing corner hold slack")?;
        let end = row.endpoint.as_str();
        let effective: Vec<(&str, f64)> = slacks
            .iter()
            .map(|(c, v)| (c, v.unwrap_or(0.0) - charge(end, c)))
            .collect();
        let least = effective
            .iter()
            .map(|(_, v)| *v)
            .reduce(first_min)
            .unwrap_or(0.0);
        if least >= target {
            continue;
        }
        match need_index.get(end) {
            Some(&k) => need[k].1 = effective,
            None => {
                need_index.insert(end, need.len());
                need.push((end, effective));
            }
        }
        // A register launches through one output (Q or QB): group by that pin.
        let detail = details.get(end).ok_or_else(|| key_error(end))?;
        let start = detail.startpoint.as_str();
        let path = detail.stages();
        let via = if start.contains('/') && path.len() > 1 && path[1].arc.as_deref() == Some("cell")
        {
            path[1].pin.as_deref()
        } else {
            None
        };
        match group_index.get(&(start, via)) {
            Some(&k) => groups[k].1.push(end),
            None => {
                group_index.insert((start, via), groups.len());
                groups.push(((start, via), vec![end]));
            }
        }
    }
    let slack_of = |s: &[(&str, f64)], c: &str| -> Res<f64> {
        s.iter()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| *v)
            .ok_or_else(|| key_error(c))
    };
    // The stages that bring every listed slack to the target: the worst deficit over the
    // corners, divided by each corner's smallest stage delay.
    let stages = |list: &[&[(&str, f64)]]| -> Res<i64> {
        let mut n = i64::MIN;
        for s in list {
            let mut deficit: Option<f64> = None;
            for c in corners {
                let d = target - slack_of(s, c)?;
                deficit = Some(deficit.map_or(d, |w| first_max(w, d)));
            }
            let deficit = deficit.unwrap_or(0.0);
            for c in corners {
                let count = py_ceil(deficit / (early * bound(b, c)?.min))
                    .ok_or_else(|| Failure::new("INPUT", "cannot convert float to integer"))?;
                n = n.max(count);
            }
        }
        Ok(n.max(1))
    };
    let need_of = |end: &str| -> Res<&[(&str, f64)]> {
        need_index
            .get(end)
            .map(|&k| need[k].1.as_slice())
            .ok_or_else(|| key_error(end))
    };
    let mut count = 0usize;
    let mut launched: HashSet<&str> = HashSet::default();
    let mut credit: HashMap<&str, i64> = HashMap::default();
    let limit = match policy.get("launch_chain_max") {
        None => 32,
        Some(v) => v.int()?,
    };
    let depth = inputs.args.max_depth;
    let primary = text(policy, "primary_clock")?;
    let mut tree: HashSet<i64> = HashSet::default();
    for n in at(policy, "clock_roots")?.as_list().unwrap_or(&[]) {
        let n = n.as_str().unwrap_or("");
        if n != primary {
            tree.insert(g.bit(n)?);
        }
    }
    let ecfg = at(policy, "electrical")?;
    let antenna = strings(ecfg, "antenna_cells")?;
    let sram_masters = strings(ecfg, "sram_masters")?;
    let sram_buffer = text(ecfg, "sram_buffer")?.to_string();
    let mut order: Vec<&((&str, Option<&str>), Vec<&str>)> = groups.iter().collect();
    order.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then_with(|| a.0.0.cmp(b.0.0))
            .then_with(|| a.0.1.unwrap_or("").cmp(b.0.1.unwrap_or("")))
    });
    for ((start, via), members) in order {
        let (start, via) = (*start, *via);
        // A shared launch point, or one endpoint beyond the endpoint-chain cap, gets one chain
        // on the launch output (register Q or input port net).
        let mut over: Vec<&str> = Vec::new();
        for e in members {
            if stages(&[need_of(e)?])? > depth {
                over.push(e);
            }
        }
        if members.len() < 2 && over.is_empty() {
            continue;
        }
        let (bit, at_point, inside);
        if let Some((inst, clock_pin)) = start.rsplit_once('/') {
            let mut outs: Vec<(&str, i64)> = Vec::new();
            for (p, pin_bit) in g.pins_of(inst) {
                let drives = g
                    .drivers
                    .get(*pin_bit)
                    .is_some_and(|d| d.instance.as_deref() == Some(inst));
                if drives {
                    // A later pin of the same label replaces an earlier one in the
                    // reference's dict.
                    match outs.iter_mut().find(|(name, _)| *name == &**p) {
                        Some(slot) => slot.1 = *pin_bit,
                        None => outs.push((p, *pin_bit)),
                    }
                }
            }
            if let Some(via) = via {
                outs.retain(|(p, _)| {
                    via.strip_prefix(inst).and_then(|r| r.strip_prefix('/')) == Some(*p)
                });
            }
            if outs.len() != 1 {
                continue;
            }
            let (out_pin, out_bit) = outs[0];
            bit = out_bit;
            at_point = geo.pin(Some(inst), out_pin)?;
            inside = g
                .pin_bit(inst, clock_pin)
                .is_some_and(|b| tree.contains(&b));
        } else {
            inside = false;
            let Some(start_bit) = g.net_bit(start).and_then(super::py::Bit::net) else {
                continue;
            };
            bit = start_bit;
            match g.drivers.get(bit) {
                Some(d) if d.instance.is_none() => {}
                _ => continue,
            }
            at_point = geo.pin(None, start)?;
        }
        if planner.clocks.contains(&bit) {
            continue;
        }
        let (ends, n): (Vec<&str>, i64) = if inside {
            // Registers on one branch of the tree share its skew: a launch chain would only
            // expose the branch's next launch point. Endpoint chains take the delay; this
            // chain only the part beyond their cap.
            if over.is_empty() {
                continue;
            }
            let mut most = i64::MIN;
            for e in &over {
                most = most.max(stages(&[need_of(e)?])?);
            }
            (over, most - depth)
        } else {
            let mut list = Vec::with_capacity(members.len());
            for e in members {
                list.push(need_of(e)?);
            }
            (members.clone(), stages(&list)?)
        };
        if n > limit || worst_setup - chain_setup(b, corners, late, n, true)? < setup_margin {
            continue;
        }
        let loads = g.loads_of(bit);
        let sinks: Vec<&Row> = loads
            .iter()
            .filter(|x| {
                x.instance.is_some()
                    && !x
                        .cell
                        .as_deref()
                        .is_some_and(|c| antenna.iter().any(|a| a == c))
            })
            .collect();
        let ports: Vec<String> = loads
            .iter()
            .filter(|x| x.instance.is_none())
            .map(|x| x.port.as_deref().unwrap_or("").to_string())
            .collect();
        if sinks.is_empty() && ports.is_empty() {
            continue;
        }
        // Each later stage moves the same sinks and ports, now on the previous stage's net.
        for level in 0..n {
            let name = format!("{}_launch_{count}_{level}", planner.prefix);
            planner.append(Op::BufferSinks {
                cell: cell.clone(),
                instance: name.clone(),
                sinks: sinks.iter().filter_map(|x| x.sink()).collect(),
                ports: ports.clone(),
            })?;
            geo.place(&name, cell, at_point)?;
        }
        count += 1;
        if inside {
            for e in &ends {
                credit.insert(e, n);
            }
        } else {
            launched.extend(ends.iter().copied());
        }
        planner.checks.push(obj(vec![
            ("startpoint", Json::str(start)),
            ("launch_pin", via.map_or(Json::Null, Json::str)),
            ("endpoints", Json::Int(ends.len() as i128)),
            ("stages", Json::Int(n.into())),
            ("cell", Json::str(cell)),
            ("inside_tree", Json::Bool(inside)),
        ]));
    }
    let mut sorted_need: Vec<&(&str, Vec<(&str, f64)>)> = need.iter().collect();
    sorted_need.sort_by(|a, b| a.0.cmp(b.0));
    for (index, (end, slacks)) in sorted_need.into_iter().enumerate() {
        let end = *end;
        let detail = details.get(end).ok_or_else(|| key_error(end))?;
        let (inst, pin, bit) = g.sink_binding(detail)?;
        require(
            !planner.clocks.contains(&bit) && (planner.bus_rewrite || !pin.contains('[')),
            "POLICY",
            "clock or bus sink lacks a delay point",
        )?;
        // A launch chain delays only its own launch point: two stages at the pin guard the
        // endpoint's other, unlisted launch points.
        let credited = credit.get(end).copied();
        let n = if launched.contains(end) {
            2
        } else {
            stages(&[slacks.as_slice()])? - credited.unwrap_or(0)
        };
        let feasible = n <= depth
            && worst_setup
                - chain_setup(
                    b,
                    corners,
                    late,
                    n + credited.unwrap_or(0),
                    credited.is_some(),
                )?
                >= setup_margin;
        if !feasible {
            let least = slacks
                .iter()
                .map(|(_, v)| *v)
                .reduce(first_min)
                .unwrap_or(0.0);
            let most = corners
                .iter()
                .map(|c| charge(end, c))
                .reduce(first_max)
                .unwrap_or(0.0);
            return Err(Failure::new(
                "HOLD_INFEASIBLE",
                format!(
                    "no bounded all-corner delay chain: {end} (effective slack {least:.3} ns, repair charge {most:.3} ns)"
                ),
            ));
        }
        let at_point = geo.pin(Some(&inst), &pin)?;
        let tag = zero_padded(index, 4);
        for level in 0..n {
            let name = format!("{}_{tag}_{level}", planner.prefix);
            planner.append(Op::InsertBuffer {
                cell: cell.clone(),
                instance: name.clone(),
                sink: (inst.clone(), pin.clone()),
            })?;
            geo.place(&name, cell, at_point)?;
        }
        if g.cell_type(&inst)
            .is_some_and(|t| sram_masters.iter().any(|m| m == t))
        {
            // A delay cell's slow edge would violate the SRAM input transition.
            let name = format!("{}_{tag}_pin", planner.prefix);
            planner.append(Op::InsertBuffer {
                cell: sram_buffer.clone(),
                instance: name.clone(),
                sink: (inst.clone(), pin.clone()),
            })?;
            geo.place(&name, &sram_buffer, at_point)?;
            planner.weigh(format!("{name}_n"));
        }
        planner.checks.push(obj(vec![
            ("endpoint", Json::str(end)),
            ("stages", Json::Int(n.into())),
            ("cell", Json::str(cell)),
        ]));
    }
    Ok(obj(vec![
        (
            "basis",
            Json::str("placement RC estimate; table extrema; re-time after placement"),
        ),
        ("violating", Json::Int(need.len() as i128)),
        (
            "charged_repairs",
            Json::Int(faster.values().filter(|v| **v > 0.0).count() as i128),
        ),
        ("report_margin_ns", Json::Float(hold.target_margin_ns)),
        ("launch_groups", Json::Int(count as i128)),
        ("worst_setup_ns", Json::Float(worst_setup)),
        ("bounds", bounds_json(&bounds)),
    ]))
}

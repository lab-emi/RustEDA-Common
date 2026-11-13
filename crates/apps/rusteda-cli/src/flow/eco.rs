// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! `rusteda eco-round`: one ECO round of the Ibex recipe's native frontend as one native
//! command (RCLI-REQ-0027). It is the reference helper's `bundle --eco-workspace`
//! (`native_frontend_inputs.py`) followed by the planner stage (`native_ecos.py`) for the
//! operations the frontend runs: `reset`, `fanout`, `primary-clock` on ideal-wire reports,
//! `electrical` and `hold` on placement-estimated RC reports.
//!
//! The bundle binds every input of the round by SHA-256 (`inputs.json`), records the clock and
//! reset roots (`roots.json`) and the round's policy (`policy.json`). The stage checks every
//! record again, binds the timing reports to their compiled STA revision, plans the operations,
//! lets the pinned `rustroad apply-eco` edit the netlist, hashes every input again and writes
//! `plan.json`, `netlist.v`, the seed and weights of the next placement and `receipt.json`.
//! Files, their order of keys, numbers and the summary line on standard output are the
//! reference's byte for byte, but for the receipt's run time, peak memory and planner hash.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap as HashMap;

use super::context::{
    Context, Failure, Res, at, fresh, need, peak_rss_kib, read_json as read_helper_json, require,
    resolve, sha_fresh, show, text, workers, write_json,
};
use super::eco_estimated::{Geometry, PlacementFiles, clock_balance, electrical, estimated_hold};
use super::eco_graph::{Graph, Planner};
use super::eco_inputs::{
    Budgets, Docs, DrvKind, Inputs, Kind, Report, io_projection, pin_views, read_json,
};
use crate::estimate::{Json, json_string, obj};

/// The round's arguments (`--workspace` is the global `-w/--workspace`: the ECO workspace).
#[derive(clap::Args, Debug)]
pub struct EcoRoundArgs {
    /// The run's pinned context (`rusteda.native-frontend-context.v1`).
    #[arg(long = "context", value_name = "FILE")]
    pub context: PathBuf,
    /// The round: reset, fanout, primary-clock, electrical or hold.
    #[arg(long = "operation", value_name = "OP")]
    pub operation: String,
    /// The timing tag whose reports the round reads (`logs/fe_<TAG>_*.stdout`).
    #[arg(long = "timing-tag", value_name = "TAG")]
    pub timing_tag: String,
    /// The netlist to edit.
    #[arg(long = "source", value_name = "FILE")]
    pub source: PathBuf,
    /// The connectivity graph of the netlist to edit.
    #[arg(long = "graph", value_name = "FILE")]
    pub graph: PathBuf,
    /// The netlist that the reports were compiled from, when it is not the source.
    #[arg(long = "timed", value_name = "FILE")]
    pub timed: Option<PathBuf>,
    /// The fresh directory that receives the bundle (`inputs.json`, `roots.json`,
    /// `policy.json`).
    #[arg(long = "bundle", value_name = "DIR")]
    pub bundle: PathBuf,
    /// Base revision of both workspaces; only `empty` (fresh directories) is accepted.
    #[arg(long = "base-rev", value_name = "REV")]
    pub base_rev: String,
    /// Plan only: write the plan, seed, weights and receipt (`planned_unverified`) without
    /// running `rustroad apply-eco` (no `netlist.v`).
    #[arg(long = "plan-only")]
    pub plan_only: bool,
}

/// The reference's two failure reports: the bundle's and the planner's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Bundle,
    Stage,
}

fn report(phase: Phase, e: &Failure) -> String {
    let mut line = String::new();
    match phase {
        Phase::Bundle => {
            line.push_str("{\"status\": \"failed\", \"code\": ");
            json_string(&mut line, &e.code);
            line.push_str(", \"error\": ");
        }
        Phase::Stage => {
            line.push_str("{\"status\": \"error\", \"code\": ");
            json_string(&mut line, &e.code);
            line.push_str(", \"message\": ");
        }
    }
    json_string(&mut line, &e.message);
    line.push_str("}\n");
    line
}

static PHASE: Mutex<Phase> = Mutex::new(Phase::Bundle);
/// The wall deadline of the running phase, in milliseconds since the command started.
static DEADLINE_MS: AtomicU64 = AtomicU64::new(180_000);

fn set_phase(phase: Phase) {
    *PHASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = phase;
}

fn phase() -> Phase {
    *PHASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `rusteda eco-round` for the ECO workspace `ws`.
pub fn run(ws: &Path, args: &EcoRoundArgs) -> ExitCode {
    let started = Instant::now();
    // The reference's alarms: 180 s for the bundle, then the planner's wall budget.
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if started.elapsed().as_millis() as u64 > DEADLINE_MS.load(Ordering::Relaxed) {
                let line = report(phase(), &Failure::new("TIMEOUT", "planner wall budget"));
                eprint!("{line}");
                std::process::exit(2);
            }
        }
    });
    match round(ws, args, started) {
        Ok(summary) => {
            super::emit(&summary);
            // The files are committed and the locks end with the process: leaving at once
            // spares freeing the decoded reports and the graph one value at a time.
            std::process::exit(0)
        }
        Err(e) => {
            eprint!("{}", report(phase(), &e));
            ExitCode::from(2)
        }
    }
}

const OPERATIONS: [&str; 5] = ["reset", "fanout", "primary-clock", "electrical", "hold"];

fn round(ws: &Path, args: &EcoRoundArgs, started: Instant) -> Res<String> {
    need(
        OPERATIONS.contains(&args.operation.as_str()),
        format!(
            "USAGE: --operation must be one of {}",
            OPERATIONS.join(", ")
        ),
    )?;
    let context = Context::load(&args.context)?;
    super::trace("context loaded");
    let docs = Docs::default();
    let xxh3 = RefCell::new(HashMap::default());
    let bundle = {
        let _lock = fresh(&args.bundle, &args.base_rev)?;
        let bundle = bundle(&context, args, &docs, &xxh3)?;
        fs::write(args.bundle.join("HEAD"), "rev1\n")?;
        bundle
    };
    context.digests.save_flow();
    super::trace("bundle done");
    set_phase(Phase::Stage);
    let budgets = Budgets::default();
    DEADLINE_MS.store(
        started.elapsed().as_millis() as u64 + budgets.timeout_seconds as u64 * 1000,
        Ordering::Relaxed,
    );
    stage(&context, args, ws, budgets, &docs, &xxh3, bundle)
}

/// What the stage takes over from the bundle of the same process.
struct Bundled {
    graph: Graph,
    /// The work units the graph cost, and those spent before it.
    work: (i64, i64),
    top: String,
}

fn drv_file(root: &Path, tag: &str, corner: &str, kind: &str) -> PathBuf {
    root.join(format!("logs/fe_{tag}_{corner}_{kind}.stdout"))
}

/// `{**record, key: value}` appended to a file record.
fn with(mut record: Json, key: &str, value: Json) -> Json {
    record.set(key, value);
    record
}

/// The bundle of the round: binds every input (`inputs.json`), the roots (`roots.json`) and the
/// policy (`policy.json`).
fn bundle<'a>(
    c: &'a Context,
    args: &EcoRoundArgs,
    docs: &'a Docs,
    xxh3: &'a RefCell<HashMap<String, (super::context::Identity, String)>>,
) -> Res<Bundled> {
    let out = &args.bundle;
    let root = &c.root;
    let tag = args.timing_tag.as_str();
    need(
        !tag.is_empty()
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "unsafe timing tag",
    )?;
    let operation = args.operation.as_str();
    let source = c.dynamic(&args.source)?;
    let timed = c.dynamic(args.timed.as_deref().unwrap_or(&args.source))?;
    let graph = c.dynamic(&args.graph)?;
    let profile = root.join("frontend/profiles");
    let estimated = matches!(operation, "electrical" | "hold");
    let sdc = c.file("sdc")?;
    let corners = ["tt", "ss"];
    // Everything the round reads is hashed, and the documents decoded, beside each other
    // first; the records and checks below then find them.
    {
        let mut items: Vec<(PathBuf, Kind)> = Vec::new();
        if estimated {
            items.push((
                root.join(format!("logs/fe_{tag}_hold.stdout")),
                Kind::Report,
            ));
            items.push((root.join(format!("logs/fe_{tag}_def.stdout")), Kind::Bytes));
        }
        for co in corners {
            items.push((drv_file(root, tag, co, "drv"), Kind::Report));
        }
        items.push((graph.clone(), Kind::Graph));
        if estimated {
            for co in corners {
                items.push((drv_file(root, tag, co, "endpoints"), Kind::Report));
            }
            items.push((
                root.join(format!("frontend/estimate-{tag}/estimate.spef")),
                Kind::Hash,
            ));
            items.push((root.join(format!("logs/fe_{tag}_seed.stdout")), Kind::Json));
            items.push((c.file("cell_lef")?, Kind::Bytes));
            items.push((
                root.join("frontend/eco-electrical/weights.json"),
                Kind::Json,
            ));
        }
        // The raw Liberty copies of the STA workspace that compiled the reports.
        if let Ok(cache) = fs::read_dir(root.join(format!("frontend/sta-{tag}/cache"))) {
            for entry in cache.flatten() {
                if entry.path().extension().is_some_and(|e| e == "lib") {
                    items.push((entry.path(), Kind::Hash));
                }
            }
        }
        items.push((source.clone(), Kind::Hash));
        items.push((timed.clone(), Kind::Hash));
        for x in ["io.json", "delay.json", "sizing.json", "cell-buses.json"] {
            items.push((profile.join(x), Kind::Hash));
        }
        for co in corners {
            for k in ["std", "ram"] {
                items.push((
                    root.join(format!("logs/fe_caps_{co}_{k}.stdout")),
                    Kind::Hash,
                ));
            }
        }
        let items: Vec<(PathBuf, Kind)> = items
            .into_iter()
            .filter_map(|(p, k)| fs::canonicalize(&p).ok().map(|p| (p, k)))
            .collect();
        docs.preload(&c.digests, &items);
    }
    super::trace("bundle: preloaded");
    let files = at(&c.doc, "files")?;
    let tools = at(&c.doc, "tools")?;
    let mut inputs = vec![
        ("schema", Json::str("rusteda.native-eco-inputs.v1")),
        ("netlist", c.rec(&source)?),
        (
            "netlist_json",
            with(
                c.rec(&graph)?,
                "source_sha256",
                at(&c.rec(&source)?, "sha256")?.clone(),
            ),
        ),
        ("sdc", c.rec(&sdc)?),
        ("io", c.rec(&profile.join("io.json"))?),
        (
            "profiles",
            obj(vec![
                ("delay", c.rec(&profile.join("delay.json"))?),
                ("lef", c.rec(&c.file("cell_lef")?)?),
                ("sizing", c.rec(&profile.join("sizing.json"))?),
                ("cell_buses", c.rec(&profile.join("cell-buses.json"))?),
            ]),
        ),
    ];
    let mut tool_records = vec![
        ("rustroad", at(tools, "rustroad")?.clone()),
        ("xxhsum", at(tools, "xxhsum")?.clone()),
        ("liberty_reader", at(files, "liberty_reader")?.clone()),
    ];
    let mut tail: Vec<(&str, Json)> = Vec::new();
    if timed != source {
        tail.push(("timed_netlist", c.rec(&timed)?));
    }
    if estimated {
        let seed = c.rec(&c.dynamic(&root.join(format!("logs/fe_{tag}_seed.stdout")))?)?;
        let def = c.rec(&c.dynamic(&root.join(format!("logs/fe_{tag}_def.stdout")))?)?;
        let placement_def_sha = at(&def, "sha256")?.clone();
        let mut placement = vec![("seed", seed), ("def", def)];
        if operation == "hold" {
            placement.push((
                "weights",
                c.rec(&c.dynamic(&root.join("frontend/eco-electrical/weights.json"))?)?,
            ));
        }
        tail.push(("placement", obj(placement)));
        tail.push(("rc_policy", c.rec(&c.file("rc_policy")?)?));
        tool_records.push(("estimator", at(files, "estimator")?.clone()));
        let estimate = read_helper_json(
            &c.dynamic(&root.join(format!("frontend/estimate-{tag}/result.json")))?,
            1 << 20,
        )?;
        let of = at(&estimate, "inputs")?;
        need(
            *at(at(of, "def")?, "sha256")? == placement_def_sha
                && at(at(of, "policy")?, "sha256")? == at(at(files, "rc_policy")?, "sha256")?,
            "estimate belongs to another placement or policy",
        )?;
    }
    let mut corner_records: Vec<(&str, Json)> = Vec::new();
    for co in corners {
        let drv = c.dynamic(&drv_file(root, tag, co, "drv"))?;
        let mut tables = Vec::new();
        for (kind, key) in [("std", "std"), ("ram", "ram")] {
            let table =
                c.rec(&c.dynamic(&root.join(format!("logs/fe_caps_{co}_{kind}.stdout")))?)?;
            let library = c.rec(&c.file(&format!("{key}_{co}"))?)?;
            tables.push(with(table, "library", library));
        }
        let mut record = vec![("pin_tables", Json::List(tables)), ("drv", c.rec(&drv)?)];
        if estimated {
            record.push((
                "spef",
                c.rec(&c.dynamic(&root.join(format!("frontend/estimate-{tag}/estimate.spef")))?)?,
            ));
            record.push((
                "endpoints",
                c.rec(&c.dynamic(&drv_file(root, tag, co, "endpoints"))?)?,
            ));
            record.push((
                "hold_fix",
                c.rec(&c.dynamic(&root.join(format!("logs/fe_{tag}_hold.stdout")))?)?,
            ));
        }
        corner_records.push((co, obj(record)));
    }
    inputs.push(("tools", obj(tool_records)));
    inputs.push(("corners", obj(corner_records)));
    inputs.extend(tail);
    let inputs = obj(inputs);
    write_json(&out.join("inputs.json"), &inputs)?;
    super::trace("bundle: inputs.json");

    let budgets = Budgets::default();
    let parsed = Inputs::new(
        &out.join("inputs.json"),
        budgets.clone(),
        &c.digests,
        docs,
        xxh3,
    )?;
    let corner_names: Vec<String> = corners.iter().map(|c| c.to_string()).collect();
    let io = io_projection(&parsed, &sdc)?;
    let pins = Rc::new(pin_views(&parsed, &corner_names)?);
    let document = parsed.graph(at(&inputs, "netlist_json")?)?;
    let before = parsed.work.units();
    let selection = c.selection()?;
    let top = text(selection, "top")?.to_string();
    let g = Graph::build(&parsed.work, &budgets, document, &top, pins, &io)?;
    super::trace("bundle: graph");
    let work = (before, parsed.work.units());
    let mut reports = Vec::new();
    for co in &corner_names {
        reports.push(parsed.report(co, "drv", &timed, &sdc)?);
    }
    super::trace("bundle: reports");
    let initial = root.join("frontend/bundle-reset/roots.json");
    let driver_json = |d: &super::eco_graph::Row| {
        obj(vec![
            (
                "instance",
                d.instance.as_deref().map_or(Json::Null, Json::str),
            ),
            ("cell", d.cell.as_deref().map_or(Json::Null, Json::str)),
            ("pin", d.pin.as_deref().map_or(Json::Null, Json::str)),
        ])
    };
    let roots = if operation == "reset" {
        // The net with the most sinks of the pin, which must be one of at least 1000 sinks,
        // named by a compiled max_fanout row of its driver.
        let choose = |pin: &str| -> Res<(String, &super::eco_graph::Row, i64)> {
            let mut best: Option<(i64, i64)> = None;
            let mut counts = Vec::with_capacity(g.loads.items.len());
            for (bit, rows) in &g.loads.items {
                let n = rows
                    .iter()
                    .filter(|x| x.pin.as_deref() == Some(pin))
                    .count() as i64;
                counts.push(n);
                if best.is_none_or(|b| (n, *bit) > b) {
                    best = Some((n, *bit));
                }
            }
            let (count, bit) =
                best.ok_or_else(|| Failure::new("INPUT", "max() iterable argument is empty"))?;
            need(
                count >= 1000 && counts.iter().filter(|n| **n == count).count() == 1,
                "ambiguous large reset/clock root",
            )?;
            let d = g
                .drivers
                .get(bit)
                .ok_or_else(|| super::context::key_error(&bit.to_string()))?;
            need(d.instance.is_some(), "root driven by a port")?;
            let driver = d.label();
            let mut names: Vec<&str> = Vec::new();
            for env in &reports {
                let rows = &env
                    .sections
                    .as_ref()
                    .and_then(|s| s.drv.as_ref())
                    .ok_or_else(|| super::context::key_error("drv"))?
                    .rows;
                for r in rows {
                    if r.pin == driver && r.kind == DrvKind::Fanout && g.bit(&r.net)? == bit {
                        names.push(&r.net);
                    }
                }
            }
            names.sort_unstable();
            need(!names.is_empty(), "compiled DRV row does not bind root")?;
            Ok((names[0].to_string(), d, count))
        };
        let (clock, driver, ck) = choose(text(selection, "clock_sink_pin")?)?;
        let (reset, rd, rb) = choose(text(at(selection, "reset")?, "pin")?)?;
        let gating = at(selection, "gating_cells")?.as_list().unwrap_or(&[]);
        need(
            gating.iter().any(|x| x.as_str() == driver.cell.as_deref()),
            "largest CK root is not the explicitly selected gate class",
        )?;
        let primary = g.bit(text(selection, "primary_clock")?)?;
        let instance = driver.instance.as_deref().unwrap_or("");
        let cells = g.top().cells.as_ref();
        let cell = cells
            .and_then(|cells| cells.get(instance))
            .ok_or_else(|| super::context::key_error(instance))?;
        let mut derived = false;
        for (port, values) in cell.connections.iter() {
            if values.iter().any(|b| b.net() == Some(primary)) {
                let direction = cell
                    .port_directions
                    .get(port)
                    .ok_or_else(|| super::context::key_error(port))?;
                if direction == "input" {
                    derived = true;
                    break;
                }
            }
        }
        need(
            derived,
            "gated root does not derive directly from the original primary clock",
        )?;
        let mut drv_records = Vec::new();
        for co in corners {
            let record = at(at(at(&inputs, "corners")?, co)?, "drv")?;
            drv_records.push(c.rec(Path::new(text(record, "path")?))?);
        }
        obj(vec![
            ("clock_root", Json::Str(clock)),
            ("reset_root", Json::Str(reset)),
            ("clock_driver", driver_json(driver)),
            ("reset_driver", driver_json(rd)),
            ("clock_sinks", Json::Int(ck.into())),
            ("reset_sinks", Json::Int(rb.into())),
            ("sdc", c.rec(&sdc)?),
            ("source", c.rec(&source)?),
            ("graph", c.rec(&graph)?),
            ("reports", Json::List(drv_records)),
        ])
    } else {
        read_helper_json(&c.dynamic(&initial)?, 1 << 20)?
    };
    need(
        at(&roots, "sdc")?.py_eq(&c.rec(&sdc)?),
        "root policy belongs to another SDC",
    )?;
    for (name, driver) in [
        ("clock_root", "clock_driver"),
        ("reset_root", "reset_driver"),
    ] {
        let bit = g.bit(text(&roots, name)?)?;
        let actual = g
            .drivers
            .get(bit)
            .ok_or_else(|| super::context::key_error(&bit.to_string()))?;
        let bound = at(&roots, driver)?;
        let actual = driver_json(actual);
        let mut same = true;
        for k in ["instance", "cell", "pin"] {
            same &= at(&actual, k)?.py_eq(at(bound, k)?);
        }
        need(
            same,
            "current root driver differs from bound first-run policy",
        )?;
    }
    write_json(&out.join("roots.json"), &roots)?;
    let mut policy = Vec::new();
    for k in [
        "top",
        "primary_clock",
        "buffers",
        "primary_buffer",
        "delay_cells",
        "target_fanout",
        "cap_fraction",
        "hold_margin_ns",
        "setup_margin_ns",
    ] {
        policy.push((k, at(selection, k)?.clone()));
    }
    let mut policy = obj(policy);
    if let Some(diode) = selection.get("port_diode") {
        policy.set("port_diode", diode.clone());
    }
    policy.set("schema", Json::str("rusteda.native-eco-policy.v1"));
    policy.set(
        "clock_roots",
        Json::List(vec![
            at(selection, "primary_clock")?.clone(),
            at(&roots, "clock_root")?.clone(),
        ]),
    );
    policy.set(
        "reset",
        with(
            at(selection, "reset")?.clone(),
            "net",
            at(&roots, "reset_root")?.clone(),
        ),
    );
    policy.set(
        "scope",
        Json::str(if estimated {
            "estimated_rc"
        } else {
            "ideal_diagnostic"
        }),
    );
    policy.set(
        "prefix",
        Json::Str(format!("native_{}", operation.replace('-', "_"))),
    );
    if estimated {
        policy.set("electrical", at(selection, "electrical")?.clone());
        policy.set(
            "launch_chain_max",
            at(selection, "launch_chain_max")?.clone(),
        );
    }
    write_json(&out.join("policy.json"), &policy)?;
    Ok(Bundled {
        graph: g,
        work,
        top,
    })
}

/// Hashes `paths[k]` for every `k` of `which` again in full, on `threads` threads, into
/// `slots`.
fn hash_into(
    paths: &[(String, String)],
    which: &[usize],
    threads: usize,
    slots: &[Mutex<Option<String>>],
) {
    let next = Mutex::new(which.iter());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(which.len()) {
            scope.spawn(|| {
                loop {
                    let k = next
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next();
                    let Some(&k) = k else { break };
                    let sha = sha_fresh(Path::new(&paths[k].0)).ok().map(|d| d.0);
                    *slots[k]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = sha;
                }
            });
        }
    });
}

/// The final input check around the apply step: runs `apply` and requires that the content of
/// every input equals its recorded SHA-256 once `apply` has returned.
///
/// Every file is read in full and hashed again (the reference's `fresh_digests`). A file whose
/// status is exact and settled (local file system, status changed at least a second ago: the
/// pinned tools and libraries, 110 MB of a round's 160 MB) is read while `apply` runs, on the
/// CPUs it leaves idle, and its identity (device, inode, size, mtime, ctime) is compared after
/// `apply`: every write, rename and replacement changes the identity of a settled file, so an
/// equal identity proves that the content hashed during `apply` is the content at its end. Any
/// other file (this run's reports, netlists and graphs, written moments ago, whose status may
/// not show a change within the same timestamp tick) is read after `apply`, as the reference
/// reads all of them.
fn checked_apply(
    c: &Context,
    paths: &[(String, String)],
    apply: impl FnOnce() -> Res<()>,
) -> Res<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    let mut settled = Vec::new();
    let mut identities = Vec::with_capacity(paths.len());
    for (k, (path, _)) in paths.iter().enumerate() {
        let id = fs::metadata(path).ok().and_then(|m| {
            let id = super::context::identity(&m);
            (c.digests.local(&m) && id[4] < now - 1_000_000_000).then_some(id)
        });
        if id.is_some() {
            settled.push(k);
        }
        identities.push(id);
    }
    // The largest first: the longest read starts at once.
    settled.sort_by_key(|&k| std::cmp::Reverse(identities[k].map_or(0, |id| id[2])));
    let slots: Vec<Mutex<Option<String>>> = paths.iter().map(|_| Mutex::new(None)).collect();
    let beside = workers(paths.len()).saturating_sub(2).max(1);
    let applied = std::thread::scope(|scope| {
        scope.spawn(|| hash_into(paths, &settled, beside, &slots));
        apply()
    });
    applied?;
    super::trace("stage: applied");
    let mut rest = Vec::new();
    for (k, (path, _)) in paths.iter().enumerate() {
        let same = identities[k].is_some()
            && fs::metadata(path)
                .ok()
                .map(|m| super::context::identity(&m))
                == identities[k];
        if !same {
            rest.push(k);
        }
    }
    hash_into(paths, &rest, workers(rest.len()), &slots);
    let unchanged = paths.iter().zip(&slots).all(|((_, sha), now)| {
        now.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            == Some(sha.as_str())
    });
    require(
        unchanged,
        "STALE_INPUT",
        "an input changed during planning/apply",
    )
}

/// `gather(bundle)`: every file record of the manifest, depth first in key order.
fn gather<'j>(value: &'j Json, records: &mut Vec<&'j Json>) {
    match value {
        Json::Object(items) => {
            if value.get("path").is_some() && value.get("sha256").is_some() {
                records.push(value);
            }
            for (_, child) in items {
                gather(child, records);
            }
        }
        Json::List(items) => {
            for child in items {
                gather(child, records);
            }
        }
        _ => {}
    }
}

fn scope_of(report: &Report) -> Res<&Json> {
    report
        .scope
        .as_ref()
        .ok_or_else(|| super::context::key_error("scope"))
}

/// The planner stage of the round (`native_ecos.py OP --workspace W --base-rev empty --inputs
/// B/inputs.json --policy B/policy.json`).
fn stage<'a>(
    c: &'a Context,
    args: &EcoRoundArgs,
    workspace: &Path,
    budgets: Budgets,
    docs: &'a Docs,
    xxh3: &'a RefCell<HashMap<String, (super::context::Identity, String)>>,
    bundled: Bundled,
) -> Res<String> {
    let started = Instant::now();
    let operation = args.operation.as_str();
    require(budgets.valid(), "BUDGET", "invalid policy budget")?;
    let limit = budgets.memory_mib << 20;
    fs::create_dir_all(workspace)?;
    let lock = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(workspace.join("workspace.lock"))?;
    lock.try_lock()
        .map_err(|_| Failure::new("WORKSPACE_LOCKED", "ECO workspace has another writer"))?;
    let mut others = false;
    for entry in fs::read_dir(workspace)? {
        if entry?.file_name() != "workspace.lock" {
            others = true;
        }
    }
    require(
        args.base_rev == "empty" && !others,
        "REVISION_CONFLICT",
        "ECO stage requires a fresh empty workspace",
    )?;
    let staging = workspace.join(".pending");
    fs::create_dir(&staging)?;
    let inputs_path = args.bundle.join("inputs.json");
    let policy_path = args.bundle.join("policy.json");
    let inputs = Inputs::new(&inputs_path, budgets, &c.digests, docs, xxh3)?;
    let policy = read_json(&policy_path, 1 << 20)?;
    require(
        policy.get("schema").and_then(Json::as_str) == Some("rusteda.native-eco-policy.v1"),
        "SCHEMA",
        "policy schema",
    )?;
    const KNOWN: [&str; 18] = [
        "schema",
        "top",
        "clock_roots",
        "primary_clock",
        "buffers",
        "primary_buffer",
        "reset",
        "delay_cells",
        "target_fanout",
        "cap_fraction",
        "hold_margin_ns",
        "setup_margin_ns",
        "scope",
        "prefix",
        "setup_corner",
        "port_diode",
        "electrical",
        "launch_chain_max",
    ];
    require(
        policy
            .as_object()
            .is_some_and(|items| items.iter().all(|(k, _)| KNOWN.contains(&k.as_str()))),
        "POLICY",
        "unknown policy field",
    )?;
    inputs.set_file(show(&resolve(&policy_path)?), inputs.sha(&policy_path)?);
    let manifest = &inputs.manifest;
    let corners: Vec<String> = at(manifest, "corners")?
        .as_object()
        .unwrap_or(&[])
        .iter()
        .map(|(k, _)| k.clone())
        .collect();
    require(
        corners.len() == 2 && corners[0] != corners[1],
        "POLICY",
        "exactly two explicit common PVT corners required",
    )?;
    let source = inputs.file64(at(manifest, "netlist")?)?;
    let timed = inputs.file64(match manifest.get("timed_netlist") {
        Some(record) => record,
        None => at(manifest, "netlist")?,
    })?;
    let source_json = at(manifest, "netlist_json")?;
    require(
        at(source_json, "source_sha256")?.as_str() == Some(inputs.sha(&source)?.as_str()),
        "STALE_INPUT",
        "structural JSON source mismatch",
    )?;
    let sdc = inputs.file(at(manifest, "sdc")?, 2 << 20)?;
    // Estimate before decoding the potentially large graph and timing reports.
    let mut records = Vec::new();
    gather(manifest, &mut records);
    let mut decoded_files: Vec<(PathBuf, u64)> = Vec::new();
    for r in records {
        let suffix = Path::new(text(r, "path")?)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(suffix, "json" | "lib" | "tsv") {
            let path = inputs.file(r, 256 << 20)?;
            let size = fs::metadata(&path)?.len();
            match decoded_files.iter_mut().find(|(p, _)| *p == path) {
                Some(slot) => slot.1 = size,
                None => decoded_files.push((path, size)),
            }
        }
    }
    let decoded: i64 = decoded_files.iter().map(|(_, n)| *n as i64).sum();
    let estimate = decoded * 8
        + inputs.args.max_connections * 128
        + inputs.args.max_operations * 1024
        + (64 << 20);
    require(
        estimate <= limit,
        "BUDGET",
        format!("upfront memory estimate {estimate} exceeds {limit}"),
    )?;
    let available = fs::read_to_string("/proc/meminfo")?
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_ascii_whitespace().next()?.parse::<i64>().ok())
        .map_or(limit, |kib| kib * 1024);
    require(
        available >= estimate,
        "BUDGET",
        "insufficient host memory for policy estimate",
    )?;
    let _io = io_projection(&inputs, &sdc)?;
    let _pins = pin_views(&inputs, &corners)?;
    let document = inputs.graph(source_json)?;
    let top = text(&policy, "top")?;
    // The bundle built the graph from the same decoded netlist JSON, pin tables and IO
    // projection, with the same budgets, after the same work: its work is charged here.
    require(
        bundled.top == top
            && std::sync::Arc::ptr_eq(&bundled.graph.doc, &document)
            && bundled.work.0 == inputs.work.units(),
        "INPUT",
        "reused bundle state belongs to another input bundle",
    )?;
    let graph = bundled.graph;
    inputs.tick(bundled.work.1 - bundled.work.0)?;
    let mut reports = Vec::new();
    for co in &corners {
        reports.push(inputs.report(co, "drv", &timed, &sdc)?);
    }
    let primary = text(&policy, "primary_clock")?;
    for x in &reports {
        let completeness = x
            .completeness
            .as_ref()
            .ok_or_else(|| super::context::key_error("completeness"))?;
        let domains = at(completeness, "clock_domains")?.as_list().unwrap_or(&[]);
        require(
            domains.iter().any(|d| d.as_str() == Some(primary)),
            "REPORT_SCOPE",
            "primary clock differs from compiled reports",
        )?;
    }
    let mut models = Vec::new();
    for x in &reports {
        models.push(at(scope_of(x)?, "wire_model")?.as_str().unwrap_or(""));
    }
    let ideal = models.iter().all(|m| *m == "ideal");
    require(
        ideal || models.iter().all(|m| *m == "spef_rc"),
        "REPORT_SCOPE",
        "mixed or unsupported wire models",
    )?;
    let policy_scope = text(&policy, "scope")?;
    let mut expected = if ideal { "ideal_diagnostic" } else { "spef_rc" };
    if expected == "spef_rc" && policy_scope == "estimated_rc" {
        require(
            manifest.get("placement").is_some() && manifest.get("rc_policy").is_some(),
            "REPORT_SCOPE",
            "estimated RC scope needs its placement and estimate policy",
        )?;
        expected = "estimated_rc";
    }
    require(
        policy_scope == expected,
        "REPORT_SCOPE",
        "policy does not match actual analysis scope",
    )?;
    require(
        operation != "electrical" || expected == "estimated_rc",
        "REPORT_SCOPE",
        "electrical repair needs estimated RC reports",
    )?;
    super::trace("stage: checked");
    let mut planner = Planner::new(&inputs, &policy, &graph)?;
    let mut extra = Json::Object(Vec::new());
    let placement;
    match operation {
        "reset" => planner.reset()?,
        "fanout" => planner.fanout_repair(&reports)?,
        "primary-clock" => planner.primary()?,
        _ => {
            // The frontend plans the electrical and hold rounds on estimated RC; the ideal
            // hold and the extracted hold of the reference planner are not rounds of it.
            require(
                expected == "estimated_rc",
                "UNSUPPORTED",
                "ideal and extracted hold are planned by the reference planner (native_ecos.py)",
            )?;
            placement = PlacementFiles::read(&inputs)?;
            let mut geometry = Geometry::new(&inputs, &placement)?;
            let timing = |kind: &str| -> Res<Vec<std::sync::Arc<Report>>> {
                let mut out = Vec::new();
                for co in &corners {
                    out.push(inputs.report(co, kind, &timed, &sdc)?);
                }
                Ok(out)
            };
            let mut balance = None;
            let balanced = at(&policy, "electrical")?.get("clock_balance").is_some();
            if operation == "electrical" && balanced {
                let holds = timing("hold_fix")?;
                let setups = timing("endpoints")?;
                balance = Some(clock_balance(
                    &mut planner,
                    &holds,
                    &setups,
                    &corners,
                    &mut geometry,
                )?);
            }
            // The hold round keeps a smaller margin: every repair may speed up hold paths,
            // which the hold plan then charges.
            extra = electrical(&mut planner, &reports, &mut geometry, operation == "hold")?;
            if let Some(balance) = balance {
                extra.set("clock_balance", balance);
            }
            if operation == "hold" {
                let holds = timing("hold_fix")?;
                let setups = timing("endpoints")?;
                let hold = estimated_hold(&mut planner, &holds, &setups, &corners, &mut geometry)?;
                extra.set("hold", hold);
                if policy.get("port_diode").is_some() {
                    planner.port_diodes(|name, cell, label| {
                        let at_port = geometry.pin(None, label)?;
                        geometry.place(name, cell, at_port)
                    })?;
                }
            }
            let seed = geometry.write_seed(
                &planner.ops,
                &staging.join("seed.json"),
                operation == "hold",
            )?;
            extra.set("seed", seed);
            let mut weights: std::collections::BTreeMap<String, Json> = Default::default();
            let place = at(manifest, "placement")?;
            if let Some(record) = place.get("weights") {
                let prior = inputs.document(record, 64 << 20)?;
                require(
                    prior.get("schema").and_then(Json::as_str) == Some("rustroad.net-weights.v1"),
                    "SCHEMA",
                    "prior net weights schema",
                )?;
                for (k, v) in at(&prior, "weights")?.as_object().unwrap_or(&[]) {
                    weights.insert(k.clone(), v.clone());
                }
            }
            for (k, v) in &planner.weights {
                weights.insert(k.clone(), Json::Int((*v).into()));
            }
            let (total, new) = (weights.len(), planner.weights.len());
            let doc = obj(vec![
                ("schema", Json::str("rustroad.net-weights.v1")),
                (
                    "source",
                    obj(vec![
                        ("operation", Json::str(operation)),
                        (
                            "rule",
                            Json::str("repair buffers stay on their short electrical side"),
                        ),
                        ("carried", Json::Int(total as i128 - new as i128)),
                    ]),
                ),
                ("weights", Json::Object(weights.into_iter().collect())),
            ]);
            fs::write(staging.join("weights.json"), doc.dumps(Some(1)) + "\n")?;
            extra.set(
                "weights",
                obj(vec![
                    ("total", Json::Int(total as i128)),
                    ("new", Json::Int(new as i128)),
                ]),
            );
        }
    }
    let plan = obj(vec![
        ("schema", Json::str("rustroad.eco-plan.v1")),
        ("module", Json::str(top)),
        (
            "operations",
            Json::List(planner.ops.iter().map(|op| op.to_json()).collect()),
        ),
    ]);
    fs::write(staging.join("plan.json"), plan.dumps(Some(2)) + "\n")?;
    super::trace("stage: planned");
    let profiles = at(manifest, "profiles")?;
    if args.plan_only {
        checked_apply(c, &inputs.files(), || Ok(()))?;
    } else {
        let mut cmd = vec![
            show(&inputs.tool("rustroad")?),
            "apply-eco".to_string(),
            "--netlist".to_string(),
            show(&source),
            "--plan".to_string(),
            show(&staging.join("plan.json")),
            "--delay-cells".to_string(),
            show(&inputs.file64(at(profiles, "delay")?)?),
            "--cell-lef".to_string(),
            show(&inputs.file64(at(profiles, "lef")?)?),
            "--sizing-profile".to_string(),
            show(&inputs.file64(at(profiles, "sizing")?)?),
            "--max-eco-operations".to_string(),
            inputs.args.max_operations.to_string(),
            "--max-input-mib".to_string(),
            "16".to_string(),
            "--eco-memory-mib".to_string(),
            inputs.args.memory_mib.to_string(),
            "--out".to_string(),
            show(&staging.join("netlist.v")),
        ];
        if let Some(buses) = profiles.get("cell_buses") {
            cmd.push("--cell-buses".to_string());
            cmd.push(show(&inputs.file(buses, 2 << 20)?));
        }
        let files = inputs.files();
        checked_apply(c, &files, || inputs.command(&cmd, &staging, "apply-native"))?;
    }
    let files = inputs.files();
    super::trace("stage: inputs hashed again");
    // The planner is this executable.
    let planner_sha256 = c.digests.digest(&resolve(&std::env::current_exe()?)?)?;
    let plan_sha256 = sha_fresh(&staging.join("plan.json"))?.0;
    let netlist = staging.join("netlist.v");
    let netlist_sha256 = if netlist.exists() {
        Json::Str(sha_fresh(&netlist)?.0)
    } else {
        Json::Null
    };
    let mut corner_scopes = Vec::new();
    for (co, r) in corners.iter().zip(&reports) {
        corner_scopes.push((co.clone(), scope_of(r)?.clone()));
    }
    let operations = planner.ops.len();
    let status = if args.plan_only {
        "planned_unverified"
    } else {
        "applied_requires_retiming"
    };
    let elapsed = started.elapsed().as_secs_f64();
    let peak = peak_rss_kib();
    let receipt = obj(vec![
        ("schema", Json::str("rusteda.native-eco-receipt.v1")),
        ("rev", Json::str("rev1")),
        ("operation", Json::str(operation)),
        ("planner_sha256", Json::Str(planner_sha256)),
        ("status", Json::str(status)),
        ("scope", Json::str(expected)),
        ("corner_scopes", Json::Object(corner_scopes)),
        (
            "inputs",
            Json::Object(files.into_iter().map(|(k, v)| (k, Json::Str(v))).collect()),
        ),
        ("operations", Json::Int(operations as i128)),
        ("checked", Json::List(std::mem::take(&mut planner.checks))),
        (
            "excluded",
            Json::List(std::mem::take(&mut planner.excluded)),
        ),
        ("extra", extra),
        ("budgets", inputs.args.to_json()),
        ("memory_estimate_bytes", Json::Int(estimate.into())),
        ("work_units", Json::Int(inputs.work.units().into())),
        ("elapsed_seconds", Json::Float(elapsed)),
        ("peak_rss_kib", Json::Int(peak.into())),
        ("plan_sha256", Json::Str(plan_sha256.clone())),
        ("netlist_sha256", netlist_sha256.clone()),
        (
            "signoff",
            obj(vec![
                ("CEC", Json::str("unverified")),
                ("DRC", Json::str("unverified")),
                ("LVS", Json::str("unverified")),
                ("post_edit_timing", Json::str("unverified")),
            ]),
        ),
    ]);
    fs::write(staging.join("receipt.json"), receipt.dumps(Some(2)) + "\n")?;
    for entry in fs::read_dir(&staging)? {
        let entry = entry?;
        fs::rename(entry.path(), workspace.join(entry.file_name()))?;
    }
    fs::remove_dir(&staging)?;
    let head = workspace.join(".HEAD.pending");
    fs::write(&head, "rev1\n")?;
    fs::rename(&head, workspace.join("HEAD"))?;
    drop(lock);
    super::trace("stage: committed");
    let summary = obj(vec![
        ("schema", Json::str("rusteda.native-eco-receipt.v1")),
        ("rev", Json::str("rev1")),
        ("status", Json::str(status)),
        ("scope", Json::str(expected)),
        ("operations", Json::Int(operations as i128)),
        ("plan_sha256", Json::Str(plan_sha256)),
        ("netlist_sha256", netlist_sha256),
        ("elapsed_seconds", Json::Float(elapsed)),
        ("peak_rss_kib", Json::Int(peak.into())),
    ]);
    Ok(summary.dumps(None) + "\n")
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The native frontend commands (RCLI-REQ-0027, RCLI-REQ-0028) against the reference helpers of
//! the RustEDA Ibex recipe (`native_frontend_inputs.py`, `native_ecos.py`), byte for byte, on a
//! small frontend run: `frontend-profiles`, `frontend-physical`, `frontend-pins`, the `fanout`
//! round (ideal wires) and the `hold` round (estimated RC) of `eco-round --plan-only`, and
//! `frontend-validate`.
//!
//! `fixtures/flow/reference.py` wrote the fixture and, with the reference helpers, the expected
//! files. Paths and the digests of files that hold paths are placeholders there (see its
//! `normalize`); the receipt's run time, peak memory and planner hash are not compared. The ECO
//! rounds check their reports against the pinned `xxhsum`: they run when `RUSTEDA_TEST_XXHSUM`
//! names it or `xxhsum` is on `PATH`, and are skipped otherwise.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A run of the fixture: its directories and the pinned hash tool.
struct Run {
    fix: PathBuf,
    base: PathBuf,
    root: PathBuf,
    xxhsum: PathBuf,
    /// Whether `xxhsum` is the real tool (the ECO rounds need it).
    hashed: bool,
}

const FILES: [(&str, &str); 18] = [
    ("sdc", "pdk/constraints.sdc"),
    ("std_tt", "pdk/std_tt.lib"),
    ("std_ss", "pdk/std_ss.lib"),
    ("ram_tt", "pdk/ram_tt.lib"),
    ("ram_ss", "pdk/ram_ss.lib"),
    ("cell_lef", "pdk/cells.lef"),
    ("std_lef", "pdk/cells.lef"),
    ("floorplan", "recipe/floorplan.json"),
    ("physical", "recipe/physical.json"),
    ("routing", "recipe/routing.json"),
    ("interfaces", "recipe/interfaces.json"),
    ("ecos", "recipe/ecos.py"),
    ("prepare", "recipe/prepare.py"),
    ("delay_helper", "recipe/delay_helper.py"),
    ("sizing_helper", "recipe/sizing_helper.py"),
    ("liberty_reader", "recipe/liberty_lite.py"),
    ("estimator", "recipe/estimator.py"),
    ("rc_policy", "recipe/rc-policy.json"),
];

fn find_xxhsum() -> Option<PathBuf> {
    if let Some(p) = env::var_os("RUSTEDA_TEST_XXHSUM") {
        return fs::canonicalize(p).ok();
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join("xxhsum"))
        .find(|p| p.is_file())
        .and_then(|p| fs::canonicalize(p).ok())
}

/// A file record as the composer of a run writes it: `path`, `sha256`, `bytes`, in this order.
fn record(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    format!(
        "{{\"path\": {}, \"sha256\": \"{}\", \"bytes\": {}}}",
        Value::from(path.to_str().unwrap()),
        sha256(&bytes),
        bytes.len()
    )
}

fn copy_tree(from: &Path, to: &Path, run: &str, fix: &str) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target, run, fix);
        } else {
            let text = fs::read_to_string(entry.path()).unwrap();
            fs::write(target, text.replace("@RUN@", run).replace("@FIX@", fix)).unwrap();
        }
    }
}

impl Run {
    /// Copies the run tree of the fixture to a fresh directory and writes its context.
    fn new(label: &str) -> Run {
        let fix =
            fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/flow"))
                .unwrap();
        let tmp = fs::canonicalize(env::temp_dir()).unwrap();
        let base = tmp.join(format!("rusteda-flow-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("run");
        copy_tree(
            &fix.join("run"),
            &root,
            root.to_str().unwrap(),
            fix.to_str().unwrap(),
        );
        // The raw Liberty copies of the two STA workspaces.
        for tag in ["ideal", "est"] {
            let cache = root.join(format!("frontend/sta-{tag}/cache"));
            fs::create_dir_all(&cache).unwrap();
            for co in ["tt", "ss"] {
                for kind in ["std", "ram"] {
                    fs::copy(
                        fix.join(format!("pdk/{kind}_{co}.lib")),
                        cache.join(format!("{kind}_{co}-0000000000000000.lib")),
                    )
                    .unwrap();
                }
            }
        }
        let (xxhsum, hashed) = match find_xxhsum() {
            Some(tool) => (tool, true),
            None => {
                // A stand-in: the helpers only pin the tool, the ECO rounds are skipped.
                let stand_in = base.join("xxhsum");
                fs::write(&stand_in, "stand-in of the pinned xxhsum\n").unwrap();
                (stand_in, false)
            }
        };
        // The context in the key order of the composer (the helpers copy its records).
        let files: Vec<String> = FILES
            .iter()
            .map(|(k, rel)| format!("\"{k}\": {}", record(&fix.join(rel))))
            .collect();
        let context = format!(
            "{{\"schema\": \"rusteda.native-frontend-context.v1\", \"run_root\": {}, \"files\": {{{}}}, \
             \"tools\": {{\"rustroad\": {}, \"xxhsum\": {}}}, \"selection\": {}}}\n",
            Value::from(root.to_str().unwrap()),
            files.join(", "),
            record(&fix.join("recipe/rustroad")),
            record(&xxhsum),
            fs::read_to_string(fix.join("recipe/selection.json")).unwrap()
        );
        fs::write(root.join("frontend/context.json"), context).unwrap();
        Run {
            fix,
            base,
            root,
            xxhsum,
            hashed,
        }
    }

    fn rusteda(&self, command: &str, workspace: &str, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rusteda"))
            .arg(command)
            .arg("--context")
            .arg(self.root.join("frontend/context.json"))
            .arg("--workspace")
            .arg(self.root.join(workspace))
            .args(["--base-rev", "empty", "--threads", "2"])
            .args(extra)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, command: &str, workspace: &str, extra: &[&str]) -> Output {
        let out = self.rusteda(command, workspace, extra);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{command}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).to_str().unwrap().to_string()
    }

    /// The files whose bytes hold a path, by the label of their placeholders.
    fn volatile(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![
            ("xxhsum".to_string(), self.xxhsum.clone()),
            (
                "roots".into(),
                self.root.join("frontend/bundle-reset/roots.json"),
            ),
            (
                "routing".into(),
                self.root.join("frontend/physical/routing.json"),
            ),
            (
                "pins-receipt".into(),
                self.root.join("frontend/physical-pins/receipt.json"),
            ),
            ("cache".into(), self.root.join("frontend/sha256-cache.json")),
        ];
        for tag in ["ideal", "est"] {
            out.push((
                format!("{tag}-meta"),
                self.root
                    .join(format!("frontend/sta-{tag}/revisions/rev0002/meta.json")),
            ));
        }
        for tag in ["ideal", "est", "final"] {
            for co in ["tt", "ss"] {
                out.push((
                    format!("{tag}-drv-{co}"),
                    self.root.join(format!("logs/fe_{tag}_{co}_drv.stdout")),
                ));
            }
        }
        for co in ["tt", "ss"] {
            out.push((
                format!("est-endpoints-{co}"),
                self.root.join(format!("logs/fe_est_{co}_endpoints.stdout")),
            ));
        }
        out.push(("est-hold".into(), self.root.join("logs/fe_est_hold.stdout")));
        for op in ["fanout", "hold"] {
            out.push((
                format!("{op}-inputs"),
                self.root.join(format!("frontend/bundle-{op}/inputs.json")),
            ));
        }
        out
    }

    /// `normalize` of `reference.py`: digests of the volatile files, paths, and the receipt's
    /// run-dependent fields as placeholders.
    fn normalize(&self, mut text: String) -> String {
        for (label, path) in self.volatile() {
            let Ok(bytes) = fs::read(&path) else { continue };
            text = text.replace(&sha256(&bytes), &format!("@SHA:{label}@"));
            let key = format!("\"sha256\": \"@SHA:{label}@\",\n");
            let size = bytes.len().to_string();
            let mut out = String::with_capacity(text.len());
            let mut rest = text.as_str();
            while let Some(k) = rest.find(&key) {
                let (head, tail) = rest.split_at(k + key.len());
                out.push_str(head);
                let indent = tail.len() - tail.trim_start().len();
                let field = &tail[indent..];
                let number = field.strip_prefix("\"bytes\": ").and_then(|v| {
                    let digits = v.len() - v.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                    (v[..digits] == size).then_some(digits)
                });
                match number {
                    Some(digits) => {
                        out.push_str(&tail[..indent]);
                        out.push_str(&format!("\"bytes\": \"@BYTES:{label}@\""));
                        rest = &field["\"bytes\": ".len() + digits..];
                    }
                    None => rest = tail,
                }
            }
            out.push_str(rest);
            text = out;
        }
        text = text
            .replace(self.root.to_str().unwrap(), "@RUN@")
            .replace(self.fix.to_str().unwrap(), "@FIX@")
            .replace(self.xxhsum.to_str().unwrap(), "@XXHSUM@");
        for (key, mark) in [
            ("elapsed_seconds", "@ELAPSED@"),
            ("peak_rss_kib", "@PEAK@"),
            ("planner_sha256", "@PLANNER@"),
        ] {
            let start = format!("\n  \"{key}\": ");
            if let Some(k) = text.find(&start) {
                let from = k + start.len();
                let len = text[from..].find([',', '\n']).unwrap();
                text.replace_range(from..from + len, &format!("\"{mark}\""));
            }
        }
        text
    }

    /// Requires that the workspace holds exactly the expected files of the case, byte for byte
    /// after `normalize`.
    fn same(&self, case: &str, workspace: &str) {
        let expected = self.fix.join("expected").join(case);
        let actual = self.root.join(workspace);
        let names = |dir: &Path| {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n != "workspace.lock")
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&actual), names(&expected), "files of {case}");
        for name in names(&expected) {
            if name == "digest-check.json" {
                continue;
            }
            let text = self.normalize(fs::read_to_string(actual.join(&name)).unwrap());
            assert_eq!(
                text,
                fs::read_to_string(expected.join(&name)).unwrap(),
                "{case}/{name}"
            );
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn round(run: &Run, op: &str, tag: &str) {
    let (bundle, plan) = (
        format!("frontend/bundle-{op}"),
        format!("frontend/plan-{op}"),
    );
    let out = run.ok(
        "eco-round",
        &plan,
        &[
            "--operation",
            op,
            "--timing-tag",
            tag,
            "--source",
            &run.path("frontend/mini.v"),
            "--graph",
            &run.path("frontend/mini-graph.json"),
            "--timed",
            &run.path(&format!("logs/fe_{tag}_export.stdout")),
            "--bundle",
            &run.path(&bundle),
            "--plan-only",
        ],
    );
    run.same(&format!("bundle-{op}"), &bundle);
    run.same(&format!("plan-{op}"), &plan);
    // The summary line is the receipt's.
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(run.root.join(&plan).join("receipt.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["status"], "planned_unverified");
    assert_eq!(summary["netlist_sha256"], Value::Null);
    for key in ["schema", "rev", "scope", "operations", "plan_sha256"] {
        assert_eq!(summary[key], receipt[key], "{key}");
    }
    assert_eq!(
        summary["plan_sha256"],
        sha256(&fs::read(run.root.join(&plan).join("plan.json")).unwrap())
    );
}

#[test]
fn the_commands_write_the_reference_helpers_files_byte_for_byte() {
    let run = Run::new("reference");
    run.ok("frontend-profiles", "frontend/profiles-out", &[]);
    run.same("profiles", "frontend/profiles-out");
    run.ok(
        "frontend-physical",
        "frontend/physical",
        &["--roots", &run.path("frontend/bundle-reset/roots.json")],
    );
    run.same("physical", "frontend/physical");
    let pins = run.ok(
        "frontend-pins",
        "frontend/physical-pins",
        &["--def", &run.path("logs/fe_pins_def.stdout")],
    );
    assert!(pins.stdout.is_empty());
    run.same("physical-pins", "frontend/physical-pins");
    if run.hashed {
        round(&run, "fanout", "ideal");
        round(&run, "hold", "est");
    } else {
        eprintln!("xxhsum not found (RUSTEDA_TEST_XXHSUM, PATH): the ECO rounds are skipped");
    }
    run.ok("frontend-validate", "frontend/result", &[]);
    run.same("result", "frontend/result");
    // The final digest check lists the pinned inputs whose digests the run shared through its
    // flow digest cache, each hashed again in full. Which inputs are listed depends on the file
    // system and on the age of the files (only settled files of local file systems are), so
    // the list is checked against the files themselves.
    let check: Value = serde_json::from_str(
        &fs::read_to_string(run.root.join("frontend/result/digest-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(check["schema"], "rusteda.frontend-digest-check.v1");
    for (path, digest) in check["checked"].as_object().unwrap() {
        assert_eq!(digest, &sha256(&fs::read(path).unwrap()), "{path}");
    }
    if let Some(cache) = check["cache"].as_object() {
        let bytes = fs::read(cache["path"].as_str().unwrap()).unwrap();
        assert_eq!(cache["sha256"], sha256(&bytes));
        assert_eq!(cache["bytes"], bytes.len());
    }
}

#[test]
fn a_used_workspace_and_a_changed_pinned_input_are_refused() {
    let run = Run::new("refused");
    run.ok("frontend-profiles", "frontend/profiles-out", &[]);
    // A second helper into the same, no longer fresh, workspace.
    let again = run.rusteda("frontend-profiles", "frontend/profiles-out", &[]);
    assert_eq!(again.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&again.stderr).unwrap();
    assert_eq!(error["status"], "failed");
    assert_eq!(error["code"], "REVISION_CONFLICT");
    // An unknown round is refused before anything is written.
    let unknown = run.rusteda(
        "eco-round",
        "frontend/plan-x",
        &[
            "--operation",
            "resize",
            "--timing-tag",
            "ideal",
            "--source",
            &run.path("frontend/mini.v"),
            "--graph",
            &run.path("frontend/mini-graph.json"),
            "--bundle",
            &run.path("frontend/bundle-x"),
        ],
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert!(!run.root.join("frontend/bundle-x").exists());
    // A context whose record no longer matches the pinned file.
    let context = run.root.join("frontend/context.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&context).unwrap()).unwrap();
    doc["files"]["sdc"]["sha256"] = json!("0".repeat(64));
    fs::write(&context, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let changed = run.rusteda("frontend-profiles", "frontend/profiles-2", &[]);
    assert_eq!(changed.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&changed.stderr).unwrap();
    assert_eq!(error["error"], "pinned input changed: sdc");
    assert!(!run.root.join("frontend/profiles-2").exists());
}

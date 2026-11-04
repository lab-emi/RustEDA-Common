// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! `rusteda import --lib-cache` (RCLI-REQ-0025): a library compiled by an earlier import is
//! linked into the next workspace instead of parsed again, and every import gets the same
//! envelope, revision and cache files as without the option.

use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "rusteda-lib-cache-{}-{label}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `rusteda --format json ARGS`: exit code and stdout.
fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rusteda"))
        .args(["--format", "json"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
    )
}

// One unknown group (a warning with a location) and two cells.
const LIB_A: &str = r#"library (cells_a) {
  delay_model : table_lookup;
  time_unit : "1ns";
  voltage_unit : "1V";
  current_unit : "1mA";
  pulling_resistance_unit : "1kohm";
  capacitive_load_unit (1, pf);
  char_config () { }
  cell (INVX1) {
    pin (A) { direction : input; capacitance : 0.01; }
    pin (Y) { direction : output; function : "(!A)"; }
  }
  cell (BUFX1) {
    pin (A) { direction : input; capacitance : 0.02; }
    pin (Y) { direction : output; function : "A"; }
  }
}
"#;

const LIB_B: &str = r#"library (cells_b) {
  delay_model : table_lookup;
  time_unit : "1ns";
  voltage_unit : "1V";
  current_unit : "1mA";
  pulling_resistance_unit : "1kohm";
  capacitive_load_unit (1, pf);
  cell (NAND2X1) {
    pin (A) { direction : input; capacitance : 0.01; }
    pin (B) { direction : input; capacitance : 0.01; }
    pin (Y) { direction : output; function : "(!(A & B))"; }
  }
}
"#;

// A second library that defines INVX1 again.
const LIB_DUP: &str = r#"library (cells_dup) {
  delay_model : table_lookup;
  time_unit : "1ns";
  voltage_unit : "1V";
  current_unit : "1mA";
  pulling_resistance_unit : "1kohm";
  capacitive_load_unit (1, pf);
  cell (INVX1) {
    pin (A) { direction : input; capacitance : 0.01; }
    pin (Y) { direction : output; function : "(!A)"; }
  }
}
"#;

struct Fixture {
    root: PathBuf,
    libs: Vec<(PathBuf, &'static str)>,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let root = temp_dir(label);
        let mut libs = Vec::new();
        for (name, text, corner) in [
            ("a.lib", LIB_A, "tt"),
            ("b.lib", LIB_B, "tt"),
            ("a.lib", LIB_A, "ss"),
        ] {
            let path = root.join(name);
            fs::write(&path, text).unwrap();
            libs.push((path, corner));
        }
        Fixture { root, libs }
    }

    /// Imports the fixture's libraries into a new workspace `name`, with `cache` if given:
    /// exit code, envelope text with the workspace path replaced by `WS`, and the workspace.
    fn import(
        &self,
        name: &str,
        cache: Option<&Path>,
        extra: &[(PathBuf, &str)],
    ) -> (i32, String, PathBuf) {
        let ws = self.root.join(name);
        let ws_s = ws.to_str().unwrap().to_string();
        assert_eq!(run(&["init", "-w", &ws_s]).0, 0);
        let mut args: Vec<String> = ["import", "-w", &ws_s, "--base-rev", "rev0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(c) = cache {
            args.extend(["--lib-cache".to_string(), c.to_str().unwrap().to_string()]);
        }
        for (path, corner) in self.libs.iter().chain(extra) {
            args.extend([
                "--lib".to_string(),
                path.to_str().unwrap().to_string(),
                "--corner".to_string(),
                corner.to_string(),
            ]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out) = run(&refs);
        (code, out.replace(&ws_s, "WS"), ws)
    }
}

/// The cache files and committed tables of a workspace: (relative path, bytes).
fn files(ws: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for dir in ["cache", "revisions/rev0001"] {
        let mut names: Vec<_> = fs::read_dir(ws.join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            if p.file_name().unwrap() == "meta.json" {
                continue;
            }
            out.push((
                format!("{dir}/{}", p.file_name().unwrap().to_str().unwrap()),
                fs::read(&p).unwrap(),
            ));
        }
    }
    out
}

/// The library entries of rev1 (name, corner, hash, source, compiled).
fn libraries(ws: &Path) -> Value {
    let meta: Value =
        serde_json::from_slice(&fs::read(ws.join("revisions/rev0001/meta.json")).unwrap()).unwrap();
    meta["libraries"].clone()
}

fn ino(p: &Path) -> u64 {
    fs::metadata(p).unwrap().ino()
}

#[test]
fn shared_cache_gives_the_same_import_and_links_the_files() {
    let fx = Fixture::new("same");
    let cache = fx.root.join("lib-cache");
    let (code0, plain, ws0) = fx.import("plain", None, &[]);
    assert_eq!(code0, 0, "{plain}");
    let env: Value = serde_json::from_str(&plain).unwrap();
    assert_eq!(env["metrics"]["libraries"], 3);
    assert_eq!(env["metrics"]["library_cells"], 5);
    assert!(
        env["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["severity"] == "warning"),
        "the fixture has reader warnings: {plain}"
    );

    // The first import with the cache compiles and publishes; the second links.
    let (code1, first, ws1) = fx.import("first", Some(&cache), &[]);
    let (code2, second, ws2) = fx.import("second", Some(&cache), &[]);
    assert_eq!((code1, code2), (0, 0));
    assert_eq!(first, plain);
    assert_eq!(second, plain);
    assert_eq!(files(&ws1), files(&ws0));
    assert_eq!(files(&ws2), files(&ws0));
    assert_eq!(libraries(&ws1), libraries(&ws0));
    assert_eq!(libraries(&ws2), libraries(&ws0));
    let mut shared: Vec<String> = fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    shared.sort();
    assert_eq!(
        shared.len(),
        6,
        "two libraries, three files each: {shared:?}"
    );
    for entry in fs::read_dir(ws2.join("cache")).unwrap() {
        let p = entry.unwrap().path();
        let name = p.file_name().unwrap();
        assert_eq!(
            ino(&p),
            ino(&cache.join(name)),
            "{name:?} is linked from the shared cache"
        );
        assert_eq!(ino(&p), ino(&ws1.join("cache").join(name)));
    }
    let record: Value = serde_json::from_slice(
        &fs::read(
            cache.join(
                shared
                    .iter()
                    .find(|n| n.starts_with("cells_a-") && n.ends_with(".json"))
                    .unwrap(),
            ),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["schema"], "rusteda.lib-cache-record.v1");
    assert_eq!(record["cells"], serde_json::json!(["INVX1", "BUFX1"]));

    // A text-mode import with the cache prints what a plain one prints.
    let ws3 = fx.root.join("text");
    let ws4 = fx.root.join("text-plain");
    let mut outs = Vec::new();
    for (ws, with_cache) in [(&ws3, true), (&ws4, false)] {
        let w = ws.to_str().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_rusteda"))
            .args(["init", "-w", w])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rusteda"));
        cmd.args(["import", "-w", w, "--base-rev", "rev0"]);
        if with_cache {
            cmd.args(["--lib-cache", cache.to_str().unwrap()]);
        }
        for (path, corner) in &fx.libs {
            cmd.args(["--lib", path.to_str().unwrap(), "--corner", corner]);
        }
        let out = cmd.stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success());
        outs.push((
            String::from_utf8(out.stdout).unwrap().replace(w, "WS"),
            String::from_utf8(out.stderr).unwrap().replace(w, "WS"),
        ));
    }
    assert_eq!(outs[0], outs[1]);
}

#[test]
fn a_record_that_does_not_match_is_not_used() {
    let fx = Fixture::new("mismatch");
    let cache = fx.root.join("lib-cache");
    let (_, plain, ws0) = fx.import("plain", None, &[]);
    assert_eq!(fx.import("first", Some(&cache), &[]).0, 0);
    let record_path = fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_str().unwrap().contains("cells_a-") && p.extension().unwrap() == "json")
        .unwrap();
    // Records of another executable, other bytes, another source, or not parsable are not used;
    // the library is compiled again, and published again.
    let good: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    for (i, field, value) in [
        (0, "executable", Value::from("0000000000000000")),
        (1, "bytes", Value::from(1)),
        (2, "source", Value::from("/elsewhere/a.lib")),
        (3, "schema", Value::from("rusteda.lib-cache-record.v0")),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        fs::write(&record_path, serde_json::to_vec(&bad).unwrap()).unwrap();
        let (code, out, ws) = fx.import(&format!("bad{i}"), Some(&cache), &[]);
        assert_eq!(code, 0);
        assert_eq!(out, plain, "{field}");
        assert_eq!(files(&ws), files(&ws0), "{field}");
        let republished: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(
            republished, good,
            "a library compiled again is published again ({field})"
        );
    }
    fs::write(&record_path, b"not json").unwrap();
    let (code, out, ws) = fx.import("garbage", Some(&cache), &[]);
    assert_eq!((code, out), (0, plain.clone()));
    assert_eq!(files(&ws), files(&ws0));

    // Damaged shared files are not adopted: the library is compiled again from its bytes.
    let rlib = record_path.with_extension("rlib");
    let tmp = cache.join("tmp");
    fs::write(&tmp, b"damaged").unwrap();
    fs::rename(&tmp, &rlib).unwrap();
    let (code, out, ws) = fx.import("damaged", Some(&cache), &[]);
    assert_eq!((code, out), (0, plain));
    assert_eq!(files(&ws), files(&ws0));

    // The same bytes from another path are another source: their diagnostics name it.
    let copy = fx.root.join("copy.lib");
    fs::copy(&fx.libs[0].0, &copy).unwrap();
    let other = Fixture {
        root: fx.root.clone(),
        libs: vec![(copy, "tt")],
    };
    let (c1, with_cache, _) = other.import("copy-cache", Some(&cache), &[]);
    let (c2, without, _) = other.import("copy-plain", None, &[]);
    assert_eq!((c1, c2), (0, 0));
    assert_eq!(with_cache, without);
    assert!(with_cache.contains("copy.lib"));
}

#[test]
fn checks_reject_cached_libraries_as_parsed_ones() {
    let fx = Fixture::new("reject");
    let cache = fx.root.join("lib-cache");
    let dup = fx.root.join("dup.lib");
    fs::write(&dup, LIB_DUP).unwrap();
    assert_eq!(fx.import("warm", Some(&cache), &[(dup.clone(), "ff")]).0, 0);
    // INVX1 twice in corner tt: rejected, with or without the (now warm) cache.
    let extra = [(dup, "tt")];
    let (c1, cached, ws1) = fx.import("dup-cache", Some(&cache), &extra);
    let (c2, plain, _) = fx.import("dup-plain", None, &extra);
    assert_eq!((c1, c2), (1, 1));
    assert_eq!(cached, plain);
    assert!(cached.contains("L0055"), "{cached}");
    let manifest: Value =
        serde_json::from_slice(&fs::read(ws1.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["head"], "rev0");
    assert!(
        fs::read_dir(ws1.join("cache")).unwrap().next().is_none(),
        "a rejected import leaves the workspace cache empty, as without the cache"
    );

    // --lib-cache naming a file is an I/O error.
    let file = fx.root.join("plain-file");
    fs::write(&file, b"x").unwrap();
    let ws = fx.root.join("bad-dir");
    let ws_s = ws.to_str().unwrap();
    assert_eq!(run(&["init", "-w", ws_s]).0, 0);
    let (code, out) = run(&[
        "import",
        "-w",
        ws_s,
        "--base-rev",
        "rev0",
        "--lib-cache",
        file.to_str().unwrap(),
        "--lib",
        fx.libs[0].0.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("IO_ERROR"), "{out}");
}

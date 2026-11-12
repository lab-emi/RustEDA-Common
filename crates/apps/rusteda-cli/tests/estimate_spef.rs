// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! `rusteda estimate-spef` (RCLI-REQ-0026): the files and summary line of the reference
//! estimator (`integration/ibex_sg13g2_ez130/estimate_spef.py` of the RustEDA recipe), byte for
//! byte, on a fixture with oriented pins, power pins, unplaced connections, a long edge split
//! into segments, escaped names and two edges detoured around a `CLASS BLOCK` macro. The
//! expected files were written by the reference estimator from the same inputs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/estimate_spef")
}

fn workspace(label: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("rusteda-estimate-{}-{label}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn estimate(ws: &Path, policy: &Path) -> std::process::Output {
    let dir = fixture();
    Command::new(env!("CARGO_BIN_EXE_rusteda"))
        .arg("estimate-spef")
        .arg("--def")
        .arg(dir.join("placed.def"))
        .arg("--lef")
        .arg(dir.join("cells.lef"))
        .arg("--policy")
        .arg(policy)
        .arg("--workspace")
        .arg(ws)
        .args(["--base-rev", "empty", "--threads", "2"])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn estimate_matches_the_reference_estimator_byte_for_byte() {
    let dir = fixture();
    let ws = workspace("reference");
    let out = estimate(&ws, &dir.join("policy.json"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, fs::read(dir.join("expected.stdout")).unwrap());
    assert_eq!(
        fs::read(ws.join("estimate.spef")).unwrap(),
        fs::read(dir.join("expected.spef")).unwrap()
    );
    let expected = fs::read_to_string(dir.join("expected-result.json"))
        .unwrap()
        .replace("@DIR@", &dir.display().to_string())
        .replace("@WS@", &ws.display().to_string());
    assert_eq!(
        fs::read_to_string(ws.join("result.json")).unwrap(),
        expected
    );
    assert_eq!(fs::read_to_string(ws.join("HEAD")).unwrap(), "rev1\n");

    // A second estimate into the same, no longer fresh, workspace is refused.
    let again = estimate(&ws, &dir.join("policy.json"));
    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stderr).contains("REVISION_CONFLICT"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn a_policy_outside_its_bounds_is_refused() {
    let dir = fixture();
    let ws = workspace("bounds");
    fs::create_dir_all(&ws).unwrap();
    let policy = env::temp_dir().join(format!(
        "rusteda-estimate-{}-policy.json",
        std::process::id()
    ));
    let text = fs::read_to_string(dir.join("policy.json"))
        .unwrap()
        .replace("\"detour\": 1.25", "\"detour\": 4.0");
    fs::write(&policy, text).unwrap();
    let out = estimate(&ws.join("estimate"), &policy);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "{\"status\": \"failed\", \"error\": \"policy values outside qualified bounds\"}\n"
    );
    let _ = fs::remove_dir_all(&ws);
    let _ = fs::remove_file(&policy);
}

// SPDX-License-Identifier: Apache-2.0
//! Bounded native CLI fixtures; source only until root coordinates the CPU gate.
//! <=8KiB view/job and128KiB output/case; <=16 roots, <=2 simultaneous child
//! workers. Include complete child startup/hash/mmap/compute/exit, not kernels.
use rusteda_rc_crossing_g1::{
    Grid, Instance, Limits, Orientation, Query, Rect, Table, TemplateRect, View, format,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    root: PathBuf,
    input: PathBuf,
    job: PathBuf,
    job_sha: String,
    request: serde_json::Value,
}
fn hash(path: &Path) -> (String, u64) {
    let mut file = File::open(path).unwrap();
    let bytes = file.metadata().unwrap().len();
    assert!(bytes <= 128 << 20);
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = file.read(&mut b).unwrap();
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    (format!("{:x}", h.finalize()), bytes)
}
fn id(path: &Path) -> serde_json::Value {
    let (sha, bytes) = hash(path);
    serde_json::json!({"path":path,"sha256":sha,"bytes":bytes})
}
fn work_limit_with_margin(request: &mut serde_json::Value, fixed: u64, margin: i64) -> u64 {
    let mut value = fixed + 1024;
    // Only decimal job-length changes are resolved; baseline fixed work came
    // from an actual successful receipt, not a duplicate engine estimator.
    for _ in 0..4 {
        request["limits"]["max_work"] = serde_json::json!(value);
        let blocks = (serde_json::to_vec(request).unwrap().len() as u64).div_ceil(64);
        let next =
            u64::try_from(i128::from(fixed) + 6 * i128::from(blocks) + i128::from(margin)).unwrap();
        if next == value {
            return value;
        }
        value = next;
    }
    panic!("bounded job-length fixed point did not converge");
}
fn frozen(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
}
fn protocol() -> String {
    let mut h = Sha256::new();
    for v in [
        include_bytes!("../src/lib.rs").as_slice(),
        include_bytes!("../src/format.rs").as_slice(),
    ] {
        h.update((v.len() as u64).to_le_bytes());
        h.update(v);
    }
    format!("{:x}", h.finalize())
}
impl Fixture {
    fn new() -> Self {
        let index = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(index < 32);
        let root = std::env::temp_dir().join(format!(
            "rusteda-g1-component-{}-{index}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let input = root.join("input");
        fs::create_dir(&input).unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(input.join("workspace.lock"))
            .unwrap();
        frozen(&input.join("HEAD"), b"rev1\n");
        let rects = [
            TemplateRect {
                rect: Rect {
                    lx: 0,
                    ly: 0,
                    ux: 4,
                    uy: 2,
                },
                layer: 0,
                ordinal: 0,
            },
            TemplateRect {
                rect: Rect {
                    lx: 0,
                    ly: 2,
                    ux: 4,
                    uy: 4,
                },
                layer: 1,
                ordinal: 1,
            },
        ];
        let instances = [
            Instance {
                at: [0, 0],
                height: 4,
                template: 0,
                first: 0,
                bounds: Rect {
                    lx: 0,
                    ly: 0,
                    ux: 4,
                    uy: 4,
                },
                orientation: Orientation::N,
            },
            Instance {
                at: [10, 0],
                height: 4,
                template: 0,
                first: 2,
                bounds: Rect {
                    lx: 10,
                    ly: 0,
                    ux: 14,
                    uy: 4,
                },
                orientation: Orientation::Fs,
            },
        ];
        // Four-by-four grid [-1..2]^2: (x0,y0)=cell5, (x1,y0)=cell6.
        let offsets = [0, 0, 0, 0, 0, 0, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
        let entries = [0u32, 1];
        let ranges = [0u32, 2];
        let queries = [
            Query {
                rect: Rect {
                    lx: -1,
                    ly: -1,
                    ux: 2,
                    uy: 1,
                },
                layer: 0,
            },
            Query {
                rect: Rect {
                    lx: 10,
                    ly: -1,
                    ux: 14,
                    uy: 1,
                },
                layer: 1,
            },
        ];
        let view = View {
            grid: Grid {
                origin: [-1, -1],
                bin: 10,
                width: 4,
                height: 4,
            },
            layer_count: 2,
            virtual_count: 4,
            offsets: Table::Native(&offsets),
            entries: Table::Native(&entries),
            instances: Table::Native(&instances),
            template_offsets: Table::Native(&ranges),
            rectangles: Table::Native(&rects),
            queries: Table::Native(&queries),
        };
        let bytes = format::encode(view, Limits::default()).unwrap();
        assert!(bytes.len() < 8192);
        frozen(&input.join("view.rbin"), &bytes);
        let worker = PathBuf::from(env!("CARGO_BIN_EXE_rusteda-rc-crossing-g1"));
        let request = serde_json::json!({"schema":"rusteda.g1-csr-worker-inputs.v1","backend":"cpu_ref","view_workspace":input,"view_revision":"rev1",
            "view":id(&input.join("view.rbin")),"worker":id(&worker),"protocol_sha256":protocol(),"limits":Limits::default()});
        let job = input.join("job.json");
        let raw = serde_json::to_vec(&request).unwrap();
        assert!(raw.len() < 65536);
        frozen(&job, &raw);
        let (job_sha, _) = hash(&job);
        Self {
            root,
            input,
            job,
            job_sha,
            request,
        }
    }
    fn run(&self, output: &str, base: &str, sha: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rusteda-rc-crossing-g1"))
            .args([
                "--workspace",
                self.root.join(output).to_str().unwrap(),
                "--base-rev",
                base,
                "--inputs",
                self.job.to_str().unwrap(),
                "--inputs-sha",
                sha,
            ])
            .output()
            .unwrap()
    }
    fn new_job(&mut self, name: &str) {
        self.job = self.input.join(name);
        frozen(&self.job, &serde_json::to_vec(&self.request).unwrap());
        self.job_sha = hash(&self.job).0;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retained failed G1 fixture: {}", self.root.display());
        } else {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}
#[test]
fn real_worker_owns_private_workspace_commits_only_exact_bits_and_checks_base() {
    let f = Fixture::new();
    let output = f.run("cpu", "empty", &f.job_sha);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= 512 && output.stderr.is_empty());
    assert_eq!(fs::read(f.root.join("cpu/HEAD")).unwrap(), b"rev1\n");
    let bytes = fs::read(f.root.join("cpu/revisions/rev1/activation.rbin")).unwrap();
    assert_eq!(&bytes[..8], b"RG1ACT01");
    assert_eq!(
        u64::from_le_bytes(bytes[128..136].try_into().unwrap()),
        0b1001
    );
    let report_bytes = fs::read(f.root.join("cpu/revisions/rev1/report.json")).unwrap();
    assert_eq!(report_bytes.len(), 2048);
    let report: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();
    assert_eq!(report["legacy_road_binding"], "unchecked");
    assert_eq!(report["drc_status"], "unchecked");
    assert_eq!(
        report["work_authority"],
        "requires_process_exit0_and_HEAD_rev1"
    );
    let completed = report["io_completed_before_receipt"].as_u64().unwrap();
    let on_success = report["io_work_on_success"].as_u64().unwrap();
    // Tail consists of actual fixed receipt encoding+write, HEAD and stdout.
    let tail = 2 * (report_bytes.len() as u64).div_ceil(64)
        + (fs::read(f.root.join("cpu/HEAD")).unwrap().len() as u64).div_ceil(64)
        + (output.stdout.len() as u64).div_ceil(64);
    assert_eq!(on_success - completed, tail);
    assert!(on_success <= report["io_reserved_work"].as_u64().unwrap());
    assert_eq!(
        u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        report["total_work_on_success"].as_u64().unwrap()
    );
    let next = f.run("cpu", "empty", &f.job_sha);
    assert!(!next.status.success());
    assert!(String::from_utf8_lossy(&next.stderr).contains("REVISION_CONFLICT"));
    assert_eq!(
        fs::read(f.root.join("cpu/revisions/rev1/activation.rbin")).unwrap(),
        bytes
    );
}
#[test]
fn wrong_job_hash_unknown_gpu_and_locked_input_never_publish_results() {
    let mut f = Fixture::new();
    let bad = f.run("bad-sha", "empty", &"0".repeat(64));
    assert!(!bad.status.success());
    assert!(!f.root.join("bad-sha").exists());
    let lock = File::open(f.input.join("workspace.lock")).unwrap();
    lock.try_lock().unwrap();
    let busy = f.run("busy", "empty", &f.job_sha);
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("WORKSPACE_BUSY"));
    drop(lock);
    f.request["backend"] = serde_json::json!("cuda");
    f.job = f.input.join("gpu-job.json");
    frozen(&f.job, &serde_json::to_vec(&f.request).unwrap());
    f.job_sha = hash(&f.job).0;
    let gpu = f.run("gpu", "empty", &f.job_sha);
    assert!(!gpu.status.success());
    assert!(String::from_utf8_lossy(&gpu.stderr).contains("GPU_UNVERIFIED"));
    assert!(!f.root.join("gpu").exists());
}
#[test]
fn hardlink_and_writer_ancestor_alias_reject_without_foreign_writes() {
    let f = Fixture::new();
    fs::hard_link(f.input.join("view.rbin"), f.input.join("alias.rbin")).unwrap();
    let output = f.run("shared", "empty", &f.job_sha);
    assert!(!output.status.success());
    assert!(!f.root.join("shared").exists());
    fs::remove_file(f.input.join("alias.rbin")).unwrap();
    let foreign = f.root.join("foreign");
    fs::create_dir(&foreign).unwrap();
    std::os::unix::fs::symlink(&foreign, f.root.join("link")).unwrap();
    let output = f.run("link/escaped", "empty", &f.job_sha);
    assert!(!output.status.success());
    assert_eq!(fs::read_dir(foreign).unwrap().count(), 0);
}

#[test]
fn tight_io_budget_has_an_exact_early_boundary_and_never_publishes() {
    let mut f = Fixture::new();
    let baseline = f.run("baseline", "empty", &f.job_sha);
    assert!(baseline.status.success());
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(f.root.join("baseline/revisions/rev1/report.json")).unwrap(),
    )
    .unwrap();
    let observed = report["io_reserved_work"].as_u64().unwrap();
    let old_blocks = fs::metadata(&f.job).unwrap().len().div_ceil(64);
    let fixed = observed - 6 * old_blocks;
    let _boundary = work_limit_with_margin(&mut f.request, fixed, 0);
    f.new_job("exact-boundary.json");
    let exact = f.run("exact-boundary", "empty", &f.job_sha);
    assert!(!exact.status.success());
    assert!(String::from_utf8_lossy(&exact.stderr).contains("IO_WORK_BUDGET_BEFORE_LARGE_IO"));
    assert!(!f.root.join("exact-boundary").exists());
    work_limit_with_margin(&mut f.request, fixed, -1);
    f.new_job("below-boundary.json");
    let below = f.run("below-boundary", "empty", &f.job_sha);
    assert!(!below.status.success());
    assert!(String::from_utf8_lossy(&below.stderr).contains("IO_WORK_BUDGET_BEFORE_LARGE_IO"));
    assert!(!f.root.join("below-boundary").exists());
    work_limit_with_margin(&mut f.request, fixed, 1);
    f.new_job("one-core-unit.json");
    let after_io = f.run("one-core-unit", "empty", &f.job_sha);
    assert!(!after_io.status.success());
    assert!(String::from_utf8_lossy(&after_io.stderr).contains("BUDGET:aggregate work"));
    assert!(!f.root.join("one-core-unit/HEAD").exists());
    assert!(!f.root.join("one-core-unit/revisions").exists());
}

#[test]
fn low_cap_rejects_before_large_invalid_view_and_wrong_elf_hash_are_read() {
    let mut f = Fixture::new();
    let path = f.input.join("sparse-invalid16m.rbin");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(16 << 20).unwrap();
    drop(file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    // No test hash/read of this sparse witness. Its wrong content/SHA, and the
    // intentionally wrong ELF SHA, would fail if either large hash were reached.
    f.request["view"] = serde_json::json!({"path":path,"bytes":16 << 20,"sha256":"0".repeat(64)});
    f.request["worker"]["sha256"] = serde_json::json!("0".repeat(64));
    f.request["limits"]["max_work"] = serde_json::json!(0);
    f.new_job("low-before-large.json");
    let output = f.run("low-before-large", "empty", &f.job_sha);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("IO_WORK_BUDGET_BEFORE_LARGE_IO"));
    assert!(!f.root.join("low-before-large").exists());
}

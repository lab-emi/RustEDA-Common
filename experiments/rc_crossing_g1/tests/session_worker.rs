//! Authored CLI controls, currently unrun. No reference executable is invoked.
//! Largest packet here is<16KiB; ELF copying/hashing is streamed and capped128MiB.
use rusteda_rc_crossing_g1::session::SessionLimits;
use rusteda_rc_crossing_g1::session_format::{self as frame, TapeHeader, TapeLayout};
use rusteda_rc_crossing_g1::{
    Grid, Instance, Orientation, Query, Record, Rect, Table, TemplateRect, View,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

static NEXT: AtomicU64 = AtomicU64::new(0);
const RETAIN_ENV: &str = "RUSTEDA_G1_SESSION_TEST_ARTIFACT_ROOT";
const RAW_CAP: usize = 8192;

fn artifact_parent() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(RETAIN_ENV)?);
    assert!(path.is_absolute());
    let mut ancestor = PathBuf::new();
    for component in path.components() {
        assert!(!matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ));
        ancestor.push(component);
        assert!(
            !fs::symlink_metadata(&ancestor)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let info = fs::symlink_metadata(&path).unwrap();
    let uid = fs::symlink_metadata(format!("/proc/{}", std::process::id()))
        .unwrap()
        .uid();
    assert!(info.is_dir() && info.uid() == uid && info.permissions().mode() & 0o077 == 0);
    Some(path)
}

fn bounded_pipe(mut stream: impl Read) -> Vec<u8> {
    let mut raw = Vec::with_capacity(RAW_CAP + 1);
    Read::by_ref(&mut stream)
        .take((RAW_CAP + 1) as u64)
        .read_to_end(&mut raw)
        .unwrap();
    raw
}
fn file_id(path: &Path) -> Value {
    let size = path.metadata().unwrap().len();
    assert!(size <= 128 << 20);
    let mut file = File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut left = size;
    while left != 0 {
        let n = usize::try_from(left.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..n]).unwrap();
        digest.update(&buffer[..n]);
        left -= n as u64;
    }
    json!({"path":path,"bytes":size,"sha256":format!("{:x}",digest.finalize())})
}
fn readonly(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
}
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn protocol() -> String {
    let parts: [(&str, &[u8]); 6] = [
        ("lib.rs", include_bytes!("../src/lib.rs")),
        ("format.rs", include_bytes!("../src/format.rs")),
        ("session.rs", include_bytes!("../src/session.rs")),
        (
            "session_format.rs",
            include_bytes!("../src/session_format.rs"),
        ),
        ("session_json.rs", include_bytes!("../src/session_json.rs")),
        ("session_main.rs", include_bytes!("../src/bin/session.rs")),
    ];
    let mut hash = Sha256::new();
    for (name, source) in parts {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update((source.len() as u64).to_le_bytes());
        hash.update(source);
    }
    format!("{:x}", hash.finalize())
}
struct Fixture {
    root: PathBuf,
    input: PathBuf,
    revision: PathBuf,
    jobs: PathBuf,
    job: PathBuf,
    worker: PathBuf,
    request: Value,
    retain: bool,
    calls: AtomicU64,
}
impl Fixture {
    fn new() -> Self {
        let parent = artifact_parent();
        let root = parent
            .as_ref()
            .cloned()
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "rusteda-g1-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        directory(&root);
        let input = root.join("input");
        directory(&input);
        directory(&input.join("revisions"));
        let revision = input.join("revisions/rev1");
        directory(&revision);
        fs::write(input.join("HEAD"), b"rev1\n").unwrap();
        readonly(&input.join("HEAD"));
        fs::write(input.join("workspace.lock"), []).unwrap();
        readonly(&input.join("workspace.lock"));
        let rect = Rect {
            lx: 0,
            ly: 0,
            ux: 10,
            uy: 10,
        };
        let offsets = [0, 1];
        let entries = [0];
        let toff = [0, 1];
        let instances = [Instance {
            at: [0, 0],
            height: 10,
            template: 0,
            first: 0,
            bounds: rect,
            orientation: Orientation::N,
        }];
        let rectangles = [TemplateRect {
            rect,
            layer: 0,
            ordinal: 0,
        }];
        let view = View {
            grid: Grid {
                origin: [0, 0],
                bin: 1000,
                width: 1,
                height: 1,
            },
            layer_count: 1,
            virtual_count: 1,
            offsets: Table::Native(&offsets),
            entries: Table::Native(&entries),
            instances: Table::Native(&instances),
            template_offsets: Table::Native(&toff),
            rectangles: Table::Native(&rectangles),
            queries: Table::Native(&[]),
        };
        let statics = revision.join("static.rbin");
        fs::write(
            &statics,
            frame::encode_static(view, SessionLimits::default()).unwrap(),
        )
        .unwrap();
        readonly(&statics);
        let static_id = file_id(&statics);
        let mut static_sha = [0u8; 32];
        for (i, pair) in static_id["sha256"]
            .as_str()
            .unwrap()
            .as_bytes()
            .chunks_exact(2)
            .enumerate()
        {
            static_sha[i] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        let query = Query {
            rect: Rect {
                lx: 1,
                ly: 1,
                ux: 2,
                uy: 2,
            },
            layer: 0,
        };
        let mut payload = [0u8; 40];
        query.encode(&mut payload).unwrap();
        let header = TapeHeader {
            layout: TapeLayout::from_file_bytes(8192 + 40, SessionLimits::default()).unwrap(),
            static_sha,
            payload_sha: Sha256::digest(payload).into(),
        };
        let head = header.encode().unwrap();
        let footer = header.encode_footer(Sha256::digest(head).into()).unwrap();
        let mut tape = Vec::with_capacity(8192 + 40);
        tape.extend(head);
        tape.extend(payload);
        tape.extend(footer);
        let tape_path = revision.join("queries.rbin");
        fs::write(&tape_path, tape).unwrap();
        readonly(&tape_path);
        let origin = json!({"schema":"rusteda.g1-static-query-origin.v2","static_view":static_id,"query_tape":file_id(&tape_path),
            "observer_legacy_bits":null,"counts":{"instances":1,"templates":1,"rectangles":1,"virtual_shapes":1,"bins":1,"entries":1,"queries":1},
            "provenance":{"fixture":"one-positive-overlap; external40cases/nativebits still required"}});
        let origin_path = revision.join("origin.json");
        fs::write(&origin_path, serde_json::to_vec(&origin).unwrap()).unwrap();
        readonly(&origin_path);
        let bin = root.join("bin");
        directory(&bin);
        let worker = bin.join("worker");
        let compiled = Path::new(env!("CARGO_BIN_EXE_rusteda-rc-crossing-g1-session"));
        assert!(compiled.metadata().unwrap().len() <= 128 << 20);
        fs::copy(compiled, &worker).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o555)).unwrap();
        let request = json!({"schema":"rusteda.g1-csr-session-inputs.v2","backend":"cpu_ref","input_workspace":input,"input_revision":"rev1",
            "static_view":file_id(&statics),"query_tape":file_id(&tape_path),"worker":file_id(&worker),"protocol_sha256":protocol(),
            "origin":{"kind":"manual_fixture","record":file_id(&origin_path)},"limits":SessionLimits::default()});
        let jobs = root.join("jobs");
        directory(&jobs);
        let job = jobs.join("job.json");
        let mut this = Self {
            root,
            input,
            revision,
            jobs,
            job,
            worker,
            request,
            retain: parent.is_some(),
            calls: AtomicU64::new(0),
        };
        if this.retain {
            directory(&this.root.join("calls"));
        }
        this.write_job();
        this.seal_input();
        this
    }
    fn write_job(&mut self) {
        fs::set_permissions(&self.jobs, fs::Permissions::from_mode(0o700)).unwrap();
        if self.job.exists() {
            fs::set_permissions(&self.job, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::write(&self.job, serde_json::to_vec(&self.request).unwrap()).unwrap();
        readonly(&self.job);
        fs::set_permissions(&self.jobs, fs::Permissions::from_mode(0o500)).unwrap();
    }
    fn seal_input(&self) {
        for path in [&self.revision, self.revision.parent().unwrap(), &self.input] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
        }
    }
    fn invoke(&self, name: &str, base: &str) -> Output {
        self.invoke_at(&self.root.join(name), base)
    }
    fn invoke_at(&self, workspace: &Path, base: &str) -> Output {
        let mut command = Command::new(&self.worker);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--base-rev")
            .arg(base)
            .arg("--inputs")
            .arg(&self.job)
            .arg("--inputs-sha")
            .arg(file_id(&self.job)["sha256"].as_str().unwrap());
        if !self.retain {
            return command.output().unwrap();
        }
        let ordinal = self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(ordinal < 32);
        let output = self.root.join("calls").join(format!("call{ordinal}"));
        directory(&output);
        let before = file_id(&self.worker);
        let argv: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        assert!(argv.len() == 9 && argv.iter().all(|value| value.len() <= 4096));
        let cwd = std::env::current_dir().unwrap();
        let mut inputs = Vec::new();
        for (role, path) in [
            ("job.json", self.job.clone()),
            ("origin.json", self.revision.join("origin.json")),
            ("static.rbin", self.revision.join("static.rbin")),
            ("queries.rbin", self.revision.join("queries.rbin")),
        ] {
            let info = path.metadata().unwrap();
            assert!(info.len() <= 65536);
            let raw = fs::read(&path).unwrap();
            assert!(raw.len() as u64 == info.len());
            let snapshot = output.join(role);
            fs::write(&snapshot, raw).unwrap();
            readonly(&snapshot);
            inputs.push(json!({"actual":file_id(&path), "snapshot":file_id(&snapshot)}));
        }
        let record = output.join("command.json");
        fs::write(
            &record,
            serde_json::to_vec(&json!({"schema":"rusteda.g1-session-selftest-command.v1",
            "argv":argv,"cwd":cwd,"worker_before":before,"inputs":inputs,
            "scope":"protocol self-test only; independent geometry/native legacy unchecked"}))
            .unwrap(),
        )
        .unwrap();
        readonly(&record);
        let started = Instant::now();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout = thread::spawn(move || bounded_pipe(stdout));
        let stderr = thread::spawn(move || bounded_pipe(stderr));
        let status = child.wait().unwrap();
        let stdout = stdout.join().unwrap();
        let stderr = stderr.join().unwrap();
        let after = file_id(&self.worker);
        for (name, raw) in [("stdout", &stdout), ("stderr", &stderr)] {
            fs::write(output.join(name), raw).unwrap();
            readonly(&output.join(name));
        }
        let result = json!({"schema":"rusteda.g1-session-selftest-result.v1","actual_pid":pid,
            "exit_code":status.code(),"signal":status.signal(),"wall_seconds":started.elapsed().as_secs_f64(),
            "stdout":file_id(&output.join("stdout")),"stderr":file_id(&output.join("stderr")),
            "worker_after":after,"worker_unchanged":before==after,"bounded_outputs":stdout.len()<=RAW_CAP && stderr.len()<=RAW_CAP,
            "actual_cli_calls":1u64,"scope":"self-authored protocol test; not independent geometry or legacy proof"});
        fs::write(
            output.join("result.json"),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        readonly(&output.join("result.json"));
        fs::set_permissions(&output, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(before, after);
        assert!(stdout.len() <= RAW_CAP && stderr.len() <= RAW_CAP);
        Output {
            status,
            stdout,
            stderr,
        }
    }
    fn rewrite_origin(&mut self, raw: &[u8]) {
        let path = self.revision.join("origin.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, raw).unwrap();
        readonly(&path);
        self.request["origin"]["record"] = file_id(&path);
        self.write_job();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.retain {
            return;
        }
        fn unseal(path: &Path) {
            if let Ok(info) = fs::symlink_metadata(path)
                && info.is_dir()
                && !info.file_type().is_symlink()
            {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        unseal(&entry.path());
                    }
                }
            }
        }
        unseal(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn diagnostic(out: &Output) -> Value {
    assert_eq!(out.status.code(), Some(2));
    serde_json::from_slice(&out.stderr).unwrap()
}

#[test]
fn actual_success_requires_header_receipt_exit_and_head() {
    let fixture = Fixture::new();
    let out = fixture.invoke("output", "empty");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = fixture.root.join("output");
    assert_eq!(fs::read(root.join("HEAD")).unwrap(), b"rev1\n");
    let activation = fs::read(root.join("revisions/rev1/activation.rbin")).unwrap();
    assert_eq!(&activation[..8], b"RG1ACT02");
    assert_eq!(
        u64::from_le_bytes(activation[256..264].try_into().unwrap()),
        1
    );
    let receipt = fs::read(root.join("revisions/rev1/report.json")).unwrap();
    assert_eq!(receipt.len(), 4096);
    let report: Value = serde_json::from_slice(&receipt).unwrap();
    assert_eq!(report["actual_native_processes"].as_u64(), Some(1));
    assert_eq!(
        report["total_work_on_success"].as_u64(),
        Some(u64::from_le_bytes(activation[40..48].try_into().unwrap()))
    );
    assert_eq!(
        report["legacy_equivalence"],
        "unchecked_requires_independent_capture_verification"
    );
    assert_eq!(report["drc_status"], "unchecked");
}
#[test]
fn early_tight_budget_precedes_wrong_worker_hash_and_large_input_read() {
    let mut fixture = Fixture::new();
    fixture.request["limits"]["max_work"] = json!(0);
    fixture.request["worker"]["sha256"] = json!("0".repeat(64));
    fixture.write_job();
    let d = diagnostic(&fixture.invoke("not-created", "empty"));
    assert!(
        d["diagnostic"]
            .as_str()
            .unwrap()
            .contains("before large IO")
    );
    assert!(!fixture.root.join("not-created").exists());
}
#[test]
fn existing_revision_conflicts_and_partial_outputs_are_never_overwritten() {
    let fixture = Fixture::new();
    let output = fixture.root.join("existing");
    directory(&output);
    fs::write(output.join("workspace.lock"), []).unwrap();
    fs::write(output.join("HEAD"), b"rev1\n").unwrap();
    for base in ["empty", "rev2"] {
        assert!(
            diagnostic(&fixture.invoke("existing", base))["diagnostic"]
                .as_str()
                .unwrap()
                .contains("REVISION_CONFLICT")
        );
    }
    assert_eq!(fs::read(output.join("HEAD")).unwrap(), b"rev1\n");
    let partial = fixture.root.join("partial");
    directory(&partial);
    fs::write(partial.join("workspace.lock"), []).unwrap();
    fs::write(partial.join("unpublished"), b"keep").unwrap();
    assert!(
        diagnostic(&fixture.invoke("partial", "empty"))["diagnostic"]
            .as_str()
            .unwrap()
            .contains("NOT_FRESH")
    );
    assert_eq!(fs::read(partial.join("unpublished")).unwrap(), b"keep");
}
#[test]
fn alias_is_rejected_before_creating_any_input_writer() {
    let fixture = Fixture::new();
    let intended = fixture.input.join("must-not-create");
    let out = fixture.invoke_at(&intended, "empty");
    assert!(
        diagnostic(&out)["diagnostic"]
            .as_str()
            .unwrap()
            .contains("INPUT_OUTPUT_ALIAS")
    );
    assert!(!intended.exists());
}
#[test]
fn unknown_inventory_is_not_ignored() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.input, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(fixture.input.join("unknown"), []).unwrap();
    readonly(&fixture.input.join("unknown"));
    fixture.seal_input();
    assert!(
        diagnostic(&fixture.invoke("no-output", "empty"))["diagnostic"]
            .as_str()
            .unwrap()
            .contains("UNKNOWN_INPUT_INVENTORY")
    );
}
#[test]
fn origin_requires_exact_fields_and_strict_counts() {
    let mut fixture = Fixture::new();
    let mut value: Value =
        serde_json::from_slice(&fs::read(fixture.revision.join("origin.json")).unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("observer_legacy_bits");
    fixture.rewrite_origin(&serde_json::to_vec(&value).unwrap());
    assert!(
        diagnostic(&fixture.invoke("missing-origin-field", "empty"))["diagnostic"]
            .as_str()
            .unwrap()
            .contains("EXACT_ORIGIN_FIELDS")
    );
    value["observer_legacy_bits"] = Value::Null;
    value["counts"]["queries"] = json!(true);
    fixture.rewrite_origin(&serde_json::to_vec(&value).unwrap());
    assert!(!fixture.invoke("bool-count", "empty").status.success());
    value["counts"]["queries"] = json!(1.0);
    fixture.rewrite_origin(&serde_json::to_vec(&value).unwrap());
    assert!(!fixture.invoke("float-count", "empty").status.success());
}
#[test]
fn late_footer_failure_does_not_publish_partial_activation() {
    let mut fixture = Fixture::new();
    let path = fixture.revision.join("queries.rbin");
    let mut tape = fs::read(&path).unwrap();
    let footer = tape.len() - 4096;
    tape[footer] ^= 1;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, tape).unwrap();
    readonly(&path);
    let changed = file_id(&path);
    fixture.request["query_tape"] = changed.clone();
    let mut origin: Value =
        serde_json::from_slice(&fs::read(fixture.revision.join("origin.json")).unwrap()).unwrap();
    origin["query_tape"] = changed;
    fixture.rewrite_origin(&serde_json::to_vec(&origin).unwrap());
    let d = diagnostic(&fixture.invoke("footer-failed", "empty"));
    assert!(d["diagnostic"].as_str().unwrap().contains("tape footer"));
    assert!(d["first_error_global_ordinal"].is_null());
    assert!(!fixture.root.join("footer-failed/HEAD").exists());
    assert!(!fixture.root.join("footer-failed/revisions").exists());
}

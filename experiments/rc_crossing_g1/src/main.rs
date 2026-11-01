// SPDX-License-Identifier: Apache-2.0
//! One-shot, short-lived CPU component worker. No CUDA code is loaded here.
use memmap2::{Mmap, MmapOptions};
use rusteda_rc_crossing_g1::{
    Limits, MAX_OUTPUT_BYTES, MAX_VIEW_BYTES, MAX_VIRTUAL, cpu_ref, format,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

type Result<T> = std::result::Result<T, String>;
const JOB_CAP: u64 = 64 << 10;
const RECEIPT_BYTES: usize = 2048;
const HEADER_ENCODE_WORK: u64 = 8; // Zero128B, magic/version and four u64 fields.
const STATUS: &[u8] =
    b"{\"status\":\"ok_component_only\",\"revision\":\"rev1\",\"backend\":\"cpu_ref\"}\n";
const PROTOCOL_PARTS: [&[u8]; 2] = [include_bytes!("lib.rs"), include_bytes!("format.rs")];

fn blocks(bytes: u64) -> u64 {
    bytes.div_ceil(64)
}
fn fingerprint_work(bytes: u64) -> Result<u64> {
    blocks(bytes)
        .checked_mul(2)
        .ok_or("IO_WORK_OVERFLOW".into())
}
fn activation_work(words: u64) -> Result<u64> {
    // One encoding/write/SHA operation per u64; header write+SHA are two passes.
    words
        .checked_mul(3)
        .and_then(|n| n.checked_add(HEADER_ENCODE_WORK + 2 * blocks(128)))
        .ok_or("IO_WORK_OVERFLOW".into())
}
fn protocol_work() -> u64 {
    PROTOCOL_PARTS
        .iter()
        .map(|part| blocks(8) + blocks(part.len() as u64))
        .sum()
}
fn tail_work() -> u64 {
    // Fixed-size JSON+padding encoding, receipt write, HEAD.tmp and stdout write.
    2 * blocks(RECEIPT_BYTES as u64) + blocks(5) + blocks(STATUS.len() as u64)
}
#[derive(Debug)]
struct IoLedger {
    completed: u64,
    bootstrap: u64,
    reservation: u64,
}
impl IoLedger {
    fn bootstrap() -> Self {
        Self {
            completed: 0,
            bootstrap: 0,
            reservation: 4 * blocks(JOB_CAP),
        }
    }
    fn record(&mut self, work: u64) -> Result<()> {
        self.completed = self
            .completed
            .checked_add(work)
            .filter(|&n| n <= self.reservation)
            .ok_or("IO_LEDGER_RESERVATION_EXCEEDED")?;
        Ok(())
    }
    fn admit(&mut self, cap: u64, job: u64, view: u64, elf: u64) -> Result<u64> {
        self.bootstrap = self.completed;
        let remaining = fingerprint_work(job)?
            .checked_add(
                fingerprint_work(view)?
                    .checked_mul(2)
                    .ok_or("IO_WORK_OVERFLOW")?,
            )
            .and_then(|n| n.checked_add(fingerprint_work(elf).ok()?.checked_mul(2)?))
            .and_then(|n| n.checked_add(protocol_work()))
            // Four possible bounded64B HEAD reads; successful fresh output has none.
            .and_then(|n| n.checked_add(4 * blocks(64)))
            .and_then(|n| n.checked_add(activation_work(MAX_VIRTUAL.div_ceil(64) as u64).ok()?))
            .and_then(|n| n.checked_add(tail_work()))
            .ok_or("IO_WORK_OVERFLOW")?;
        self.reservation = self
            .completed
            .checked_add(remaining)
            .filter(|&n| n < cap)
            .ok_or("IO_WORK_BUDGET_BEFORE_LARGE_IO")?;
        Ok(cap - self.reservation)
    }
}
fn need(value: bool, message: &str) -> Result<()> {
    if value { Ok(()) } else { Err(message.into()) }
}
fn fail_io(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(256).collect()
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FileId {
    path: PathBuf,
    sha256: String,
    bytes: u64,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Backend {
    CpuRef,
    Cuda,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    backend: Backend,
    view_workspace: PathBuf,
    view_revision: String,
    view: FileId,
    worker: FileId,
    protocol_sha256: String,
    limits: Limits,
}
#[derive(Debug)]
struct Args {
    workspace: PathBuf,
    base_rev: String,
    inputs: PathBuf,
    inputs_sha: String,
}
fn args() -> Result<Args> {
    let mut input = std::env::args().skip(1);
    let mut pairs = Vec::new();
    for _ in 0..5 {
        match input.next() {
            None => break,
            Some(key) => {
                let value = input.next().ok_or("ARGUMENT_VALUE")?;
                need(key.len() <= 32 && value.len() <= 4096, "ARGUMENT_LENGTH")?;
                pairs.push((key, value));
            }
        }
    }
    need(input.next().is_none() && pairs.len() == 4, "ARGUMENT_COUNT")?;
    let take = |name: &str| -> Result<String> {
        let rows: Vec<_> = pairs.iter().filter(|(k, _)| k == name).collect();
        need(rows.len() == 1, "ARGUMENT_KEYS")?;
        Ok(rows[0].1.clone())
    };
    for (key, _) in &pairs {
        need(
            ["--workspace", "--base-rev", "--inputs", "--inputs-sha"].contains(&key.as_str()),
            "UNKNOWN_ARGUMENT",
        )?;
    }
    Ok(Args {
        workspace: PathBuf::from(take("--workspace")?),
        base_rev: take("--base-rev")?,
        inputs: PathBuf::from(take("--inputs")?),
        inputs_sha: take("--inputs-sha")?,
    })
}
fn handle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn plain(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(fail_io)?.join(path)
    };
    let mut current = PathBuf::new();
    for component in path.components() {
        need(
            !matches!(component, Component::ParentDir | Component::CurDir),
            "PARENT_PATH",
        )?;
        current.push(component.as_os_str());
        let info = fs::symlink_metadata(&current).map_err(fail_io)?;
        need(!info.file_type().is_symlink(), "SYMLINK")?;
    }
    Ok(path)
}
fn fresh_path(path: &Path) -> Result<PathBuf> {
    let parent = plain(path.parent().ok_or("WORKSPACE_PARENT")?)?;
    let name = path.file_name().ok_or("WORKSPACE_NAME")?;
    need(!name.is_empty(), "WORKSPACE_NAME")?;
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(info) => {
            need(!info.file_type().is_symlink(), "WRITER_SYMLINK")?;
            plain(&path)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(fail_io(e)),
    }
}
fn fingerprint(file: &mut File, path: &Path, bytes: u64, io: &mut IoLedger) -> Result<FileId> {
    let before = file.metadata().map_err(fail_io)?;
    need(
        before.is_file() && before.len() == bytes,
        "FILE_ADMITTED_BYTES_CHANGED",
    )?;
    file.seek(SeekFrom::Start(0)).map_err(fail_io)?;
    let mut h = Sha256::new();
    let mut left = before.len();
    let mut buffer = [0u8; 65536];
    while left > 0 {
        let count = usize::try_from(left.min(buffer.len() as u64)).map_err(fail_io)?;
        let n = file.read(&mut buffer[..count]).map_err(fail_io)?;
        need(n > 0, "FILE_SHRANK")?;
        left -= n as u64;
        h.update(&buffer[..n]);
    }
    need(
        file.read(&mut buffer[..1]).map_err(fail_io)? == 0,
        "FILE_GREW",
    )?;
    let after = file.metadata().map_err(fail_io)?;
    let named = fs::symlink_metadata(path).map_err(fail_io)?;
    need(
        (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) == (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
        ) && (named.dev(), named.ino()) == (before.dev(), before.ino())
            && !named.file_type().is_symlink(),
        "FILE_CHANGED",
    )?;
    io.record(fingerprint_work(bytes)?)?;
    Ok(FileId {
        path: path.to_path_buf(),
        sha256: format!("{:x}", h.finalize()),
        bytes: before.len(),
    })
}
fn regular(path: &Path, cap: u64) -> Result<PathBuf> {
    let path = plain(path)?;
    let info = fs::symlink_metadata(&path).map_err(fail_io)?;
    need(
        info.is_file() && info.nlink() == 1 && info.len() <= cap,
        "REGULAR_UNIQUE_FILE_REQUIRED",
    )?;
    Ok(path)
}
fn open_immutable(path: &Path, cap: u64) -> Result<(File, PathBuf, u64)> {
    let path = regular(path, cap)?;
    let file = File::open(&path).map_err(fail_io)?;
    let info = file.metadata().map_err(fail_io)?;
    need(
        info.is_file()
            && info.len() <= cap
            && info.nlink() == 1
            && info.permissions().mode() & 0o222 == 0,
        "INPUT_NOT_PRIVATE_READONLY",
    )?;
    Ok((file, path, info.len()))
}
fn read_small(file: &mut File, cap: usize, io: &mut IoLedger) -> Result<Vec<u8>> {
    need(
        file.metadata().map_err(fail_io)?.len() <= cap as u64,
        "METADATA_CAP",
    )?;
    file.seek(SeekFrom::Start(0)).map_err(fail_io)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(cap + 1).map_err(fail_io)?;
    file.take((cap + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(fail_io)?;
    need(bytes.len() <= cap, "METADATA_GREW")?;
    io.record(blocks(bytes.len() as u64))?;
    Ok(bytes)
}
fn locked(file: &File, shared: bool) -> Result<()> {
    // Non-blocking: no daemon, interactive prompt or unlimited lock wait.
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err("WORKSPACE_BUSY".into()),
        Err(TryLockError::Error(e)) => Err(fail_io(e)),
    }
}
fn head(root: &Path, io: &mut IoLedger) -> Result<String> {
    let path = root.join("HEAD");
    if !path.exists() {
        return Ok("empty".into());
    }
    let path = regular(&path, 64)?;
    let mut f = File::open(path).map_err(fail_io)?;
    let raw = read_small(&mut f, 64, io)?;
    let text = std::str::from_utf8(&raw).map_err(fail_io)?;
    let value = text.strip_suffix('\n').ok_or("HEAD_FRAMING")?;
    need(handle(value), "HEAD_HANDLE")?;
    Ok(value.into())
}
#[expect(
    unsafe_code,
    reason = "read-only mapping of a unique immutable private input under its held shared workspace lock"
)]
fn mapped(file: &File) -> Result<Mmap> {
    // SAFETY: the descriptor was checked regular, unique, <=64MiB and readonly.
    // Its private input workspace shared lock stays held for the entire map.
    // The producer contract forbids mutation/truncation until that lock releases.
    unsafe { MmapOptions::new().map(file).map_err(fail_io) }
}
fn protocol_sha(io: &mut IoLedger) -> Result<String> {
    let mut h = Sha256::new();
    for part in PROTOCOL_PARTS {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    io.record(protocol_work())?;
    Ok(format!("{:x}", h.finalize()))
}
fn output_file(
    path: &Path,
    header: &[u8],
    words: &[u64],
    io: &mut IoLedger,
) -> Result<(String, u64)> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(fail_io)?;
    let mut h = Sha256::new();
    file.write_all(header).map_err(fail_io)?;
    h.update(header);
    io.record(2 * blocks(header.len() as u64))?;
    let mut bytes = header.len() as u64;
    for word in words {
        let raw = word.to_le_bytes();
        file.write_all(&raw).map_err(fail_io)?;
        h.update(raw);
        io.record(3)?;
        bytes += 8;
        need(bytes <= MAX_OUTPUT_BYTES, "OUTPUT_CAP")?;
    }
    file.sync_all().map_err(fail_io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).map_err(fail_io)?;
    Ok((format!("{:x}", h.finalize()), bytes))
}
fn run() -> Result<()> {
    let started = Instant::now();
    let args = args()?;
    need(
        handle(&args.base_rev) && sha(&args.inputs_sha),
        "CLI_HANDLE_OR_SHA",
    )?;
    // The only pre-budget byte work is this explicitly bounded64KiB bootstrap:
    // job read+SHA, parse-buffer read and parsed-buffer SHA (four block passes).
    // JSON parsing, arguments, metadata syscalls, EOF probes and error output
    // have separate fixed byte/count limits; this is not an instruction counter.
    let mut io = IoLedger::bootstrap();
    let (mut input, job_path, job_bytes_admitted) = open_immutable(&args.inputs, JOB_CAP)?;
    let job_id = fingerprint(&mut input, &job_path, job_bytes_admitted, &mut io)?;
    need(job_id.sha256 == args.inputs_sha, "INPUT_JOB_SHA_CHANGED")?;
    let job_bytes = read_small(&mut input, JOB_CAP as usize, &mut io)?;
    let parsed_sha = format!("{:x}", Sha256::digest(&job_bytes));
    io.record(blocks(job_bytes.len() as u64))?;
    need(
        job_bytes.len() as u64 == job_id.bytes && parsed_sha == job_id.sha256,
        "PARSED_JOB_BYTES_CHANGED",
    )?;
    let request: Request = serde_json::from_slice(&job_bytes).map_err(fail_io)?;
    need(
        request.schema == "rusteda.g1-csr-worker-inputs.v1",
        "JOB_SCHEMA",
    )?;
    if matches!(request.backend, Backend::Cuda) {
        return Err("GPU_UNVERIFIED:CPU_GATE_AND_KERNEL_NOT_IMPLEMENTED".into());
    }
    request.limits.validate().map_err(fail_io)?;
    let input_root = plain(&request.view_workspace)?;
    need(
        input_root.is_dir()
            && input_root.metadata().map_err(fail_io)?.permissions().mode() & 0o077 == 0,
        "PRIVATE_INPUT_WORKSPACE",
    )?;
    let input_lock =
        File::open(regular(&input_root.join("workspace.lock"), 4096)?).map_err(fail_io)?;
    need(
        input_lock.metadata().map_err(fail_io)?.is_file()
            && input_lock.metadata().map_err(fail_io)?.nlink() == 1,
        "INPUT_LOCK_ALIAS",
    )?;
    locked(&input_lock, true)?;
    let (mut view_file, view_path, view_bytes_admitted) =
        open_immutable(&request.view.path, MAX_VIEW_BYTES)?;
    need(
        view_path == request.view.path
            && view_bytes_admitted == request.view.bytes
            && sha(&request.view.sha256)
            && view_path.starts_with(&input_root)
            && job_id.path.starts_with(&input_root),
        "INPUT_VIEW_METADATA_OWNER",
    )?;
    let executable = plain(&std::env::current_exe().map_err(fail_io)?)?;
    let mut exe = File::open(&executable).map_err(fail_io)?;
    let elf_info = exe.metadata().map_err(fail_io)?;
    let elf_bytes_admitted = elf_info.len();
    need(
        elf_info.is_file()
            && executable == request.worker.path
            && elf_bytes_admitted == request.worker.bytes
            && elf_bytes_admitted <= 128 << 20
            && sha(&request.worker.sha256),
        "WORKER_ELF_METADATA_CHANGED",
    )?;
    // No view/ELF bytes or embedded source hashes have been read yet. Reserve
    // all remaining successful-path I/O from checked metadata before those passes.
    let mut effective = request.limits;
    effective.max_work = io.admit(
        request.limits.max_work,
        job_id.bytes,
        view_bytes_admitted,
        elf_bytes_admitted,
    )?;
    need(
        handle(&request.view_revision) && head(&input_root, &mut io)? == request.view_revision,
        "INPUT_REVISION_CONFLICT",
    )?;
    let view_id = fingerprint(&mut view_file, &view_path, view_bytes_admitted, &mut io)?;
    need(view_id == request.view, "INPUT_VIEW_IDENTITY_CHANGED")?;
    let actual_worker = fingerprint(&mut exe, &executable, elf_bytes_admitted, &mut io)?;
    need(actual_worker == request.worker, "WORKER_ELF_CHANGED")?;
    let protocol = protocol_sha(&mut io)?;
    need(
        sha(&request.protocol_sha256) && protocol == request.protocol_sha256,
        "PROTOCOL_SOURCE_CHANGED",
    )?;
    let root = fresh_path(&args.workspace)?;
    need(
        !root.starts_with(&input_root) && !input_root.starts_with(&root),
        "WORKSPACE_ALIAS",
    )?;
    need(
        root.parent()
            .ok_or("OUTPUT_PARENT")?
            .metadata()
            .map_err(fail_io)?
            .permissions()
            .mode()
            & 0o077
            == 0,
        "PRIVATE_OUTPUT_PARENT",
    )?;
    if !root.exists() {
        fs::create_dir(&root).map_err(fail_io)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(fail_io)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("workspace.lock"))
            .map_err(fail_io)?;
    }
    need(
        root.metadata().map_err(fail_io)?.permissions().mode() & 0o077 == 0,
        "PRIVATE_OUTPUT_WORKSPACE",
    )?;
    let output_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(regular(&root.join("workspace.lock"), 4096)?)
        .map_err(fail_io)?;
    need(
        output_lock.metadata().map_err(fail_io)?.is_file()
            && output_lock.metadata().map_err(fail_io)?.nlink() == 1,
        "OUTPUT_LOCK_ALIAS",
    )?;
    locked(&output_lock, false)?;
    let actual_head = head(&root, &mut io)?;
    need(actual_head == args.base_rev, "REVISION_CONFLICT")?;
    need(actual_head == "empty", "ONE_SHOT_REQUIRES_FRESH_WORKSPACE")?;
    let mut inventory = 0usize;
    for entry in fs::read_dir(&root).map_err(fail_io)? {
        inventory += 1;
        need(
            inventory <= 1 && entry.map_err(fail_io)?.file_name() == "workspace.lock",
            "UNCLEAN_EMPTY_WORKSPACE",
        )?;
    }
    let mapping = mapped(&view_file)?;
    need(mapping.len() as u64 == view_id.bytes, "MAPPED_BYTES")?;
    let view = format::decode(&mapping, effective).map_err(fail_io)?;
    let activation = cpu_ref(view, effective).map_err(fail_io)?;
    need(
        fingerprint(&mut view_file, &view_id.path, view_id.bytes, &mut io)? == view_id
            && head(&input_root, &mut io)? == request.view_revision,
        "INPUT_CHANGED_DURING_USE",
    )?;
    need(
        fingerprint(&mut input, &job_id.path, job_id.bytes, &mut io)? == job_id,
        "JOB_CHANGED_DURING_USE",
    )?;
    need(
        fingerprint(&mut exe, &executable, actual_worker.bytes, &mut io)? == actual_worker,
        "WORKER_CHANGED_DURING_USE",
    )?;
    let io_on_success = io
        .completed
        .checked_add(activation_work(activation.marked.len() as u64)?)
        .and_then(|n| n.checked_add(tail_work()))
        .filter(|&n| n <= io.reservation)
        .ok_or("IO_SUCCESS_LEDGER_OVERFLOW")?;
    let work = activation
        .stats
        .total_work
        .checked_add(io_on_success)
        .filter(|&n| n <= request.limits.max_work)
        .ok_or("WORK_BUDGET")?;
    let mut header = [0u8; 128];
    header[..8].copy_from_slice(b"RG1ACT01");
    header[8..12].copy_from_slice(&1u32.to_le_bytes());
    for (at, n) in [
        (16, view.virtual_count as u64),
        (24, view.queries.len() as u64),
        (32, activation.marked.len() as u64),
        (40, work),
    ] {
        header[at..at + 8].copy_from_slice(&n.to_le_bytes());
    }
    io.record(HEADER_ENCODE_WORK)?;
    drop(mapping); // Direct mmap does not leave a retained malloc input pool.
    let revisions = root.join("revisions");
    fs::create_dir(&revisions).map_err(fail_io)?;
    let revision = revisions.join("rev1");
    fs::create_dir(&revision).map_err(fail_io)?;
    let output = revision.join("activation.rbin");
    let (hash, bytes) = output_file(&output, &header, &activation.marked, &mut io)?;
    let io_completed_before_receipt = io.completed;
    let report = serde_json::json!({"schema":"rusteda.g1-csr-worker-result.v1","status":"ok_component_only","backend":"cpu_ref",
        "input_sha256":view_id.sha256,"job_sha256":job_id.sha256,"worker_sha256":actual_worker.sha256,"protocol_sha256":protocol,
        "output_sha256":hash,"output_bytes":bytes,"io_work_on_success":io_on_success,"io_completed_before_receipt":io_completed_before_receipt,
        "io_reserved_work":io.reservation,"bootstrap_work":io.bootstrap,"work_authority":"requires_process_exit0_and_HEAD_rev1",
        "total_work_on_success":work,"stats":activation.stats,
        "elapsed_seconds":started.elapsed().as_secs_f64(),"legacy_road_binding":"unchecked","corner":"not_applicable","wire_model":"integer_activation_only","drc_status":"unchecked","lvs_status":"unchecked"});
    let mut raw = serde_json::to_vec(&report).map_err(fail_io)?;
    need(
        raw.len() <= RECEIPT_BYTES
            && bytes + RECEIPT_BYTES as u64 + 5 + 512 + 1024 <= MAX_OUTPUT_BYTES,
        "OUTPUT_AGGREGATE_CAP",
    )?;
    // Exactly2048 encoded-or-padding bytes removes receipt-size self-reference.
    raw.resize(RECEIPT_BYTES, b' ');
    io.record(blocks(RECEIPT_BYTES as u64))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(revision.join("report.json"))
        .map_err(fail_io)?;
    file.write_all(&raw).map_err(fail_io)?;
    io.record(blocks(RECEIPT_BYTES as u64))?;
    file.sync_all().map_err(fail_io)?;
    fs::set_permissions(
        revision.join("report.json"),
        fs::Permissions::from_mode(0o444),
    )
    .map_err(fail_io)?;
    need(head(&root, &mut io)? == args.base_rev, "REVISION_CONFLICT")?;
    let temporary = root.join("HEAD.tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(fail_io)?;
    f.write_all(b"rev1\n").map_err(fail_io)?;
    io.record(blocks(5))?;
    f.sync_all().map_err(fail_io)?;
    fs::rename(&temporary, root.join("HEAD")).map_err(fail_io)?;
    File::open(&root)
        .map_err(fail_io)?
        .sync_all()
        .map_err(fail_io)?;
    std::io::stdout().write_all(STATUS).map_err(fail_io)?;
    io.record(blocks(STATUS.len() as u64))?;
    need(io.completed == io_on_success, "IO_SUCCESS_LEDGER_MISMATCH")?;
    Ok(())
}
fn main() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!("{{\"status\":\"error\",\"diagnostic\":\"COMPONENT_PANIC\"}}")
    }));
    if let Err(error) = run() {
        let diagnostic: String = error.chars().take(96).collect();
        eprintln!(
            "{}",
            serde_json::json!({"status":"unverified_component","diagnostic":diagnostic,"legacy_road_binding":"unchecked"})
        );
        std::process::exit(2);
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Phase-A foreground CPUv3 adapter. No GPU entry/runtime or inherited8be qualification.
#[path = "../session_json.rs"]
mod session_json;
use memmap2::{Mmap, MmapOptions};
use rusteda_rc_crossing_g1::dto::{BudgetSnapshotDto, SessionLimitsDto, SessionStatsDto};
use rusteda_rc_crossing_g1::session::{Budget, Failure, PreparedStatic, Session, SessionLimits};
use rusteda_rc_crossing_g1::session_format::{
    self as frame, ActivationHeader, HEADER_BYTES, TapeHeader, TapeLayout,
};
use rusteda_rc_crossing_g1::{Error, MAX_VIRTUAL, Query, Table};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const SMALL: u64 = 65536;
const RECEIPT: usize = 4096;
const STATUS: &[u8] = b"{\"status\":\"ok_component_only\",\"revision\":\"rev1\",\"backend\":\"cpu_ref_session_v3\"}\n";
const HEADER_WORK: u64 = 16;
const ERROR_TAIL_WORK: u64 = 16; // fixed512-byte logical encode/pad plus attempted write
const PROTOCOL_DOMAIN: &[u8] = b"rusteda.g1.compute-session.protocol.v3\0";
const PARTS: [(&str, &str, &[u8]); 25] = [
    (
        "geometry.mod",
        "crates/core/rusteda-compute/src/geometry/mod.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/mod.rs"),
    ),
    (
        "task.mod",
        "crates/core/rusteda-compute/src/geometry/crossings/mod.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/crossings/mod.rs"),
    ),
    (
        "task.integer",
        "crates/core/rusteda-compute/src/geometry/crossings/integer.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/crossings/integer.rs"),
    ),
    (
        "task.cpu",
        "crates/core/rusteda-compute/src/geometry/crossings/cpu.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/crossings/cpu.rs"),
    ),
    (
        "task.session",
        "crates/core/rusteda-compute/src/geometry/crossings/session.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/crossings/session.rs"),
    ),
    (
        "task.device_abi",
        "crates/core/rusteda-compute/src/geometry/crossings/device_abi.rs",
        include_bytes!(
            "../../../../crates/core/rusteda-compute/src/geometry/crossings/device_abi.rs"
        ),
    ),
    (
        "task.cuda",
        "crates/core/rusteda-compute/src/geometry/crossings/cuda.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/geometry/crossings/cuda.rs"),
    ),
    (
        "task.kernel",
        "crates/core/rusteda-compute/src/geometry/kernels/g1_crossings.rs",
        include_bytes!(
            "../../../../crates/core/rusteda-compute/src/geometry/kernels/g1_crossings.rs"
        ),
    ),
    (
        "runtime.cuda",
        "crates/core/rusteda-compute/src/cuda.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/cuda.rs"),
    ),
    (
        "compute.lib",
        "crates/core/rusteda-compute/src/lib.rs",
        include_bytes!("../../../../crates/core/rusteda-compute/src/lib.rs"),
    ),
    (
        "compute.cargo",
        "crates/core/rusteda-compute/Cargo.toml",
        include_bytes!("../../../../crates/core/rusteda-compute/Cargo.toml"),
    ),
    (
        "workspace.cargo",
        "Cargo.toml",
        include_bytes!("../../../../Cargo.toml"),
    ),
    (
        "workspace.lock",
        "Cargo.lock",
        include_bytes!("../../../../Cargo.lock"),
    ),
    (
        "adapter.lib",
        "experiments/rc_crossing_g1/src/lib.rs",
        include_bytes!("../lib.rs"),
    ),
    (
        "adapter.session",
        "experiments/rc_crossing_g1/src/session.rs",
        include_bytes!("../session.rs"),
    ),
    (
        "adapter.dto",
        "experiments/rc_crossing_g1/src/dto.rs",
        include_bytes!("../dto.rs"),
    ),
    (
        "adapter.format",
        "experiments/rc_crossing_g1/src/format.rs",
        include_bytes!("../format.rs"),
    ),
    (
        "adapter.session_format",
        "experiments/rc_crossing_g1/src/session_format.rs",
        include_bytes!("../session_format.rs"),
    ),
    (
        "adapter.session_json",
        "experiments/rc_crossing_g1/src/session_json.rs",
        include_bytes!("../session_json.rs"),
    ),
    (
        "adapter.v3_main",
        "experiments/rc_crossing_g1/src/bin/session_v3.rs",
        include_bytes!("session_v3.rs"),
    ),
    (
        "adapter.cargo",
        "experiments/rc_crossing_g1/Cargo.toml",
        include_bytes!("../../Cargo.toml"),
    ),
    (
        "adapter.lock",
        "experiments/rc_crossing_g1/Cargo.lock",
        include_bytes!("../../Cargo.lock"),
    ),
    (
        "gpu.main",
        "experiments/rc_crossing_g1/gpu/src/main.rs",
        include_bytes!("../../gpu/src/main.rs"),
    ),
    (
        "gpu.cargo",
        "experiments/rc_crossing_g1/gpu/Cargo.toml",
        include_bytes!("../../gpu/Cargo.toml"),
    ),
    (
        "gpu.lock",
        "experiments/rc_crossing_g1/gpu/Cargo.lock",
        include_bytes!("../../gpu/Cargo.lock"),
    ),
];
type Result<T> = std::result::Result<T, WorkerError>;
#[derive(Debug)]
struct WorkerError {
    diagnostic: String,
    stage: &'static str,
    ordinal: Option<u64>,
}
impl WorkerError {
    fn new(message: impl std::fmt::Display) -> Self {
        Self {
            diagnostic: message.to_string().chars().take(160).collect(),
            stage: "authority_or_io",
            ordinal: None,
        }
    }
}
impl From<Error> for WorkerError {
    fn from(e: Error) -> Self {
        Self::new(e)
    }
}
impl From<Failure> for WorkerError {
    fn from(e: Failure) -> Self {
        Self {
            diagnostic: e.error.to_string(),
            stage: match e.stage {
                rusteda_rc_crossing_g1::session::Stage::Static => "static",
                rusteda_rc_crossing_g1::session::Stage::Window => "window",
                rusteda_rc_crossing_g1::session::Stage::Query => "query",
                rusteda_rc_crossing_g1::session::Stage::Finish => "finish",
            },
            ordinal: e.query_ordinal,
        }
    }
}
fn need(value: bool, code: &'static str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(WorkerError::new(code))
    }
}
fn io_error(e: impl std::fmt::Display) -> WorkerError {
    WorkerError::new(e)
}
fn b(n: u64) -> u64 {
    n.div_ceil(64)
}
fn checked_sum(values: &[u64]) -> Result<u64> {
    values.iter().try_fold(0u64, |n, x| {
        n.checked_add(*x)
            .ok_or_else(|| WorkerError::new("IO_WORK_OVERFLOW"))
    })
}
fn fingerprint_work(n: u64) -> u64 {
    2 * b(n)
}
fn activation_work(n: u64) -> u64 {
    HEADER_WORK + 2 * b(256) + 3 * n
}
fn publication_tail(words: u64) -> u64 {
    activation_work(words) + 2 * b(RECEIPT as u64) + b(5) + b(STATUS.len() as u64)
}
fn protocol_work() -> Result<u64> {
    need(
        PARTS.len() == 25
            && PARTS.iter().all(|(name, _, _)| name.len() <= 128)
            && PARTS.iter().map(|(_, _, v)| v.len()).sum::<usize>() <= 2 << 20,
        "PROTOCOL_SOURCE_CAP",
    )?;
    PARTS
        .iter()
        .try_fold(b(PROTOCOL_DOMAIN.len() as u64), |work, (name, _, raw)| {
            checked_sum(&[work, 2 * b(8), b(name.len() as u64), b(raw.len() as u64)])
        })
}
fn protocol(budget: &mut Budget) -> Result<[u8; 32]> {
    budget.io(protocol_work()?)?;
    let mut hash = Sha256::new();
    hash.update(PROTOCOL_DOMAIN);
    for (name, _, raw) in PARTS {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((raw.len() as u64).to_le_bytes());
        hash.update(raw);
    }
    Ok(hash.finalize().into())
}
fn digest(text: &str) -> Result<[u8; 32]> {
    need(
        text.len() == 64
            && text
                .bytes()
                .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
        "SHA256_FRAMING",
    )?;
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let digit = |v: u8| if v <= b'9' { v - b'0' } else { v - b'a' + 10 };
        *slot = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(out)
}
fn hex(raw: &[u8]) -> String {
    raw.iter().map(|n| format!("{n:02x}")).collect()
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileId {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Origin {
    ManualFixture {
        record: FileId,
    },
    ActualRoadCapture {
        record: FileId,
        observer_legacy_bits: FileId,
    },
}
impl Origin {
    fn record(&self) -> &FileId {
        match self {
            Self::ManualFixture { record } | Self::ActualRoadCapture { record, .. } => record,
        }
    }
    fn bits(&self) -> Option<&FileId> {
        match self {
            Self::ManualFixture { .. } => None,
            Self::ActualRoadCapture {
                observer_legacy_bits,
                ..
            } => Some(observer_legacy_bits),
        }
    }
    fn record_name(&self) -> &'static str {
        if self.bits().is_some() {
            "capture.json"
        } else {
            "origin.json"
        }
    }
    fn schema(&self) -> &'static str {
        if self.bits().is_some() {
            "rusteda.g1-road-stream-capture.v2"
        } else {
            "rusteda.g1-static-query-origin.v2"
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Backend {
    CpuRef,
    CudaRequired,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    backend: Backend,
    input_workspace: PathBuf,
    input_revision: String,
    static_view: FileId,
    query_tape: FileId,
    worker: FileId,
    protocol_sha256: String,
    origin: Origin,
    limits: SessionLimitsDto,
    source_registry: FileId,
    task_abi: u32,
    device_abi: u32,
    cuda_image: Option<FileId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    instances: u64,
    templates: u64,
    rectangles: u64,
    virtual_shapes: u64,
    bins: u64,
    entries: u64,
    queries: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginRecord {
    schema: String,
    static_view: FileId,
    query_tape: FileId,
    observer_legacy_bits: Option<FileId>,
    counts: Counts,
    provenance: serde_json::Value,
}
#[derive(Debug)]
struct Args {
    workspace: PathBuf,
    base: String,
    inputs: PathBuf,
    sha: String,
}
fn args() -> Result<Args> {
    let values = std::env::args_os()
        .skip(1)
        .take(10)
        .map(|v| {
            v.into_string()
                .map_err(|_| WorkerError::new("ARGUMENT_UTF8"))
        })
        .collect::<Result<Vec<_>>>()?;
    need(
        values.len() == 8 && values.iter().all(|s| s.len() <= 4096),
        "EXACT_FOUR_ARGUMENT_PAIRS",
    )?;
    let mut names = BTreeSet::new();
    for pair in values.chunks_exact(2) {
        need(
            ["--workspace", "--base-rev", "--inputs", "--inputs-sha"].contains(&pair[0].as_str())
                && names.insert(pair[0].as_str()),
            "ARGUMENT_KEYS",
        )?;
    }
    let value = |key| {
        values
            .chunks_exact(2)
            .find(|p| p[0] == key)
            .map(|p| p[1].clone())
            .ok_or_else(|| WorkerError::new("ARGUMENT_MISSING"))
    };
    Ok(Args {
        workspace: PathBuf::from(value("--workspace")?),
        base: value("--base-rev")?,
        inputs: PathBuf::from(value("--inputs")?),
        sha: value("--inputs-sha")?,
    })
}
fn plain(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io_error)?.join(path)
    };
    let mut prefix = PathBuf::new();
    for item in path.components() {
        need(
            !matches!(item, Component::ParentDir | Component::CurDir),
            "PATH_COMPONENT",
        )?;
        prefix.push(item);
        need(
            !fs::symlink_metadata(&prefix)
                .map_err(io_error)?
                .file_type()
                .is_symlink(),
            "SYMLINK_ANCESTOR",
        )?;
    }
    Ok(path)
}
#[derive(Debug)]
struct InputFile {
    file: File,
    path: PathBuf,
    bytes: u64,
}
impl InputFile {
    fn open(path: &Path, cap: u64, uid: u32) -> Result<Self> {
        let path = plain(path)?;
        let named = fs::symlink_metadata(&path).map_err(io_error)?;
        need(
            named.is_file()
                && named.nlink() == 1
                && named.len() <= cap
                && named.uid() == uid
                && named.permissions().mode() & 0o222 == 0,
            "UNIQUE_OWNED_IMMUTABLE_FILE",
        )?;
        let file = File::open(&path).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        need(
            (named.dev(), named.ino(), named.len()) == (opened.dev(), opened.ino(), opened.len())
                && opened.is_file()
                && opened.len() <= cap
                && opened.nlink() == 1
                && opened.uid() == uid
                && opened.permissions().mode() & 0o222 == 0,
            "INPUT_OPEN_ALIAS",
        )?;
        Ok(Self {
            file,
            path,
            bytes: opened.len(),
        })
    }
    fn metadata_unchanged(&self, before: &fs::Metadata) -> Result<()> {
        let after = self.file.metadata().map_err(io_error)?;
        let named = fs::symlink_metadata(&self.path).map_err(io_error)?;
        need(
            (
                before.dev(),
                before.ino(),
                before.len(),
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec(),
            ) == (
                after.dev(),
                after.ino(),
                after.len(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec(),
            ) && (named.dev(), named.ino()) == (before.dev(), before.ino())
                && named.is_file()
                && named.nlink() == 1
                && named.permissions().mode() & 0o222 == 0,
            "INPUT_CHANGED",
        )
    }
    fn fingerprint(&mut self) -> Result<FileId> {
        let before = self.file.metadata().map_err(io_error)?;
        need(before.len() == self.bytes, "INPUT_ADMITTED_SIZE_CHANGED")?;
        self.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut hash = Sha256::new();
        let mut left = self.bytes;
        let mut buffer = [0u8; 65536];
        while left != 0 {
            let count = usize::try_from(left.min(buffer.len() as u64)).map_err(io_error)?;
            self.file
                .read_exact(&mut buffer[..count])
                .map_err(io_error)?;
            hash.update(&buffer[..count]);
            left -= count as u64;
        }
        need(
            self.file.read(&mut buffer[..1]).map_err(io_error)? == 0,
            "INPUT_GREW",
        )?;
        self.metadata_unchanged(&before)?;
        Ok(FileId {
            path: self.path.clone(),
            bytes: self.bytes,
            sha256: hex(&hash.finalize()),
        })
    }
    fn verify(&mut self, expected: &FileId, budget: &mut Budget) -> Result<()> {
        budget.io(fingerprint_work(self.bytes))?;
        need(&self.fingerprint()? == expected, "FILEID_MISMATCH")
    }
    fn small(&mut self) -> Result<Vec<u8>> {
        need(self.bytes <= SMALL, "METADATA_SIZE")?;
        let before = self.file.metadata().map_err(io_error)?;
        need(before.len() == self.bytes, "METADATA_SIZE_CHANGED")?;
        self.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(self.bytes as usize + 1)
            .map_err(io_error)?;
        Read::by_ref(&mut self.file)
            .take(self.bytes + 1)
            .read_to_end(&mut raw)
            .map_err(io_error)?;
        need(raw.len() as u64 == self.bytes, "METADATA_GREW_OR_SHRANK")?;
        self.metadata_unchanged(&before)?;
        Ok(raw)
    }
}
fn locked(file: &File, shared: bool) -> Result<()> {
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(WorkerError::new("WORKSPACE_BUSY")),
        Err(TryLockError::Error(e)) => Err(io_error(e)),
    }
}
fn inventory(root: &Path, origin: &Origin, uid: u32) -> Result<()> {
    let expected_root = BTreeSet::from(["HEAD", "revisions", "workspace.lock"]);
    let expected_rev = BTreeSet::from(["rev1"]);
    let mut expected_data = BTreeSet::from(["static.rbin", "queries.rbin", origin.record_name()]);
    if origin.bits().is_some() {
        expected_data.insert("legacy-bits.rbin");
    }
    for (path, names) in [
        (root.to_path_buf(), expected_root),
        (root.join("revisions"), expected_rev),
        (root.join("revisions/rev1"), expected_data),
    ] {
        let info = fs::symlink_metadata(&path).map_err(io_error)?;
        need(
            info.is_dir()
                && !info.file_type().is_symlink()
                && info.uid() == uid
                && info.permissions().mode() & 0o277 == 0,
            "PRIVATE_INPUT_DIRECTORY",
        )?;
        let mut seen = BTreeSet::new();
        for (n, entry) in fs::read_dir(&path).map_err(io_error)?.enumerate() {
            need(n < 6, "INPUT_INVENTORY_BOUND")?;
            let entry = entry.map_err(io_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| WorkerError::new("INPUT_FILENAME"))?;
            need(
                names.contains(name.as_str()) && seen.insert(name),
                "UNKNOWN_INPUT_INVENTORY",
            )?;
            let info = fs::symlink_metadata(entry.path()).map_err(io_error)?;
            need(
                !info.file_type().is_symlink() && info.uid() == uid,
                "INPUT_SYMLINK_OR_OWNER",
            )?;
        }
        need(seen.len() == names.len(), "INCOMPLETE_INPUT_INVENTORY")?;
    }
    Ok(())
}
fn head(root: &Path, uid: u32, budget: &mut Budget) -> Result<FileId> {
    let mut file = InputFile::open(&root.join("HEAD"), 64, uid)?;
    budget.io(fingerprint_work(file.bytes))?;
    let value = file.fingerprint()?;
    need(
        file.bytes == 5 && value.sha256 == hex(&Sha256::digest(b"rev1\n")),
        "INPUT_REVISION_CONFLICT",
    )?;
    budget.fixed_event()?;
    Ok(value)
}
fn open_expected(value: &FileId, cap: u64, uid: u32) -> Result<InputFile> {
    digest(&value.sha256)?;
    let file = InputFile::open(&value.path, cap, uid)?;
    need(
        file.path == value.path && file.bytes == value.bytes,
        "INPUT_FILEID_METADATA",
    )?;
    Ok(file)
}
#[expect(
    unsafe_code,
    reason = "unique sealed readonly static file under held input shared lock"
)]
fn mapped(file: &InputFile) -> Result<Mmap> {
    // SAFETY: bounded regular readonly unique private file; producer contract and
    // its held shared lock forbid truncation/mutation until this map is dropped.
    unsafe { MmapOptions::new().map(&file.file).map_err(io_error) }
}
#[derive(Debug, Default)]
struct BytePass {
    bytes: u64,
}
impl BytePass {
    fn record(&mut self, n: u64, cap: u64, session: &mut Session<'_, '_>) -> Result<()> {
        let next = self
            .bytes
            .checked_add(n)
            .filter(|n| *n <= cap)
            .ok_or_else(|| WorkerError::new("STREAM_BYTE_CAP"))?;
        session.io(b(next) - b(self.bytes))?;
        self.bytes = next;
        Ok(())
    }
}
#[derive(Debug)]
struct IoInputs {
    job: u64,
    origin: u64,
    elf: u64,
    static_bytes: u64,
    tape: TapeLayout,
    bits: u64,
    buffer: u64,
    registry: u64,
}
fn io_reservation(inputs: IoInputs) -> Result<u64> {
    let IoInputs {
        job,
        origin,
        elf,
        static_bytes,
        tape,
        bits,
        buffer,
        registry,
    } = inputs;
    checked_sum(&[
        2 * b(job),
        6 * b(registry),
        registry_part_work()?,
        6 * b(origin),
        4 * b(elf),
        4 * b(static_bytes),
        4 * b(tape.total_bytes),
        4 * b(bits),
        2 * b(tape.payload_bytes),
        2 * b(HEADER_BYTES as u64) + b(HEADER_BYTES as u64),
        b((HEADER_BYTES - 144 + 5 * 63) as u64),
        2 * b((HEADER_BYTES - 128) as u64),
        protocol_work()?,
        4 * b(64),
        b(buffer),
        b(2 * HEADER_BYTES as u64),
        publication_tail(MAX_VIRTUAL.div_ceil(64) as u64),
    ])
}
fn intended_output(path: &Path) -> Result<PathBuf> {
    let parent = plain(
        path.parent()
            .ok_or_else(|| WorkerError::new("OUTPUT_PARENT"))?,
    )?;
    let root = parent.join(
        path.file_name()
            .ok_or_else(|| WorkerError::new("OUTPUT_NAME"))?,
    );
    match fs::symlink_metadata(&root) {
        Ok(info) => need(!info.file_type().is_symlink(), "OUTPUT_SYMLINK")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(e)),
    }
    Ok(root)
}
fn output_root(
    root: PathBuf,
    base: &str,
    uid: u32,
    budget: &mut Budget,
) -> Result<(PathBuf, File)> {
    need(
        !base.is_empty()
            && base.len() <= 64
            && base
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || v == b'_' || v == b'-'),
        "BASE_REV_HANDLE",
    )?;
    let parent = root
        .parent()
        .ok_or_else(|| WorkerError::new("OUTPUT_PARENT"))?;
    let info = parent.metadata().map_err(io_error)?;
    need(
        info.uid() == uid && info.permissions().mode() & 0o077 == 0,
        "PRIVATE_OUTPUT_PARENT",
    )?;
    if !root.exists() {
        need(base == "empty", "REVISION_CONFLICT")?;
        fs::create_dir(&root).map_err(io_error)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("workspace.lock"))
            .map_err(io_error)?;
    }
    let root = plain(&root)?;
    let info = root.metadata().map_err(io_error)?;
    need(
        info.is_dir() && info.uid() == uid && info.permissions().mode() & 0o077 == 0,
        "PRIVATE_OUTPUT_ROOT",
    )?;
    let lock_path = root.join("workspace.lock");
    let info = fs::symlink_metadata(&lock_path).map_err(io_error)?;
    need(
        info.is_file() && info.nlink() == 1 && info.len() == 0 && info.uid() == uid,
        "OUTPUT_LOCK_ALIAS",
    )?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(io_error)?;
    locked(&lock, false)?;
    match fs::symlink_metadata(root.join("HEAD")) {
        Ok(info) => {
            need(
                info.is_file()
                    && !info.file_type().is_symlink()
                    && info.nlink() == 1
                    && info.len() <= 64,
                "OUTPUT_HEAD_TYPE",
            )?;
            let mut f = File::open(root.join("HEAD")).map_err(io_error)?;
            let mut raw = Vec::new();
            Read::by_ref(&mut f)
                .take(65)
                .read_to_end(&mut raw)
                .map_err(io_error)?;
            budget.io(b(raw.len() as u64))?;
            need(raw.len() as u64 == info.len(), "OUTPUT_HEAD_CHANGED")?;
            need(raw == format!("{base}\n").as_bytes(), "REVISION_CONFLICT")?;
            return Err(WorkerError::new("ONE_SHOT_REQUIRES_FRESH_WORKSPACE"));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            need(base == "empty", "REVISION_CONFLICT")?
        }
        Err(e) => return Err(io_error(e)),
    }
    let mut count = 0;
    for entry in fs::read_dir(&root).map_err(io_error)? {
        count += 1;
        need(
            count == 1 && entry.map_err(io_error)?.file_name() == "workspace.lock",
            "NOT_FRESH_PARTIAL_OUTPUT_UNCHECKED",
        )?;
    }
    need(count == 1, "OUTPUT_LOCK_MISSING")?;
    Ok((root, lock))
}

fn output_file(
    path: &Path,
    header: &[u8; 256],
    words: &[u64],
    budget: &mut Budget,
) -> Result<(String, u64)> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    let mut sha = Sha256::new();
    budget.io(2 * b(header.len() as u64))?;
    file.write_all(header).map_err(io_error)?;
    sha.update(header);
    for value in words {
        budget.io(3)?;
        let raw = value.to_le_bytes();
        file.write_all(&raw).map_err(io_error)?;
        sha.update(raw);
    }
    file.sync_all().map_err(io_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).map_err(io_error)?;
    Ok((hex(&sha.finalize()), 256 + 8 * words.len() as u64))
}

#[derive(Debug, Default)]
struct ErrorLedger {
    budget: Option<Budget>,
    requested_work_cap: Option<u64>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePart {
    name: String,
    source_role: String,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRegistry {
    schema: String,
    protocol_sha256: String,
    parts: Vec<SourcePart>,
    source_manifest: FileId,
}
fn registry_part_work() -> Result<u64> {
    PARTS.iter().try_fold(0u64, |work, (_, _, raw)| {
        checked_sum(&[work, b(raw.len() as u64)])
    })
}
fn verify_registry(
    file: &mut InputFile,
    expected: &FileId,
    protocol_sha: &[u8; 32],
    budget: &mut Budget,
) -> Result<FileId> {
    let raw = file.small()?;
    budget.io(2 * b(file.bytes))?; // actual bounded read and SHA of parsed bytes
    need(
        hex(&Sha256::digest(&raw)) == expected.sha256,
        "REGISTRY_PARSED_BYTES",
    )?;
    let value: SourceRegistry =
        serde_json::from_value(session_json::parse(&raw).map_err(io_error)?).map_err(io_error)?;
    need(
        value.schema == "rusteda.g1-source-registry.v3"
            && value.parts.len() == PARTS.len()
            && digest(&value.protocol_sha256)? == *protocol_sha,
        "REGISTRY_SCHEMA_PARTS_PROTOCOL",
    )?;
    for (record, (name, source_role, source)) in value.parts.iter().zip(PARTS) {
        budget.fixed_event()?; // one bounded source-part identity comparison
        budget.io(b(source.len() as u64))?;
        need(
            record.name == name
                && record.source_role == source_role
                && record.bytes == source.len() as u64
                && digest(&record.sha256)? == <[u8; 32]>::from(Sha256::digest(source)),
            "REGISTRY_EXACT_ORDERED_SOURCE",
        )?;
    }
    need(
        value.source_manifest.path.is_absolute() && value.source_manifest.bytes <= 64 << 20,
        "SOURCE_MANIFEST_RECORD",
    )?;
    digest(&value.source_manifest.sha256)?;
    // This exact FileId is an evidence leaf. The independent build/launcher joins it to
    // compiled source and this worker; it is not an arbitrary recursively opened path.
    Ok(value.source_manifest)
}

fn run(ledger: &mut ErrorLedger) -> Result<()> {
    let started = Instant::now();
    let args = args()?;
    digest(&args.sha)?;
    let uid = fs::symlink_metadata(format!("/proc/{}", std::process::id()))
        .map_err(io_error)?
        .uid();
    let mut job = InputFile::open(&args.inputs, SMALL, uid)?;
    let job_parent = job
        .path
        .parent()
        .ok_or_else(|| WorkerError::new("JOB_PARENT"))?
        .metadata()
        .map_err(io_error)?;
    need(
        job_parent.uid() == uid && job_parent.permissions().mode() & 0o277 == 0,
        "SEALED_PRIVATE_JOB_PARENT",
    )?;
    let job_id = job.fingerprint()?;
    need(job_id.sha256 == args.sha, "JOB_SHA")?;
    let raw = job.small()?;
    need(
        hex(&Sha256::digest(&raw)) == job_id.sha256,
        "JOB_PARSED_BYTES",
    )?;
    let bootstrap = 4 * b(job.bytes);
    let request_value = session_json::parse(&raw).map_err(io_error)?;
    let keys = request_value
        .as_object()
        .ok_or_else(|| WorkerError::new("JOB_OBJECT"))?;
    let wanted = BTreeSet::from([
        "schema",
        "backend",
        "input_workspace",
        "input_revision",
        "static_view",
        "query_tape",
        "worker",
        "protocol_sha256",
        "origin",
        "limits",
        "source_registry",
        "task_abi",
        "device_abi",
        "cuda_image",
    ]);
    need(
        keys.keys().map(String::as_str).collect::<BTreeSet<_>>() == wanted,
        "EXACT_V3_JOB_FIELDS",
    )?;
    let request: Request = serde_json::from_value(request_value).map_err(io_error)?;
    let mut limits: SessionLimits = request.limits.into();
    need(
        request.schema == "rusteda.g1-csr-session-inputs.v3" && request.input_revision == "rev1",
        "JOB_SCHEMA_OR_REVISION",
    )?;
    need(
        matches!(request.backend, Backend::CpuRef),
        "BACKEND_UNAVAILABLE_PHASE_A_CPU_ONLY",
    )?;
    need(
        request.task_abi == 1 && request.device_abi == 0 && request.cuda_image.is_none(),
        "PHASE_A_CPU_ABI_OR_IMAGE",
    )?;
    limits.validate()?;
    ledger.requested_work_cap = Some(limits.max_work);
    // Keep one immutable whole-command terminal reserve outside the one mutable
    // query/session ledger. It is never lent to a prefix or reset. Even if the
    // success publication promise releases its unused IO holds, a subsequent
    // bounded error attempt remains within the original requested aggregate cap.
    limits.max_work = limits
        .max_work
        .checked_sub(ERROR_TAIL_WORK)
        .ok_or_else(|| WorkerError::new("before large IO: error-tail reservation"))?;
    let input = plain(&request.input_workspace)?;
    need(
        !job.path.starts_with(&input),
        "JOB_MUST_BE_EXTERNAL_TO_SEALED_INPUT",
    )?;
    let info = input.metadata().map_err(io_error)?;
    need(
        info.is_dir() && info.uid() == uid && info.permissions().mode() & 0o077 == 0,
        "PRIVATE_INPUT_ROOT",
    )?;
    let lock_info = fs::symlink_metadata(input.join("workspace.lock")).map_err(io_error)?;
    need(
        lock_info.is_file()
            && lock_info.nlink() == 1
            && lock_info.len() == 0
            && lock_info.uid() == uid
            && lock_info.permissions().mode() & 0o222 == 0,
        "SEALED_INPUT_LOCK",
    )?;
    let input_lock = File::open(input.join("workspace.lock")).map_err(io_error)?;
    let opened_lock = input_lock.metadata().map_err(io_error)?;
    need(
        (opened_lock.dev(), opened_lock.ino()) == (lock_info.dev(), lock_info.ino())
            && opened_lock.is_file()
            && opened_lock.len() == 0
            && opened_lock.nlink() == 1
            && opened_lock.uid() == uid
            && opened_lock.permissions().mode() & 0o222 == 0,
        "INPUT_LOCK_OPEN_ALIAS",
    )?;
    locked(&input_lock, true)?;
    inventory(&input, &request.origin, uid)?;
    let revision = input.join("revisions/rev1");
    need(
        request.static_view.path == revision.join("static.rbin")
            && request.query_tape.path == revision.join("queries.rbin")
            && request.origin.record().path == revision.join(request.origin.record_name())
            && request
                .origin
                .bits()
                .is_none_or(|v| v.path == revision.join("legacy-bits.rbin")),
        "EXACT_ROLE_PATHS",
    )?;
    let mut statics = open_expected(&request.static_view, limits.max_static_bytes, uid)?;
    let mut tape = open_expected(&request.query_tape, limits.max_tape_bytes, uid)?;
    let mut origin = open_expected(request.origin.record(), SMALL, uid)?;
    let mut bits = request
        .origin
        .bits()
        .map(|v| open_expected(v, 128 << 10, uid))
        .transpose()?;
    let executable = plain(&std::env::current_exe().map_err(io_error)?)?;
    need(executable == request.worker.path, "ACTUAL_WORKER_PATH")?;
    let mut worker = open_expected(&request.worker, 128 << 20, uid)?;
    let mut registry = open_expected(&request.source_registry, SMALL, uid)?;
    need(
        registry.path.parent() == job.path.parent() && registry.path != job.path,
        "REGISTRY_PRIVATE_JOB_PARENT",
    )?;
    let mut unique = BTreeSet::new();
    for file in [&job, &statics, &tape, &origin, &worker, &registry]
        .into_iter()
        .chain(bits.iter())
    {
        let m = file.file.metadata().map_err(io_error)?;
        need(unique.insert((m.dev(), m.ino())), "ROLE_INODE_ALIAS")?;
    }
    let layout = TapeLayout::from_file_bytes(tape.bytes, limits)?;
    let buffer_bytes = u64::from(40u32) * layout.queries.min(limits.max_tile_queries as u64);
    let io = io_reservation(IoInputs {
        job: job.bytes,
        origin: origin.bytes,
        elf: worker.bytes,
        static_bytes: statics.bytes,
        tape: layout,
        bits: bits.as_ref().map_or(0, |f| f.bytes),
        buffer: buffer_bytes,
        registry: registry.bytes,
    })?;
    let control = Budget::control_reservation(layout.queries, limits.max_threads)?;
    ledger.budget = Some(Budget::new(limits.max_work, bootstrap, io, control)?);
    let budget = ledger
        .budget
        .as_mut()
        .ok_or_else(|| WorkerError::new("GLOBAL_LEDGER"))?;
    budget.fixed_event()?;
    let source_head = head(&input, uid, budget)?;
    registry.verify(&request.source_registry, budget)?;
    statics.verify(&request.static_view, budget)?;
    tape.verify(&request.query_tape, budget)?;
    worker.verify(&request.worker, budget)?;
    origin.verify(request.origin.record(), budget)?;
    if let (Some(file), Some(expected)) = (&mut bits, request.origin.bits()) {
        file.verify(expected, budget)?;
    }
    let protocol_sha = protocol(budget)?;
    need(
        protocol_sha == digest(&request.protocol_sha256)?,
        "PROTOCOL_SHA",
    )?;
    let source_manifest = verify_registry(
        &mut registry,
        &request.source_registry,
        &protocol_sha,
        budget,
    )?;
    let origin_raw = origin.small()?;
    budget.io(b(origin.bytes))?;
    let origin_sha = Sha256::digest(&origin_raw);
    budget.io(b(origin.bytes))?;
    need(
        hex(&origin_sha) == request.origin.record().sha256,
        "ORIGIN_PARSED_BYTES",
    )?;
    let origin_value = session_json::parse(&origin_raw).map_err(io_error)?;
    let origin_fields = origin_value
        .as_object()
        .ok_or_else(|| WorkerError::new("ORIGIN_OBJECT"))?;
    let wanted = BTreeSet::from([
        "schema",
        "static_view",
        "query_tape",
        "observer_legacy_bits",
        "counts",
        "provenance",
    ]);
    need(
        origin_fields
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == wanted,
        "EXACT_ORIGIN_FIELDS",
    )?;
    let record: OriginRecord = serde_json::from_value(origin_value).map_err(io_error)?;
    need(
        record.schema == request.origin.schema()
            && record.static_view == request.static_view
            && record.query_tape == request.query_tape
            && record.observer_legacy_bits.as_ref() == request.origin.bits()
            && record.provenance.is_object(),
        "ORIGIN_BINDING",
    )?;
    budget.fixed_event()?;
    let intended = intended_output(&args.workspace)?;
    need(
        !intended.starts_with(&input)
            && !input.starts_with(&intended)
            && !job.path.starts_with(&intended),
        "INPUT_OUTPUT_ALIAS",
    )?;
    let (root, _output_lock) = output_root(intended, &args.base, uid, budget)?;
    budget.fixed_event()?;
    let mapping = mapped(&statics)?;
    need(mapping.len() as u64 == statics.bytes, "MAPPED_STATIC_BYTES")?;
    let decoded = frame::decode_static(&mapping, limits)?;
    budget.io(b(decoded.zero_scan_bytes))?;
    budget.fixed_event()?;
    let view = decoded.view;
    let counts = Counts {
        instances: view.instances.len() as u64,
        templates: view
            .template_offsets
            .len()
            .checked_sub(1)
            .ok_or_else(|| WorkerError::new("TEMPLATE_COUNT"))? as u64,
        rectangles: view.rectangles.len() as u64,
        virtual_shapes: view.virtual_count as u64,
        bins: u64::from(view.grid.width) * u64::from(view.grid.height),
        entries: view.entries.len() as u64,
        queries: layout.queries,
    };
    need(record.counts == counts, "ORIGIN_COUNTS")?;
    if let Some(file) = &bits {
        need(
            file.bytes == 8 * counts.virtual_shapes.div_ceil(64),
            "OBSERVER_BITS_EXTENT",
        )?;
    }
    let prepared = PreparedStatic::new(view, limits, budget)?;
    let mut session = Session::new(prepared, limits, layout.queries)?;
    tape.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut header_raw = [0u8; HEADER_BYTES];
    let mut footer_raw = [0u8; HEADER_BYTES];
    session.io(b((2 * HEADER_BYTES) as u64))?; // explicit stack-buffer zero initialization
    tape.file.read_exact(&mut header_raw).map_err(io_error)?;
    session.io(b(HEADER_BYTES as u64))?;
    let header = TapeHeader::decode(&header_raw, layout)?;
    need(
        header.static_sha == digest(&request.static_view.sha256)?,
        "TAPE_STATIC_SHA",
    )?;
    session.io(b((HEADER_BYTES - 128) as u64))?;
    let header_sha: [u8; 32] = Sha256::digest(header_raw.as_slice()).into();
    session.io(b(HEADER_BYTES as u64))?;
    session.fixed_event()?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(buffer_bytes as usize)
        .map_err(io_error)?;
    need(
        buffer.capacity() as u64 <= 3 << 20,
        "ACTUAL_QUERY_BUFFER_CAPACITY",
    )?;
    buffer.resize(buffer_bytes as usize, 0);
    session.io(b(buffer_bytes))?;
    let mut remaining = layout.queries;
    let mut read_pass = BytePass::default();
    let mut sha_pass = BytePass::default();
    let mut payload_sha = Sha256::new();
    while remaining != 0 {
        let n = remaining.min(limits.max_tile_queries as u64);
        let count = usize::try_from(n * 40).map_err(io_error)?;
        need(count <= buffer.len(), "QUERY_BUFFER_EXTENT")?;
        read_pass.record(count as u64, layout.payload_bytes, &mut session)?;
        tape.file
            .read_exact(&mut buffer[..count])
            .map_err(io_error)?;
        payload_sha.update(&buffer[..count]);
        sha_pass.record(count as u64, layout.payload_bytes, &mut session)?;
        session.push(Table::<Query>::bytes(&buffer[..count])?)?;
        remaining -= n;
    }
    need(
        read_pass.bytes == layout.payload_bytes && sha_pass.bytes == layout.payload_bytes,
        "COMPLETE_PAYLOAD_BYTES",
    )?;
    tape.file.read_exact(&mut footer_raw).map_err(io_error)?;
    session.io(b(HEADER_BYTES as u64))?;
    header.validate_footer(&footer_raw, header_sha, payload_sha.finalize().into())?;
    session.io(b((HEADER_BYTES - 128) as u64))?;
    session.fixed_event()?;
    let mut eof = [0u8; 1];
    need(
        tape.file.read(&mut eof).map_err(io_error)? == 0,
        "TAPE_TRAILING_BYTES",
    )?;
    let activation = session.finish()?;
    statics.verify(&request.static_view, budget)?;
    tape.verify(&request.query_tape, budget)?;
    origin.verify(request.origin.record(), budget)?;
    worker.verify(&request.worker, budget)?;
    if let (Some(file), Some(expected)) = (&mut bits, request.origin.bits()) {
        file.verify(expected, budget)?;
    }
    job.verify(&job_id, budget)?;
    registry.verify(&request.source_registry, budget)?;
    need(
        head(&input, uid, budget)? == source_head,
        "INPUT_HEAD_CHANGED",
    )?;
    inventory(&input, &request.origin, uid)?;
    budget.fixed_event()?;
    drop(mapping);
    let words = activation.marked.len() as u64;
    let total_output = 256 + 8 * words + RECEIPT as u64 + 256 + 512 + 64 + 5;
    need(
        total_output <= limits.max_output_bytes,
        "OUTPUT_AGGREGATE_BYTES",
    )?;
    let io_tail = publication_tail(words);
    let final_io = budget
        .io_completed()
        .checked_add(io_tail)
        .ok_or_else(|| WorkerError::new("FINAL_IO_OVERFLOW"))?;
    let promise = budget.publication_promise(io_tail, 2)?;
    let header = ActivationHeader {
        virtual_shapes: counts.virtual_shapes,
        queries: counts.queries,
        words,
        success_work: promise,
        static_sha: digest(&request.static_view.sha256)?,
        tape_sha: digest(&request.query_tape.sha256)?,
        protocol_sha,
        workers: activation.stats.workers as u32,
        prefixes: activation.stats.executed_prefixes,
    }
    .encode()?;
    budget.io(HEADER_WORK)?;
    let revisions = root.join("revisions");
    fs::create_dir(&revisions).map_err(io_error)?;
    let revision = revisions.join("rev1");
    fs::create_dir(&revision).map_err(io_error)?;
    let (output_sha, output_bytes) = output_file(
        &revision.join("activation.rbin"),
        &header,
        &activation.marked,
        budget,
    )?;
    let budget_before_receipt =
        serde_json::to_value(BudgetSnapshotDto::from(budget.snapshot())).map_err(io_error)?;
    let receipt = serde_json::json!({"schema":"rusteda.g1-csr-session-result.v3","status":"ok_component_only","backend":"cpu_ref_session_v3",
        "job_sha256":job_id.sha256,"origin_sha256":request.origin.record().sha256,"static_sha256":request.static_view.sha256,
        "tape_sha256":request.query_tape.sha256,"worker_sha256":request.worker.sha256,"protocol_sha256":hex(&protocol_sha),
        "observer_legacy_bits_sha256":request.origin.bits().map(|f|f.sha256.as_str()),"output_sha256":output_sha,"output_bytes":output_bytes,
        "source_registry":request.source_registry,"source_manifest":source_manifest,"task_abi":1,"device_abi":0,"cuda_image":null,"phase":"phase_a_cpu_only_placeholders",
        "requested_max_work":ledger.requested_work_cap,"fixed_error_tail_reservation":ERROR_TAIL_WORK,"success_unused_error_tail":ERROR_TAIL_WORK,
        "counts":counts,"stats":SessionStatsDto::from(activation.stats),"budget_before_receipt":budget_before_receipt,"io_completed_before_receipt":budget.io_completed(),
        "io_work_on_success":final_io,"total_work_on_success":promise,"completion_authority":"requires actual exit0 and HEADrev1",
        "actual_native_processes":1u64,"actual_pid":std::process::id(),"elapsed_before_receipt_seconds":started.elapsed().as_secs_f64(),
        "timing_scope":"before-receipt only; external command/exit/cleanup timing required",
        "legacy_equivalence":"unchecked_requires_independent_capture_verification","corner":"not_applicable","wire_model":"integer_activation_only",
        "unconstrained_endpoints":"not_applicable","drc_status":"unchecked","lvs_status":"unchecked"});
    let mut raw = serde_json::to_vec(&receipt).map_err(io_error)?;
    need(raw.len() <= RECEIPT, "RECEIPT_CAP")?;
    raw.resize(RECEIPT, b' ');
    budget.io(b(RECEIPT as u64))?;
    let report = revision.join("report.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report)
        .map_err(io_error)?;
    file.write_all(&raw).map_err(io_error)?;
    budget.io(b(RECEIPT as u64))?;
    file.sync_all().map_err(io_error)?;
    fs::set_permissions(report, fs::Permissions::from_mode(0o444)).map_err(io_error)?;
    fs::set_permissions(&revision, fs::Permissions::from_mode(0o500)).map_err(io_error)?;
    fs::set_permissions(&revisions, fs::Permissions::from_mode(0o500)).map_err(io_error)?;
    need(!root.join("HEAD").exists(), "REVISION_CONFLICT")?;
    budget.fixed_event()?;
    let temporary = root.join("HEAD.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(io_error)?;
    file.write_all(b"rev1\n").map_err(io_error)?;
    budget.io(b(5))?;
    file.sync_all().map_err(io_error)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o444)).map_err(io_error)?;
    fs::rename(&temporary, root.join("HEAD")).map_err(io_error)?;
    File::open(&root)
        .map_err(io_error)?
        .sync_all()
        .map_err(io_error)?;
    budget.fixed_event()?;
    std::io::stdout().write_all(STATUS).map_err(io_error)?;
    budget.io(b(STATUS.len() as u64))?;
    budget.complete_success(promise)?;
    Ok(())
}
fn main() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!("{{\"status\":\"unverified_component\",\"diagnostic\":\"COMPONENT_PANIC\"}}")
    }));
    let mut ledger = ErrorLedger::default();
    if let Err(error) = run(&mut ledger) {
        // One fixed512-byte encode/pad + write tail is reserved before large IO. Bootstrap failures
        // precede global admission and explicitly have unavailable aggregate accounting.
        let snapshot = ledger.budget.as_ref().map(Budget::snapshot);
        let tail_charged = snapshot.is_some_and(|s| {
            s.completed
                .checked_add(s.io_held)
                .and_then(|n| n.checked_add(s.control_held))
                .and_then(|n| n.checked_add(s.query_held))
                .and_then(|n| n.checked_add(ERROR_TAIL_WORK))
                .zip(ledger.requested_work_cap)
                .is_some_and(|(used, cap)| used <= cap)
        });
        let diagnostic: String = error
            .diagnostic
            .chars()
            .take(120)
            .map(|c| {
                if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .collect();
        let value = serde_json::json!({"status":"unverified_component","diagnostic":diagnostic,"stage":error.stage,
            "first_error_global_ordinal":error.ordinal,"global_admission":snapshot.is_some(),
            "attempted_work":snapshot.map(|s|s.completed+ERROR_TAIL_WORK),"io_work":snapshot.map(|s|s.io_completed+ERROR_TAIL_WORK),
            "held_safety_work":snapshot.map(|s|s.io_held+s.control_held+s.query_held),
            "error_tail_charged":tail_charged,"legacy_equivalence":"unchecked"});
        let mut raw = serde_json::to_vec(&value).unwrap_or_default();
        if raw.len() > 511 || raw.is_empty() {
            raw = b"{\"status\":\"unverified_component\",\"diagnostic\":\"BOUNDED_ERROR\",\"failed_work_scope\":\"attempted charges plus held safety; not exact completed traversal\"}".to_vec();
        }
        raw.resize(511, b' ');
        raw.push(b'\n');
        let _ = std::io::stderr().write_all(&raw);
        std::process::exit(2);
    }
}

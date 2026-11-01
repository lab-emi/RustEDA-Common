// SPDX-License-Identifier: Apache-2.0
//! Bounded image/gate adapter. No CUDA call occurs while this object is built.
use super::*;
use rusteda_compute::cuda::VerifiedPtx;

const AUX_CAP: u64 = 2 << 20;
const PTX_CAP: u64 = 8 << 20;
const MAPS_CAP: u64 = 2 << 20;
const TOOLS: [&str; 14] = [
    "backend",
    "cargo",
    "cargo_oxide",
    "cuda_header",
    "curand_header",
    "libclang",
    "libclang_cpp",
    "libdevice",
    "llc",
    "nvcc",
    "rust_llvm",
    "rustc",
    "rustc_driver",
    "system_llvm",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompilerTool {
    role: String,
    file: FileId,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    protocol_sha256: String,
    source_manifest: FileId,
    entry: String,
    arch: String,
    device_abi: u32,
    parameter_bytes: u32,
    ptx: FileId,
    cpu_gate: FileId,
    compiler_gate: FileId,
    compiler_tools: Vec<CompilerTool>,
    runtime_images: Vec<FileId>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CpuGate {
    schema: String,
    status: String,
    protocol_sha256: String,
    source_manifest: FileId,
    cuda_image: Option<FileId>,
    worker: FileId,
    manual_review: FileId,
    tiny_review: FileId,
    real_reviews: [FileId; 2],
    word_tolerance: u32,
    outcome_tolerance: u32,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompilerGate {
    schema: String,
    status: String,
    protocol_sha256: String,
    source_manifest: FileId,
    entry: String,
    device_abi: u32,
    ptx: FileId,
    builds: [FileId; 2],
    independent_review: FileId,
}

fn image_record(value: &FileId, cap: u64) -> Result<()> {
    need(
        value.path.is_absolute()
            && value.path.as_os_str().len() <= 4096
            && value.bytes != 0
            && value.bytes <= cap,
        "GPU_FILEID_EXTENT",
    )?;
    digest(&value.sha256)?;
    Ok(())
}
fn typed<T: serde::de::DeserializeOwned>(raw: &[u8], wanted: &[&str]) -> Result<T> {
    let value = session_json::parse(raw).map_err(io_error)?;
    let fields = value
        .as_object()
        .ok_or_else(|| WorkerError::new("GPU_JSON_OBJECT"))?;
    need(
        fields.keys().map(String::as_str).collect::<BTreeSet<_>>()
            == wanted.iter().copied().collect::<BTreeSet<_>>(),
        "GPU_EXACT_JSON_KEYS",
    )?;
    serde_json::from_value(value).map_err(io_error)
}
fn sealed(expected: &FileId, cap: u64, uid: u32) -> Result<InputFile> {
    image_record(expected, cap)?;
    open_expected(expected, cap, uid)
}

/// System/runtime image roles are deliberately separate from immutable data.
/// Paths are exact real files; UID is root or the caller, group/world writes
/// reject. The full current bytes and metadata are checked on the same fd.
#[derive(Debug)]
struct RuntimeImage {
    file: File,
    before: fs::Metadata,
    expected: FileId,
}
impl RuntimeImage {
    fn open(expected: &FileId, uid: u32) -> Result<Self> {
        image_record(expected, 256 << 20)?;
        let path = plain(&expected.path)?;
        let before = fs::symlink_metadata(&path).map_err(io_error)?;
        need(
            before.is_file()
                && before.len() == expected.bytes
                && before.nlink() != 0
                && [0, uid].contains(&before.uid())
                && before.mode() & 0o022 == 0,
            "GPU_RUNTIME_IMAGE_ROLE",
        )?;
        let file = File::open(&path).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        need(
            (opened.dev(), opened.ino(), opened.len())
                == (before.dev(), before.ino(), before.len()),
            "GPU_RUNTIME_IMAGE_OPEN",
        )?;
        Ok(Self {
            file,
            before,
            expected: expected.clone(),
        })
    }
    fn verify(&mut self, sink: &mut impl IoSink) -> Result<()> {
        sink.io(fingerprint_work(self.expected.bytes))?;
        self.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut left = self.expected.bytes;
        let mut chunk = [0_u8; 65536];
        let mut hash = Sha256::new();
        while left != 0 {
            let n = usize::try_from(left.min(chunk.len() as u64)).map_err(io_error)?;
            self.file.read_exact(&mut chunk[..n]).map_err(io_error)?;
            hash.update(&chunk[..n]);
            left -= n as u64;
        }
        need(
            self.file.read(&mut chunk[..1]).map_err(io_error)? == 0,
            "GPU_RUNTIME_IMAGE_GREW",
        )?;
        let opened = self.file.metadata().map_err(io_error)?;
        let named = fs::symlink_metadata(&self.expected.path).map_err(io_error)?;
        let tuple = |m: &fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mode(),
                m.uid(),
                m.nlink(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        need(
            tuple(&opened) == tuple(&self.before)
                && tuple(&named) == tuple(&self.before)
                && hex(&hash.finalize()) == self.expected.sha256,
            "GPU_RUNTIME_IMAGE_CHANGED",
        )
    }
}

/// A bounded manifest bootstrap precedes the global ledger, exactly as the job
/// bootstrap does. All auxiliary sizes are then stat-admitted before payloads.
pub(super) struct ImagePlan {
    manifest_file: InputFile,
    manifest_id: FileId,
    manifest: Manifest,
    cpu: InputFile,
    compiler: InputFile,
    source: InputFile,
    ptx: InputFile,
    runtime: Vec<RuntimeImage>,
    leaves: Vec<(InputFile, FileId)>,
    bootstrap: u64,
    aux_hold: u64,
    map_observations: Vec<Vec<[u64; 3]>>,
}
impl ImagePlan {
    pub fn bootstrap(expected: &FileId, uid: u32, cap: u64) -> Result<Self> {
        let mut manifest_file = sealed(expected, SMALL, uid)?;
        let bootstrap = 4 * b(expected.bytes);
        need(bootstrap <= cap, "GPU_BOOTSTRAP_WORK")?;
        need(
            manifest_file.fingerprint()? == *expected,
            "GPU_MANIFEST_FILEID",
        )?;
        let raw = manifest_file.small()?;
        need(
            hex(&Sha256::digest(&raw)) == expected.sha256,
            "GPU_MANIFEST_PARSED_BYTES",
        )?;
        let manifest: Manifest = typed(
            &raw,
            &[
                "schema",
                "protocol_sha256",
                "source_manifest",
                "entry",
                "arch",
                "device_abi",
                "parameter_bytes",
                "ptx",
                "cpu_gate",
                "compiler_gate",
                "compiler_tools",
                "runtime_images",
            ],
        )?;
        need(
            manifest.schema == "rusteda.g1-cuda-image.v1"
                && manifest.entry == "rusteda_g1_crossings_v1"
                && manifest.arch == "sm_80"
                && manifest.device_abi == 1
                && manifest.parameter_bytes == 80,
            "GPU_IMAGE_ABI_ENTRY_ARCH",
        )?;
        need(
            manifest.compiler_tools.len() == TOOLS.len()
                && manifest
                    .compiler_tools
                    .iter()
                    .zip(TOOLS)
                    .all(|(r, n)| r.role == n),
            "GPU_COMPILER_TOOL_ROLES",
        )?;
        for role in &manifest.compiler_tools {
            image_record(&role.file, 1 << 30)?;
        }
        need(
            (1..=8).contains(&manifest.runtime_images.len()),
            "GPU_RUNTIME_IMAGE_COUNT",
        )?;
        let total = manifest.runtime_images.iter().try_fold(0_u64, |sum, f| {
            sum.checked_add(f.bytes)
                .ok_or_else(|| WorkerError::new("GPU_RUNTIME_IMAGE_SUM"))
        })?;
        need(total <= 256 << 20, "GPU_RUNTIME_IMAGE_AGGREGATE")?;
        let cpu = sealed(&manifest.cpu_gate, SMALL, uid)?;
        let compiler = sealed(&manifest.compiler_gate, SMALL, uid)?;
        let source = sealed(&manifest.source_manifest, AUX_CAP, uid)?;
        let ptx = sealed(&manifest.ptx, PTX_CAP, uid)?;
        let mut runtime = Vec::new();
        runtime
            .try_reserve_exact(manifest.runtime_images.len())
            .map_err(io_error)?;
        let mut seen = BTreeSet::new();
        for expected in &manifest.runtime_images {
            let image = RuntimeImage::open(expected, uid)?;
            need(
                seen.insert((image.before.dev(), image.before.ino())),
                "GPU_RUNTIME_IMAGE_ALIAS",
            )?;
            runtime.push(image);
        }
        // Opaque gate-review leaves are not parsed as authority by this worker.
        // Their exact actual-chain interpretation is required in Root's request.
        // Reserve their fixed maximum before either gate JSON is read.
        let aux_hold = 10 * 4 * b(AUX_CAP) + 4 * b(128 << 20);
        Ok(Self {
            manifest_file,
            manifest_id: expected.clone(),
            manifest,
            cpu,
            compiler,
            source,
            ptx,
            runtime,
            leaves: Vec::new(),
            bootstrap,
            aux_hold,
            map_observations: Vec::new(),
        })
    }
    pub fn bootstrap_work(&self) -> u64 {
        self.bootstrap
    }
    pub fn io_hold(&self) -> Result<u64> {
        checked_sum(&[
            2 * b(self.manifest_id.bytes),
            6 * b(self.cpu.bytes),
            6 * b(self.compiler.bytes),
            4 * b(self.source.bytes),
            6 * b(self.ptx.bytes),
            4 * self
                .runtime
                .iter()
                .map(|f| b(f.expected.bytes))
                .sum::<u64>(),
            2 * b(MAPS_CAP),
            self.aux_hold,
        ])
    }
    pub fn verify_open(
        &mut self,
        protocol: &[u8; 32],
        source: &FileId,
        worker: &FileId,
        uid: u32,
        budget: &mut Budget,
    ) -> Result<VerifiedPtx> {
        need(
            digest(&self.manifest.protocol_sha256)? == *protocol
                && self.manifest.source_manifest == *source,
            "GPU_IMAGE_SOURCE_PROTOCOL",
        )?;
        self.cpu.verify(&self.manifest.cpu_gate, budget)?;
        self.compiler.verify(&self.manifest.compiler_gate, budget)?;
        self.source.verify(&self.manifest.source_manifest, budget)?;
        let mut read_doc = |file: &mut InputFile, expected: &FileId| -> Result<Vec<u8>> {
            budget.io(2 * b(file.bytes))?;
            let raw = file.small()?;
            need(
                hex(&Sha256::digest(&raw)) == expected.sha256,
                "GPU_GATE_PARSED_BYTES",
            )?;
            Ok(raw)
        };
        let cpu: CpuGate = typed(
            &read_doc(&mut self.cpu, &self.manifest.cpu_gate)?,
            &[
                "schema",
                "status",
                "protocol_sha256",
                "source_manifest",
                "cuda_image",
                "worker",
                "manual_review",
                "tiny_review",
                "real_reviews",
                "word_tolerance",
                "outcome_tolerance",
            ],
        )?;
        let compiler: CompilerGate = typed(
            &read_doc(&mut self.compiler, &self.manifest.compiler_gate)?,
            &[
                "schema",
                "status",
                "protocol_sha256",
                "source_manifest",
                "entry",
                "device_abi",
                "ptx",
                "builds",
                "independent_review",
            ],
        )?;
        need(
            cpu.schema == "rusteda.g1-cuda-cpu-gate.v1"
                && cpu.status == "passed_fresh_cpu_component_only"
                && digest(&cpu.protocol_sha256)? == *protocol
                && cpu.source_manifest == *source
                && cpu.cuda_image.is_none()
                && cpu.worker == *worker
                && cpu.word_tolerance == 0
                && cpu.outcome_tolerance == 0,
            "GPU_FRESH_CPU_GATE",
        )?;
        need(
            compiler.schema == "rusteda.g1-cuda-compiler-gate.v1"
                && compiler.status == "passed_compiler_artifact_only"
                && digest(&compiler.protocol_sha256)? == *protocol
                && compiler.source_manifest == *source
                && compiler.entry == self.manifest.entry
                && compiler.device_abi == 1
                && compiler.ptx == self.manifest.ptx,
            "GPU_COMPILER_GATE",
        )?;
        need(
            compiler.builds[0] != compiler.builds[1],
            "GPU_TWO_DISTINCT_BUILD_RECEIPTS",
        )?;
        let references = [
            cpu.manual_review,
            cpu.tiny_review,
            cpu.real_reviews[0].clone(),
            cpu.real_reviews[1].clone(),
            compiler.builds[0].clone(),
            compiler.builds[1].clone(),
            compiler.independent_review,
        ];
        self.leaves
            .try_reserve_exact(references.len())
            .map_err(io_error)?;
        let mut identities = BTreeSet::new();
        for expected in references {
            let mut file = sealed(&expected, AUX_CAP, uid)?;
            let m = file.file.metadata().map_err(io_error)?;
            need(
                identities.insert((m.dev(), m.ino())),
                "GPU_GATE_REVIEW_ALIAS",
            )?;
            file.verify(&expected, budget)?;
            self.leaves.push((file, expected));
        }
        for image in &mut self.runtime {
            image.verify(budget)?;
        }
        self.ptx.verify(&self.manifest.ptx, budget)?;
        budget.io(2 * b(self.ptx.bytes))?; // bounded read plus VerifiedPtx SHA
        self.ptx.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(self.ptx.bytes as usize)
            .map_err(io_error)?;
        need(raw.capacity() as u64 <= PTX_CAP, "GPU_PTX_CAPACITY")?;
        Read::by_ref(&mut self.ptx.file)
            .take(self.ptx.bytes + 1)
            .read_to_end(&mut raw)
            .map_err(io_error)?;
        need(raw.len() as u64 == self.ptx.bytes, "GPU_PTX_READ_EXTENT")?;
        VerifiedPtx::verify(raw, self.ptx.bytes, digest(&self.manifest.ptx.sha256)?)
            .map_err(io_error)
    }
    pub fn verify_maps(&mut self, sink: &mut impl IoSink) -> Result<()> {
        need(self.map_observations.len() < 2, "GPU_MAP_OBSERVATION_COUNT")?;
        let path = format!("/proc/{}/maps", std::process::id());
        let mut file = File::open(path).map_err(io_error)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(MAPS_CAP as usize + 1)
            .map_err(io_error)?;
        Read::by_ref(&mut file)
            .take(MAPS_CAP + 1)
            .read_to_end(&mut raw)
            .map_err(io_error)?;
        need(raw.len() as u64 <= MAPS_CAP, "GPU_MAPS_BYTES")?;
        sink.io(b(raw.len() as u64))?;
        let text = std::str::from_utf8(&raw).map_err(io_error)?;
        let mut lines = 0;
        let mut cuda = false;
        let mut observed = BTreeSet::new();
        for line in text.lines() {
            lines += 1;
            need(lines <= 16384 && line.len() <= 8192, "GPU_MAPS_ROWS")?;
            let Some(at) = line.find('/') else {
                continue;
            };
            let path = &line[at..];
            let name = Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if name.starts_with("libcuda.") || name.starts_with("libnvidia-") {
                let (index, image) = self
                    .runtime
                    .iter()
                    .enumerate()
                    .find(|(_, f)| f.expected.path == Path::new(path))
                    .ok_or_else(|| WorkerError::new("GPU_UNDECLARED_MAPPED_IMAGE"))?;
                need(!path.contains(" (deleted)"), "GPU_DELETED_MAPPED_IMAGE")?;
                let mut fields = line.split_whitespace();
                fields.next();
                fields.next();
                fields.next();
                let device = fields
                    .next()
                    .ok_or_else(|| WorkerError::new("GPU_MAP_DEVICE"))?;
                let inode = fields
                    .next()
                    .ok_or_else(|| WorkerError::new("GPU_MAP_INODE"))?
                    .parse::<u64>()
                    .map_err(io_error)?;
                let (major, minor) = device
                    .split_once(':')
                    .ok_or_else(|| WorkerError::new("GPU_MAP_DEVICE"))?;
                let major = u64::from_str_radix(major, 16).map_err(io_error)?;
                let minor = u64::from_str_radix(minor, 16).map_err(io_error)?;
                let dev = image.before.dev();
                let expected_major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xfffff000);
                let expected_minor = (dev & 0xff) | ((dev >> 12) & 0xffffff00);
                need(
                    inode == image.before.ino()
                        && (major, minor) == (expected_major, expected_minor),
                    "GPU_MAPPED_FILE_IDENTITY",
                )?;
                observed.insert([index as u64, dev, inode]);
                cuda |= name.starts_with("libcuda.");
            }
        }
        need(cuda, "GPU_DRIVER_MAPPING_MISSING")?;
        self.map_observations.push(observed.into_iter().collect());
        Ok(())
    }
    pub fn runtime_image_count(&self) -> usize {
        self.runtime.len()
    }
    pub fn map_observations(&self) -> &[Vec<[u64; 3]>] {
        &self.map_observations
    }
    pub fn finish(&mut self, budget: &mut Budget) -> Result<()> {
        self.manifest_file.verify(&self.manifest_id, budget)?;
        self.cpu.verify(&self.manifest.cpu_gate, budget)?;
        self.compiler.verify(&self.manifest.compiler_gate, budget)?;
        self.source.verify(&self.manifest.source_manifest, budget)?;
        self.ptx.verify(&self.manifest.ptx, budget)?;
        for (file, expected) in &mut self.leaves {
            file.verify(expected, budget)?;
        }
        for image in &mut self.runtime {
            image.verify(budget)?;
        }
        self.verify_maps(budget)
    }
}

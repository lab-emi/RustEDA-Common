// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The CUDA runtime (feature `cuda`, COMPUTE-REQ-0002, COMPUTE-REQ-0004): kernels written in
//! CUDA C are compiled at run time by NVRTC for the architecture of the device and launched
//! through the CUDA driver API. The CUDA driver and NVRTC libraries are loaded at run time; builds
//! need no CUDA toolkit.

// Backend glue over the CUDA driver and NVRTC C APIs (AGENTS rules 8.4): every `unsafe` block
// carries its justification.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex, OnceLock};

use cudarc::driver::{CudaContext, CudaModule, DriverError};
pub use cudarc::driver::{
    CudaFunction, CudaSlice, CudaStream, DeviceRepr, LaunchConfig, PinnedHostSlice, PushKernelArg,
    ValidAsZeroBits,
};
use cudarc::nvrtc::{Ptx, result as nr, sys as ns};

use crate::GpuError;

impl From<DriverError> for GpuError {
    fn from(e: DriverError) -> GpuError {
        GpuError(format!("CUDA driver: {e}"))
    }
}

/// Releases an NVRTC program when dropped, so that every exit path of compilation destroys its
/// program (COMPUTE-REQ-0004).
struct Program(ns::nvrtcProgram);

impl Drop for Program {
    fn drop(&mut self) {
        // SAFETY: `self.0` was created by `nvrtc::result::create_program` and is destroyed only
        // here.
        let _ = unsafe { nr::destroy_program(self.0) };
    }
}

/// The default CUDA device with its stream and the modules compiled for it.
pub struct Device {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    arch: String,
    name: String,
    /// Compiled modules keyed by their source text and complete NVRTC option list.
    modules: Mutex<HashMap<(&'static str, Vec<String>), Arc<CudaModule>>>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("arch", &self.arch)
            .finish_non_exhaustive()
    }
}

static DEVICE: OnceLock<Result<Device, GpuError>> = OnceLock::new();

/// The default device, opened once per process.
pub fn device() -> Result<&'static Device, GpuError> {
    match DEVICE.get_or_init(open) {
        Ok(d) => Ok(d),
        Err(e) => Err(e.clone()),
    }
}

fn open() -> Result<Device, GpuError> {
    // SAFETY: the probes only load the shared libraries; no CUDA function is called.
    if !unsafe { cudarc::driver::sys::is_culib_present() } {
        return Err(GpuError("CUDA driver library (libcuda) not found".into()));
    }
    // SAFETY: as above.
    if !unsafe { cudarc::nvrtc::sys::is_culib_present() } {
        return Err(GpuError(
            "NVRTC library (libnvrtc) not found; add the library directory of a CUDA toolkit to \
             LD_LIBRARY_PATH"
                .into(),
        ));
    }
    let ctx = CudaContext::new(0)?;
    let (maj, min) = ctx.compute_capability()?;
    let name = ctx.name()?;
    let stream = ctx.new_stream()?;
    let suffix = if maj >= 9 { "a" } else { "" };
    Ok(Device {
        ctx,
        stream,
        arch: format!("sm_{maj}{min}{suffix}"),
        name,
        modules: Mutex::new(HashMap::new()),
    })
}

impl Device {
    /// The device name and architecture (COMPUTE-REQ-0004).
    pub fn describe(&self) -> String {
        format!("{} ({})", self.name, self.arch)
    }

    pub fn stream(&self) -> &Arc<CudaStream> {
        &self.stream
    }

    /// A new stream on this device's context (COMPUTE-REQ-0004).
    pub fn new_stream(&self) -> Result<Arc<CudaStream>, GpuError> {
        Ok(self.ctx.new_stream()?)
    }

    /// Free and total device memory in bytes (COMPUTE-REQ-0004).
    pub fn mem_info(&self) -> Result<(usize, usize), GpuError> {
        self.ctx.bind_to_thread()?;
        Ok(cudarc::driver::result::mem_get_info()?)
    }

    /// Page-locked host memory of at least `len` elements, not write-combined (COMPUTE-REQ-0004),
    /// so that the host reads it at full speed after a device-to-host copy.
    pub fn pinned<T: DeviceRepr>(&self, len: usize) -> Result<PinnedHostSlice<T>, GpuError> {
        self.ctx.bind_to_thread()?;
        // SAFETY: callers read the slice only after a device-to-host copy has written it; flag 0
        // selects cached, non-write-combined page-locked memory.
        Ok(unsafe { self.ctx.alloc_pinned_with_flags::<T>(len.max(1), 0) }?)
    }

    /// Kernel `name` of the CUDA C source `src` with the default options, compiled once per
    /// process.
    pub fn function(&self, src: &'static str, name: &str) -> Result<CudaFunction, GpuError> {
        self.function_with(src, &[], name)
    }

    /// Kernel `name` of the CUDA C source `src` compiled with extra NVRTC `options` (for example
    /// `-DREAL_F64` and `--fmad=false`), which replace the default options they name; cached per
    /// process by source and complete option list (COMPUTE-REQ-0004).
    pub fn function_with(
        &self,
        src: &'static str,
        options: &[&str],
        name: &str,
    ) -> Result<CudaFunction, GpuError> {
        // Module loading does not bind the context to the calling thread by itself.
        self.ctx.bind_to_thread()?;
        // Default code generation settings (COMPUTE-REQ-0002): architecture-specific target from
        // compute capability 9.0 on, NVRTC's default fused multiply-add contraction, correctly
        // rounded division and square root, no flush to zero.
        let mut opts = vec![
            format!("--gpu-architecture={}", self.arch),
            "--std=c++17".into(),
            "--fmad=true".into(),
            "--prec-div=true".into(),
            "--prec-sqrt=true".into(),
            "--ftz=false".into(),
        ];
        for &opt in options {
            if let Some((k, _)) = opt.split_once('=')
                && opt.starts_with("--")
                && let Some(slot) = opts
                    .iter_mut()
                    .find(|o| o.split_once('=').is_some_and(|(ok, _)| ok == k))
            {
                *slot = opt.into();
            } else {
                opts.push(opt.into());
            }
        }
        let key = (src, opts);
        let module = {
            let mut m = self
                .modules
                .lock()
                .map_err(|_| GpuError("module cache poisoned".into()))?;
            match m.get(&key) {
                Some(x) => x.clone(),
                None => {
                    let x = self.compile(src, &key.1)?;
                    m.insert(key, x.clone());
                    x
                }
            }
        };
        Ok(module.load_function(name)?)
    }

    /// Compiles CUDA C to a CUBIN for this device's architecture with `opts` and loads it.
    fn compile(&self, src: &str, opts: &[String]) -> Result<Arc<CudaModule>, GpuError> {
        let err = |what: &str, e: &dyn std::fmt::Debug| GpuError(format!("NVRTC {what}: {e:?}"));
        let csrc = CString::new(src).map_err(|e| err("source", &e))?;
        let prog = Program(nr::create_program(&csrc, None).map_err(|e| err("create", &e))?);
        // SAFETY: `prog.0` was created above and `Program`'s `Drop` destroys it on every return
        // path below.
        let compiled = unsafe { nr::compile_program(prog.0, opts) };
        if let Err(e) = compiled {
            // SAFETY: `prog.0` is valid until `prog` drops at return.
            let log = unsafe { nr::get_program_log(prog.0) }
                .map(|l| l.iter().map(|c| *c as u8 as char).collect::<String>())
                .unwrap_or_default();
            return Err(GpuError(format!(
                "NVRTC compile: {e:?}: {}",
                log.trim_end_matches('\0')
            )));
        }
        let mut n = 0usize;
        // SAFETY: `prog.0` holds a successful compilation; `n` receives the CUBIN size.
        unsafe { ns::nvrtcGetCUBINSize(prog.0, &mut n) }
            .result()
            .map_err(|e| err("CUBIN size", &e))?;
        let mut cubin = vec![0u8; n];
        // SAFETY: `cubin` has exactly the size NVRTC reported.
        unsafe { ns::nvrtcGetCUBIN(prog.0, cubin.as_mut_ptr().cast()) }
            .result()
            .map_err(|e| err("CUBIN", &e))?;
        drop(prog);
        Ok(self.ctx.load_module(Ptx::from_binary(cubin))?)
    }

    /// Copies `data` to a new device array.
    pub fn upload<T: DeviceRepr>(&self, data: &[T]) -> Result<CudaSlice<T>, GpuError> {
        Ok(self.stream.clone_htod(data)?)
    }

    /// A zero-filled device array (at least one element: some kernels bind every argument).
    pub fn zeros<T: DeviceRepr + ValidAsZeroBits>(
        &self,
        len: usize,
    ) -> Result<CudaSlice<T>, GpuError> {
        Ok(self.stream.alloc_zeros(len.max(1))?)
    }

    /// Copies a device array to the host after all queued work has finished; execution errors
    /// of earlier launches are reported here.
    pub fn read<T: DeviceRepr>(&self, s: &CudaSlice<T>) -> Result<Vec<T>, GpuError> {
        let v = self.stream.clone_dtoh(s)?;
        self.stream.synchronize()?;
        Ok(v)
    }

    /// Waits for all work queued on the stream.
    pub fn sync(&self) -> Result<(), GpuError> {
        Ok(self.stream.synchronize()?)
    }
}

/// One-dimensional launch shape for `units` work items in blocks of `block` threads.
pub fn launch_1d(units: usize, block: u32) -> LaunchConfig {
    LaunchConfig {
        grid_dim: ((units as u32).div_ceil(block).max(1), 1, 1),
        block_dim: (block, 1, 1),
        shared_mem_bytes: 0,
    }
}

/// Maximum admitted prebuilt PTX text, independent of any compiler image cache.
pub const MAX_PREBUILT_PTX_BYTES: usize = 8 << 20;
/// Fixed G1 driver entry; the ABI probe is deliberately not a selectable image.
pub const G1_PTX_ENTRY: &str = "rusteda_g1_crossings_v1";

/// Hash-checked, bounded PTX text with the exact G1 entry parameter shape.
///
/// This verifies the artifact bytes, not the external source/tool/CPU gate. The
/// adapter must authenticate that complete chain before calling this constructor
/// and charge its read/hash passes before any context creation. No path is opened
/// here, so the adapter can retain its same-descriptor locks and file identities.
pub struct VerifiedPtx {
    text: String,
    sha256: [u8; 32],
}

impl std::fmt::Debug for VerifiedPtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedPtx")
            .field("bytes", &self.text.len())
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl VerifiedPtx {
    /// Admits already-read PTX bytes before allowing a driver-owned lifetime.
    ///
    /// # Errors
    /// Rejects excess capacity/length, wrong hash, non-ASCII or NUL text, wrong
    /// target, duplicate/missing entry or any noncanonical parameter shape.
    pub fn verify(
        raw: Vec<u8>,
        expected_bytes: u64,
        expected_sha256: [u8; 32],
    ) -> Result<Self, GpuError> {
        use sha2::{Digest, Sha256};

        if raw.is_empty()
            || raw.len() > MAX_PREBUILT_PTX_BYTES
            || raw.capacity() > MAX_PREBUILT_PTX_BYTES
            || raw.len() as u64 != expected_bytes
            || !raw.is_ascii()
            || raw.contains(&0)
        {
            return Err(GpuError("prebuilt PTX byte admission".into()));
        }
        let sha256: [u8; 32] = Sha256::digest(&raw).into();
        if sha256 != expected_sha256 {
            return Err(GpuError("prebuilt PTX identity mismatch".into()));
        }
        let text = String::from_utf8(raw).map_err(|_| GpuError("prebuilt PTX encoding".into()))?;
        validate_g1_ptx_signature(&text)?;
        Ok(Self { text, sha256 })
    }

    /// Digest of the exact bytes transferred to the driver JIT.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Length of the exact bounded text owned until module loading completes.
    pub fn bytes(&self) -> usize {
        self.text.len()
    }
}

fn validate_g1_ptx_signature(text: &str) -> Result<(), GpuError> {
    let bad = || GpuError("prebuilt PTX target or 80-byte entry ABI".into());
    if !text.is_ascii() || text.len() > MAX_PREBUILT_PTX_BYTES {
        return Err(bad());
    }
    let mut lexer = PtxLexer { text, at: 0 };
    let mut previous = None;
    let mut targets = 0;
    let mut entries = 0;
    while let Some(token) = lexer.next()? {
        if token == PtxToken::Word(".target") {
            if lexer.next()? != Some(PtxToken::Word("sm_80")) {
                return Err(bad());
            }
            targets += 1;
        } else if token == PtxToken::Word(".entry") {
            let name = lexer.next()?.ok_or_else(bad)?;
            if name == PtxToken::Word(G1_PTX_ENTRY) {
                if previous != Some(PtxToken::Word(".visible"))
                    || lexer.next()? != Some(PtxToken::Punctuation(b'('))
                {
                    return Err(bad());
                }
                entries += 1;
                let start = lexer.at;
                let mut count = 0;
                loop {
                    if lexer.at - start > 4096
                        || lexer.next()? != Some(PtxToken::Word(".param"))
                        || lexer.next()? != Some(PtxToken::Word(".u64"))
                    {
                        return Err(bad());
                    }
                    if matches!(count, 0 | 2 | 7 | 8)
                        && (lexer.next()? != Some(PtxToken::Word(".ptr"))
                            || lexer.next()? != Some(PtxToken::Word(".align"))
                            || lexer.next()? != Some(PtxToken::Word("1")))
                    {
                        return Err(bad());
                    }
                    let Some(PtxToken::Word(name)) = lexer.next()? else {
                        return Err(bad());
                    };
                    if name.is_empty()
                        || name.len() > 128
                        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    {
                        return Err(bad());
                    }
                    count += 1;
                    match lexer.next()? {
                        Some(PtxToken::Punctuation(b',')) if count < 10 => {}
                        Some(PtxToken::Punctuation(b')')) if count == 10 => break,
                        _ => return Err(bad()),
                    }
                }
            }
        }
        previous = Some(token);
    }
    if targets != 1 || entries != 1 {
        return Err(bad());
    }
    // This is a structural entry ABI check, not an instruction/atomic proof.
    // The independent artifact gate supplies the named-entry semantic proof.
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PtxToken<'text> {
    Word(&'text str),
    Punctuation(u8),
    Quoted,
}

/// One forward bounded scan; comments/quoted filenames cannot invent an entry.
struct PtxLexer<'text> {
    text: &'text str,
    at: usize,
}

impl<'text> PtxLexer<'text> {
    fn next(&mut self) -> Result<Option<PtxToken<'text>>, GpuError> {
        let raw = self.text.as_bytes();
        let bad = || GpuError("unterminated prebuilt PTX lexical field".into());
        while self.at < raw.len() {
            if raw[self.at].is_ascii_whitespace() {
                self.at += 1;
                continue;
            }
            if raw.get(self.at..self.at + 2) == Some(b"//") {
                self.at += 2;
                while self.at < raw.len() && raw[self.at] != b'\n' {
                    self.at += 1;
                }
                continue;
            }
            if raw.get(self.at..self.at + 2) == Some(b"/*") {
                self.at += 2;
                while self.at < raw.len() && raw.get(self.at..self.at + 2) != Some(b"*/") {
                    self.at += 1;
                }
                if self.at == raw.len() {
                    return Err(bad());
                }
                self.at += 2;
                continue;
            }
            if raw[self.at] == b'"' {
                self.at += 1;
                loop {
                    let byte = *raw.get(self.at).ok_or_else(bad)?;
                    self.at += 1;
                    if byte == b'"' {
                        break;
                    }
                    if byte == b'\\' {
                        raw.get(self.at).ok_or_else(bad)?;
                        self.at += 1;
                    }
                }
                return Ok(Some(PtxToken::Quoted));
            }
            let word = |byte: u8| byte.is_ascii_alphanumeric() || b"_.$".contains(&byte);
            if word(raw[self.at]) {
                let first = self.at;
                while self.at < raw.len() && word(raw[self.at]) {
                    self.at += 1;
                }
                return Ok(Some(PtxToken::Word(&self.text[first..self.at])));
            }
            let byte = raw[self.at];
            self.at += 1;
            return Ok(Some(PtxToken::Punctuation(byte)));
        }
        Ok(None)
    }
}

/// Conservative simultaneous device allocation model for one bounded G1 tile.
///
/// Host scope/file-cache/opaque driver admission remains the adapter's separate
/// obligation before `open`. These are VRAM terms, not a host-RAM fit claim.
#[derive(Clone, Copy, Debug)]
pub struct DeviceAdmission {
    payload_bytes: u64,
    padded_threads: u32,
}

impl DeviceAdmission {
    /// Creates a bounded payload/local-memory reservation without device calls.
    ///
    /// # Errors
    /// Rejects payloads above40MiB or a nonzero thread count not padded to the
    /// fixed128-thread block size, or beyond one65,536-query tile.
    pub fn new(payload_bytes: u64, padded_threads: u32) -> Result<Self, GpuError> {
        if payload_bytes > 40 << 20
            || padded_threads > 65_536
            || !padded_threads.is_multiple_of(128)
        {
            return Err(GpuError("prebuilt PTX device admission".into()));
        }
        Ok(Self {
            payload_bytes,
            padded_threads,
        })
    }

    fn required_vram(self) -> Result<u64, GpuError> {
        self.payload_bytes
            .checked_add(u64::from(self.padded_threads) * 256)
            .and_then(|n| n.checked_add(256 << 20))
            .and_then(|n| n.checked_add(64 << 20))
            .ok_or_else(|| GpuError("prebuilt PTX VRAM arithmetic".into()))
    }
}

/// Actual function and device observations, independent of host cgroup peaks.
#[derive(Clone, Copy, Debug)]
pub struct PtxDeviceStats {
    pub free_device_bytes_after_load: usize,
    pub total_device_bytes: usize,
    pub local_bytes_per_thread: u32,
    pub registers_per_thread: u32,
    pub shared_bytes_per_block: u32,
    pub reserved_vram_bytes: u64,
}

/// One foreground driver-only context, stream and hash-bound prebuilt module.
///
/// Unlike [`device`], this owns no global cache and does not probe NVRTC, compile
/// CUDA C, or run the existing NVRTC self-test. Source/protocol authorization and
/// a separately hash-bound driver-only self-test remain required adapter gates.
pub struct PtxDevice {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    _module: Arc<CudaModule>,
    function: CudaFunction,
    description: String,
    stats: PtxDeviceStats,
}

impl std::fmt::Debug for PtxDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtxDevice")
            .field("description", &self.description)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl PtxDevice {
    /// Creates an owned native context only after pure artifact admission.
    ///
    /// # Errors
    /// Returns unavailable/driver errors or rejects capability, actual local
    /// storage, block size or free-VRAM observations. Never selects CPU fallback.
    pub fn open(ptx: VerifiedPtx, admission: DeviceAdmission) -> Result<Self, GpuError> {
        let required = admission.required_vram()?;
        if std::env::var_os("CUDA_CACHE_DISABLE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return Err(GpuError(
                "prebuilt PTX requires recorded CUDA_CACHE_DISABLE=1".into(),
            ));
        }
        // SAFETY: this only probes the driver shared library; no NVRTC symbol is
        // accessed. The complete host resource and artifact proof precedes it.
        if !unsafe { cudarc::driver::sys::is_culib_present() } {
            return Err(GpuError("CUDA driver library (libcuda) not found".into()));
        }
        let ctx = CudaContext::new(0)?;
        let (major, minor) = ctx.compute_capability()?;
        if major < 8 {
            return Err(GpuError("prebuilt G1 PTX requires sm_80 or later".into()));
        }
        let description = format!("{} (sm_{major}{minor})", ctx.name()?);
        let stream = ctx.new_stream()?;
        let module = ctx.load_module(Ptx::from_src(ptx.text))?;
        let function = module.load_function(G1_PTX_ENTRY)?;
        let local = u32::try_from(function.local_size_bytes()?)
            .map_err(|_| GpuError("negative CUDA local size".into()))?;
        let registers = u32::try_from(function.num_regs()?)
            .map_err(|_| GpuError("negative CUDA register count".into()))?;
        let shared = u32::try_from(function.shared_size_bytes()?)
            .map_err(|_| GpuError("negative CUDA shared size".into()))?;
        if local > 256 || shared != 0 || function.max_threads_per_block()? < 128 {
            return Err(GpuError(
                "prebuilt PTX actual function memory or block bound".into(),
            ));
        }
        ctx.bind_to_thread()?;
        let (free, total) = cudarc::driver::result::mem_get_info()?;
        if u64::try_from(free).map_err(|_| GpuError("CUDA free memory width".into()))? < required {
            return Err(GpuError("prebuilt PTX free VRAM admission".into()));
        }
        Ok(Self {
            ctx,
            stream,
            _module: module,
            function,
            description,
            stats: PtxDeviceStats {
                free_device_bytes_after_load: free,
                total_device_bytes: total,
                local_bytes_per_thread: local,
                registers_per_thread: registers,
                shared_bytes_per_block: shared,
                reserved_vram_bytes: required,
            },
        })
    }

    pub fn describe(&self) -> &str {
        &self.description
    }
    pub fn stats(&self) -> PtxDeviceStats {
        self.stats
    }
    pub fn stream(&self) -> &Arc<CudaStream> {
        &self.stream
    }
    pub fn function(&self) -> &CudaFunction {
        &self.function
    }

    /// Synchronizes while the owned context/module remain alive.
    pub fn sync(&self) -> Result<(), GpuError> {
        Ok(self.stream.synchronize()?)
    }

    /// Explicit timed synchronization before releasing this foreground owner.
    pub fn close(self) -> Result<(), GpuError> {
        self.ctx.bind_to_thread()?;
        self.sync()
    }
}

/// Source of the device self-test kernel: `y[i] = 2 x[i] + 1` for `i < n`.
const SELF_TEST_SRC: &str = r#"
extern "C" __global__ void self_test(const float* x, float* y, unsigned n) {
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) { y[i] = 2.0f * x[i] + 1.0f; }
}
"#;

/// Runs the self-test kernel and compares every result exactly (COMPUTE-REQ-0001).
pub fn self_test(dev: &Device) -> Result<(), GpuError> {
    let n = 1000usize;
    let x: Vec<f32> = (0..n).map(|i| i as f32 * 0.25 - 17.0).collect();
    let xd = dev.upload(&x)?;
    let mut yd = dev.zeros::<f32>(n)?;
    let f = dev.function(SELF_TEST_SRC, "self_test")?;
    let n32 = n as u32;
    let mut b = dev.stream.launch_builder(&f);
    b.arg(&xd).arg(&mut yd).arg(&n32);
    // SAFETY: the kernel reads `n` floats from `xd` and writes `n` floats to `yd`, both of which
    // hold at least `n` elements; `n32` matches the kernel's `unsigned` parameter.
    unsafe { b.launch(launch_1d(n, 128)) }?;
    let y = dev.read(&yd)?;
    for (i, (a, b)) in x.iter().zip(&y).enumerate() {
        if *b != 2.0 * a + 1.0 {
            return Err(GpuError(format!(
                "device self-test failed at element {i}: {b} instead of {}",
                2.0 * a + 1.0
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> String {
        let parameters = (0..10)
            .map(|index| {
                let pointer = if matches!(index, 0 | 2 | 7 | 8) {
                    " .ptr .align 1"
                } else {
                    ""
                };
                format!(".param .u64{pointer} arg_{index}")
            })
            .collect::<Vec<_>>()
            .join(",\n");
        format!(".visible .entry {G1_PTX_ENTRY}({parameters}) {{ ret; }}\n")
    }

    fn verify_text(text: &str) -> Result<VerifiedPtx, GpuError> {
        use sha2::{Digest, Sha256};
        VerifiedPtx::verify(
            text.as_bytes().to_vec(),
            text.len() as u64,
            Sha256::digest(text.as_bytes()).into(),
        )
    }

    #[test]
    fn prebuilt_ptx_identity_and_structural_entry_are_checked_without_device() {
        let text = format!(".version 8.0\n.target sm_80\n.address_size 64\n{}", entry());
        let checked = verify_text(&text).expect("bounded structural artifact");
        assert_eq!(checked.bytes(), text.len());
        assert!(VerifiedPtx::verify(text.into_bytes(), 1, [0; 32]).is_err());
    }

    #[test]
    fn comments_and_quoted_paths_cannot_supply_an_entry() {
        let fake = entry();
        assert!(verify_text(&format!(".target sm_80\n/* {fake} */")).is_err());
        assert!(verify_text(&format!(".target sm_80\n.file 1 {fake:?}\n")).is_err());
        assert!(verify_text(&format!(".target sm_80\n// {}", fake.replace('\n', " "))).is_err());
        let valid = format!(
            ".target sm_80\n{}",
            entry().replace(".visible", ".visible /* ignored */")
        );
        assert!(verify_text(&valid).is_ok());
    }

    #[test]
    fn prebuilt_ptx_rejects_ambiguous_widths_entries_and_lexical_tails() {
        let valid = format!(".target sm_80\n{}", entry());
        assert!(verify_text(&valid.replace(".u64 arg_9", ".u32 arg_9")).is_err());
        assert!(verify_text(&valid.replace(".align 1", ".align 8")).is_err());
        assert!(verify_text(&format!("{valid}{}", entry())).is_err());
        assert!(verify_text(&valid.replace("sm_80", "sm_70")).is_err());
        assert!(verify_text(&format!("{valid}/* unterminated")).is_err());
        assert!(verify_text(&format!("{valid}\"unterminated")).is_err());
    }

    #[test]
    fn prebuilt_ptx_capacity_is_admitted_before_hash_or_context() {
        let mut excess = Vec::with_capacity(MAX_PREBUILT_PTX_BYTES + 1);
        excess.push(b';');
        let error = VerifiedPtx::verify(excess, 1, [0; 32]).expect_err("excess live capacity");
        assert_eq!(error.to_string(), "prebuilt PTX byte admission");
    }

    #[test]
    fn device_model_counts_padded_threads_and_keeps_vram_separate() {
        let admitted = DeviceAdmission::new(40 << 20, 65_536).expect("maximum bounded tile");
        assert_eq!(
            admitted.required_vram().expect("bounded arithmetic"),
            376 << 20
        );
        assert!(DeviceAdmission::new(40 << 20, 127).is_err());
        assert!(DeviceAdmission::new((40 << 20) + 1, 128).is_err());
        assert!(DeviceAdmission::new(0, 65_664).is_err());
    }

    // COMPUTE-REQ-0004: every W0 device item keeps its signature and every additive item is
    // callable with its specified signature.
    #[test]
    fn device_api_is_additive() {
        fn _assert_signatures(dev: &Device) -> Result<(), GpuError> {
            let _d: &'static Device = device()?;
            let _st: &Arc<CudaStream> = dev.stream();
            let _f1: CudaFunction = dev.function("extern \"C\" __global__ void k() {}", "k")?;
            let _f2: CudaFunction = dev.function_with(
                "extern \"C\" __global__ void k() {}",
                &["--fmad=false", "-DREAL_F64"],
                "k",
            )?;
            let slice: CudaSlice<f32> = dev.upload(&[1.0f32, 2.0f32])?;
            let _z: CudaSlice<u32> = dev.zeros::<u32>(4)?;
            let _host: Vec<f32> = dev.read(&slice)?;
            dev.sync()?;
            let _cfg: LaunchConfig = launch_1d(10, 64);
            self_test(dev)?;
            let _pin: PinnedHostSlice<f64> = dev.pinned::<f64>(8)?;
            let _new_st: Arc<CudaStream> = dev.new_stream()?;
            let (_free, _total): (usize, usize) = dev.mem_info()?;
            let _desc: String = dev.describe();
            Ok(())
        }
        let cfg = launch_1d(257, 128);
        assert_eq!(cfg.grid_dim, (3, 1, 1));
        assert_eq!(cfg.block_dim, (128, 1, 1));
    }
}

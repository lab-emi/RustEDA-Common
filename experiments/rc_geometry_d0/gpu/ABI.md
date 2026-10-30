# D0 cudarc launcher ABI

Activated only after the recorded independent CPU gate and timing release.
The implementation in `src/cuda.rs` uses the following session API:

```rust
Session::new(&VerifiedPtx) -> Result<Session, Error>;
Session::run(Batch<'_>, Limits) -> Result<Run, Error>;
```

`VerifiedPtx::load(manifest)` must stream-verify source/toolchain/PTX FileIds and
caps before creating a CUDA context. It rejects status `unbuilt`, missing/stale
PTX and any source mismatch. A device failure returns `Error::Device`, never a
zero-filled result. The optional GPU-required test must fail when unavailable.

Use already cached cudarc 0.19.10 and its existing `CudaContext`, `CudaStream`,
`CudaSlice`, `PushKernelArg`, `nvrtc::Ptx::from_src` and `load_module` APIs.
`from_src` holds prebuilt PTX text; it performs no NVRTC compilation. Do not
call Common's existing NVRTC-dependent `cuda::device()` for this experiment.

The device entry has six slices and one output pointer. Match the inspected
PTX signature explicitly: for each slice, emit device pointer followed by a
u64 element length; then the output pointer. Do not confuse pointer-only CUDA-C
arguments with Rust slice ABI. Pin launch block size 128, check grid arithmetic,
validate all host shapes/queries/pairs, allocate a sentinel-filled output and
enforce memory estimate <= min(512MiB, free_VRAM/2). Upload, launch, synchronize,
read back and compare all output words with CPU exactly, including zero pairs,
127/128/129/257/513 pairs, contacts, negative and extreme coordinates.

Tests must bind the PTX hash manifest, mutate one hash/source byte and require
rejection, repeat deterministically, and record unavailable GPU cases as
unverified. Keep context/JIT, input validation/packing, H2D, launch, sync, D2H
and whole-call timing separate. No GPU performance claim is authorized by
source preparation alone. Actual required tests passed separately; see evidence.json.

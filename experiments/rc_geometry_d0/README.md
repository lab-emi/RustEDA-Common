# D0: hash-bound Rust integer geometry on CPU and PTX

Status: D0 feasibility validated after the independent CPU gate passed. The
same integer predicate executed exactly on the real RTX 4090 Laptop GPU;
source/CPU-gate/PTX hash negatives and byte-reproducible PTX rebuilds passed.
This is not completed G1 or evidence of a RustROAD/end-to-end speedup. No
dependency download or system CUDA installation was used. The user explicitly
approved cuda-oxide/PTX for Common and RustROAD.

This is an independent experiment, outside the foundation workspace. It does
not change a production backend, the default `cuda` feature, RustROAD geometry,
electrical arithmetic, SPEF accumulation order or a published workspace.

## Concrete API and scope

`Batch` borrows integer SoA buffers: four bounds columns per shape/query,
shape/query layers, excluded shape *indices*, and `(query_index,shape_index)`
pairs. `cpu_ref(batch, limits)` returns one `u32` zero/one flag per input pair,
in input order, using Rayon with immutable input shared between workers.
`cuda::Session::new(verified_ptx)` and `Session::run(batch, limits)` run the same allocation-free
`predicate::hit` source once per GPU thread. No floating point, reductions,
atomics, output append buffers, paths or unbounded search are involved.

The pair predicate excludes the query's own shape, requires equal target
layers, and tests strict positive rectangle intersection in i64 DBU. D0
requires positive-area rectangles and validates every index before allocation
or launch. Unsupported/invalid inputs fail explicitly; an unavailable GPU or
missing generated PTX is unverified, never a pass. A later RustROAD integration
must preserve original shape IDs when packing local indices and compact flags
in the original pair order before its existing shape-ID sort. Virtual cell
geometry is not expanded into a new global shape universe.

This filter alone does not establish extraction acceleration. G1 requires a
measured batching/lowering strategy; CPU spatial indexing may already remove
most rejected pairs. Keep f64 capacitance calculations and their exact ordered
additions on the CPU until a separate equivalence gate proves otherwise.

The shared predicate lives at
`crates/core/rusteda-compute/src/geometry/kernels/rect_pairs.rs`, included by
both the standalone CPU driver and the pinned cuda-oxide device crate. Device
files were activated only after four CPU tests passed and the parent agent
released CPU timing isolation. Production dispatch and dependency layers are
unchanged. The default experiment build remains CPU-only.

## Upfront budgets

Hard maxima: S=4,000,000 shapes, Q=65,536 queries, P=8,000,000 pairs, at most
65,536 candidates per query, four CPU workers, one corner and one GPU launch.
Shapes cost `36*S` bytes, queries `40*Q`, pairs `8*P`, flags `4*P`.

- Device buffers: `36*S+40*Q+12*P` = 242,621,440 B (231.4 MiB).
- Conservative host bound: two input copies plus CPU/device flag copies and
  256 MiB runtime headroom: `2*(36*S+40*Q+8*P)+8*P+256MiB`, about 718.8 MiB.
- Scratch is constant per CPU worker; inputs are never cloned per worker.
- Two concurrent test cases can create 2x4 bounded CPU workers; their stack
  allowance fits the fixed runtime headroom, without multiplying input storage.
- Admission caps: 1 GiB host buffers/headroom and 512 MiB device buffers;
  device execution also requires the estimate <= half reported free VRAM.
- Per-query candidate counts and `max_pairs` bound work and output allocation;
  checked arithmetic and fallible reservations precede all retained buffers.

Execution used a fresh preflight (54 GiB available RAM, 92 GiB disk), isolated
targets and `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2`; CPU build/tests under 4 GiB RAM,
1 GiB swap, 180s; device tests under 2 GiB RAM, 512 MiB swap, 180s with GNU
`time -v`. No virtual-address-space ulimit for GPU processes. Serial GPU cases;
no run estimated above 4 GiB, otherwise take the root scale lock. Measure actual
host peak RSS and device allocation; cap ordinary test RSS below 2 GiB.

## Qualified local toolchain (read-only)

- cuda-rust revision `a4a939a9b5f01f3bf712e92386922e1bc7c434a9`, matching
  RustSpice's committed GPU crate.
- `/home/cgao/git/RustEDA/.work/gpu/cuda-rust/cuda-oxide/target/debug/cargo-oxide`
- Rust `/home/cgao/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu` with
  rustc-dev, rust-src and llvm-tools installed.
- `CUDA_TOOLKIT_PATH=/home/cgao/git/RustEDA/.work/gpu/cuda-13.4`, symlink to
  the qualified Python 3.13 NVIDIA wheels; CUDA components 13.4.92, cuRAND
  10.4.4.72. cuda.h, curand.h, nvcc and libdevice are present.
- `CUDA_OXIDE_LLC=/usr/bin/llc`, LLVM 23.1.3; driver proc metadata 615.78.08.

Current [NVIDIA installation documentation](https://nvidia.github.io/cuda-rust/cuda-oxide/getting-started/installation.html)
lists Linux, Rust 1.99.1, LLVM 21+ with NVPTX and CUDA 13+; the
[source-build guide](https://nvidia.github.io/cuda-rust/cuda-oxide/appendix/building-from-source.html)
lists the pinned Rust components. Use the qualified revision rather than moving
main. No system toolkit installation or dependency download is authorized by
this preparation step.

Existing Common `cuda::device()` probes NVRTC before opening its context. The
D0 experiment instead uses the same cudarc 0.19.10 driver primitives directly:
`CudaContext::new`, `load_module(Ptx::from_src(...))`, stream upload/launch/read
and synchronization. `Ptx::from_src` is a container for prebuilt PTX and does
not invoke NVRTC. Future production integration should add an explicit PTX
entrypoint without silently changing existing CUDA-C dispatch behavior.

## Acceptance and reproduction

1. CPU independent oracle: interval overlap via widened i128 min/max, explicit
   layer/exclusion expectations; contacts, negative coordinates, extreme i64
   bounds, invalid indices, allocation/work budgets and candidate ceilings.
2. Build the device crate offline from the pinned cached dependencies and the
   exact shared source; `--no-fmad`, `sm_80`. Inspect the generated PTX parameter
   ABI against the launcher: a Rust slice is pointer followed by u64 length.
3. Create a manifest binding the predicate, host and GPU wrappers, Cargo inputs,
   build command, tool identities, target and PTX SHA-256. Refuse stale or absent
   artifacts. Regenerate twice and require byte-identical PTX.
4. Require all GPU flags equal cpu_ref exactly (integer tolerance zero), test
   non-multiple block sizes, sentinel output initialization, repeated launches,
   and a corrupted PTX/source hash negative. GPU-required mode must fail when
   unavailable. A skipped GPU case is recorded unverified.
5. Report packing/validation, context/JIT, H2D, kernel, D2H, synchronization and
   whole-call wall time separately, including cold/warm calls and allocation
   bytes. Interleave repeated CPU/GPU observations; claim no RustROAD or
   end-to-end gain until real rev4/rev56 SPEFs and dual-PDK gates are exact.

`build_manifest.py --workspace NEW --base-rev empty --cpu-gate CPU_GATE --ptx
gpu/rusteda_rc_geometry_kernels_d0.ptx` binds all code/Cargo inputs, the passed
CPU gate and eight qualified tool files. It writes an artifact manifest with
`built_unqualified`; the separate real-device validation receipt establishes
D0 correctness. `VerifiedPtx::load` checks hashes before CUDA context creation.

Build the device crate from `gpu/` using the existing cargo-oxide executable,
`CARGO_NET_OFFLINE=true`, `CUDA_OXIDE_BACKEND=/home/cgao/.cargo/cuda-oxide/librustc_codegen_cuda.so`,
the qualified toolkit/LLVM paths above, an isolated `CARGO_TARGET_DIR`, and
`build --no-fmad --arch sm_80`. This explicit backend prevents rebuilding or
modifying the qualified cuda-oxide cache. The generated PTX has thirteen u64
parameters: six pointer/length pairs and one output pointer.

Run CPU checks first with `cargo test --offline --manifest-path Cargo.toml`.
The two GPU tests are explicitly selected with `--features cuda --test gpu --
--ignored --nocapture`, `RUSTEDA_D0_REQUIRE_GPU=1` and
`RUSTEDA_D0_PTX_MANIFEST=...`. They fail when required hardware/artifacts are
unavailable; default ignored tests never establish GPU correctness. A requested
CUDA run has no silent CPU substitution. Invalid inputs, exceeded budgets,
driver faults and sentinel output all fail explicitly.

## Recorded D0 result

- CPU gate: four independent exact/budget tests, before GPU activation.
- GPU gate: two required tests on RTX 4090 Laptop (`sm_89`), integer tolerance
  zero, including extreme i64 coordinates, strict contacts, non-block sizes,
  repeated launches, sentinel coverage and source/CPU-gate/PTX hash rejection.
- PTX rebuild: actual first and forced recompile both SHA-256
  `e056448b57d54c8354e358bb4280761372b668a3d06aeaa803e6a8018e3215b4`.
- Clippy (`--all-targets --all-features -D warnings`), docs, formatting and shared
  source integrity checks passed. Initial PTX build peak 552,708 KiB; final real
  GPU test peak 213,608 KiB; comparison process peak 396,120 KiB.

Three interleaved observations per size included fixture packing, validation,
allocation, transfers and synchronization. Final cold-process run disabled the
driver disk cache with `CUDA_CACHE_DISABLE=1` per the
[NVIDIA environment-variable contract](https://docs.nvidia.com/cuda/cuda-programming-guide/05-appendices/environment-variables.html).
At 8M pairs the medians were CPU 89.27 ms, reused GPU 87.51 ms and cold GPU 807.38 ms
with packing. Cold setup cost 719.87 ms (manifest 347.19 ms, context/JIT 372.67 ms).
Warm GPU components included packing 44.77 ms, H2D/allocation 17.11 ms,
kernel/sync 0.513 ms and D2H/sync 9.15 ms. Laptop clock variation is uncontrolled;
this near-parity result establishes no end-to-end performance benefit.

Peak simultaneously-live device buffer payload was 97,261,568 B; the sampled
global free-memory high-water delta was 100,663,296 B. The latter includes pooling
and other GPU clients, and excludes context already allocated before the
baseline. Driver/context memory was not separately isolated, rather than
reported as zero. Raw private receipts are in root
`.work/perf-20261030/d0-execution/`; `evidence.json` binds the accepted artifacts.

Future work needs a separate profile and bounded CPU oracle for keeping a
spatial index resident and generating/clipping candidates on-device, followed
by exact CPU ordering. Pair filtering after CPU packing alone is not promoted
as G1, and no production RustROAD integration was attempted.

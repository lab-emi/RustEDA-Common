# D0: hash-bound Rust integer geometry on CPU and PTX

Status: source preparation only. No Cargo build, PTX generation, GPU execution,
download, performance measurement or correctness claim has been made. The user
explicitly approved cuda-oxide/PTX for Common and RustROAD. CPU execution and
independent geometry checks must pass before the first device test.

This is an independent experiment, outside the foundation workspace. It does
not change a production backend, the default `cuda` feature, RustROAD geometry,
electrical arithmetic, SPEF accumulation order or a published workspace.

## Concrete API and scope

`Batch` borrows integer SoA buffers: four bounds columns per shape/query,
shape/query layers, excluded shape *indices*, and `(query_index,shape_index)`
pairs. `cpu_ref(batch, limits)` returns one `u32` zero/one flag per input pair,
in input order, using Rayon with immutable input shared between workers.
Planned `cuda::run(batch, limits, hash_bound_ptx)` runs the same allocation-free
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

Only the CPU crate is active at this checkpoint. Files ending `.blueprint` are
inactive device-crate source blueprints. They must not be activated or built
until the bounded pure-Rust CPU tests have actually passed and the parent agent
has released the current timing run. No PTX artifact or device test result is
present; GPU correctness remains unverified.

## Upfront budgets

Hard maxima: S=4,000,000 shapes, Q=65,536 queries, P=8,000,000 pairs, at most
65,536 candidates per query, four CPU workers, one corner and one GPU launch.
Shapes cost `36*S` bytes, queries `40*Q`, pairs `8*P`, flags `4*P`.

- Device buffers: `36*S+40*Q+12*P` = 242,621,440 B (231.4 MiB).
- Conservative host bound: two input copies plus CPU/device flag copies and
  256 MiB runtime headroom: `2*(36*S+40*Q+8*P)+8*P+256MiB`, about 718.8 MiB.
- Scratch is constant per CPU worker; inputs are never cloned per worker.
- Admission caps: 1 GiB host buffers/headroom and 512 MiB device buffers;
  device execution also requires the estimate <= half reported free VRAM.
- Per-query candidate counts and `max_pairs` bound work and output allocation;
  checked arithmetic and fallible reservations precede all retained buffers.

After the parent agent permits execution: fresh host-memory check; isolated
target; `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2`; CPU build/tests under 4 GiB RAM,
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

## Required acceptance, after permission to execute

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

# G1 isolated CSR activation component

This standalone CPU prototype passed 14 unit/CLI controls and an independent
zero-tolerance geometry audit of 40 frozen cases through 120 real helper
executions (workers 1, 2, and 4). The independent decoder also passed 42 corruption,
type, budget, and resource rejection controls. It did not call this crate's
algorithm or codec to derive its expected results.

This is a component result. CUDA/PTX, the actual RustROAD activation/legacy-work
bridge, and production dispatch are unimplemented. DRC, LVS, timing, full-flow
QoR, production memory admission, and end-to-end speedup remain unchecked. The
production Common workspace, the D0 experiment, and default dispatch are unchanged.

The crate has its own workspace and committed lockfile: MSRV 1.97, qualified
Rust 1.97.1, five pinned direct dependencies and 28 locked registry dependencies.
The implementation is pure Rust with bounded Rayon workers. [ABI.md](ABI.md)
describes the exact packet and the unresolved production streaming contract.

The view is at most 64 MiB: dense-grid CSR offsets and instance IDs, at most
100,000 instances, 1,024 template ranges, 16,384 shared local rectangles, 65,536
queries **per single batch**, and 1,000,000 canonical virtual-shape bits. Instances
carry N/FS transforms, exact transformed-template-union bounds, template IDs, and
canonical prefixes. The reference streams candidates without materializing
query-by-shape pairs. Geometry is integer DBU with checked widened transforms;
private bitsets and work/errors merge deterministically in query order.

The one-shot native helper holds a shared input lock and exclusive output lock,
checks private immutable input identities and base revision, maps the packet,
and publishes a successful rev1. CUDA requests return GPU_UNVERIFIED. Auxiliary
vectors are capped at 16 MiB, worker stacks at 8 MiB, and output aggregate at
128 KiB, including the 2,048-byte JSON receipt. A caller-owned foreground scope
must enforce memory and wall time; the helper creates no daemon or escaping scope.

Only a bounded 64 KiB job bootstrap precedes parsing the requested work budget.
Before large view/ELF reads, metadata-based admission reserves every declared
read/SHA/encode/write pass. Conservative reservation, completed work, and the
conditional successful-completion ledger are separate. Fixed-size receipt padding
avoids a self-referential byte count. Its completion totals require actual exit0
and HEAD=rev1. Parsing, metadata, EOF, and bounded error handling are explicit
control scopes, not a claim to count every CPU instruction. Engine work is capped
at 100,000,000,000; smaller configured budgets reject gracefully.

Local evidence is retained under `/home/cgao/git/RustEDA/.work/perf-20261030/`:

- `g1-component01/cpu-source01` through `cpu-source04`: immutable source history;
  the original IO ledger defect and two strict-clippy failures are preserved.
- `g1-component-build01/frozen/BUILD_QUALIFIED.json`: all five stages passed
  (fmt, 14 tests/0 ignored, strict all-target/all-feature clippy, docs, release).
  Build maximum RSS was 345,776 KiB; final target allocation 316,485,632 bytes
  stayed below the 1 GiB ceiling. Each stage used an owned 2 GiB/512 MiB scope,
  CPU28-29, job1/tests2, and 180-second deadline, with cleanup verified.
- `g1-component-build01/fixture-batches01/EXECUTED.json`: 108+3+9 actual successful
  children and 880 pinned raw files. Whole-batch GNU walls were 13.38/0.57/1.28 s;
  RSS 21,636/20,576/20,628 KiB. The FS batch cgroup peak was unavailable.
- `g1-geometry-independent01/READY.json`: independent exact CPU result,
  SHA `03adc7aef0c91eced23e933f20bfee78ed31d0979871d479e1f01c01f1c12398`.
  It binds 1,396 current FileIDs, 32,310 CSR membership checks, actual execution
  authority, and an independent IO ledger. Audit 1.07 s/RSS 33,104 KiB;
  42 decoder controls 0.05 s/RSS 24,008 KiB.

The frozen qualified helper is 6,536,248 bytes, SHA
`a8a11cea75640fbe8b12143d79aae33c94d7b9c26954d5ae11e8dce8b4a48a8f`.
Its source protocol SHA is
`7e49fd28a829c510148e93af3d075403c498ee5aba9706991dfaab4d8006a8a6`.
Later README/ABI/HANDOFF edits only join evidence; executable inputs are identical
to source04. No extra native run is implied by these documentation edits.

Production admission remains unselected: cross-phase base 2,440,091,756 bytes
plus opaque 1 GiB CUDA reserve and two 64 MiB mappings leaves 110,045,076 bytes
for every other owner. The provisional component estimate below 128 MiB is not
extra permitted production storage. Late headroom is only 5,893,652 bytes;
parent ELF, runtime, allocator, driver/kernel/cache retirement and the full
stream lifetime still require proof or rejection. Tiny-fixture RSS does not
establish those bounds. No GPU kernel, PTX, native legacy build, or new benchmark
is authorized by this README.

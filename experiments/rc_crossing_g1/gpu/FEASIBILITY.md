# First phase-B compiler milestone

This package is an explicit compiler probe, not the G1 geometry kernel. Its entry
is `rusteda_g1_abi80_atomic_or_probe`; the adapter cannot request or load it.
No source-only result is a compiled or executed GPU qualification.

Two Rust slice arguments plus six scalar/pointer arguments must become exactly
ten 64-bit PTX parameters, in the accepted static pointer/bytes, query pointer/
record count, global ordinal, layer mask, max cells, outcome pointer, activation
pointer/word count order. Queries use a fixed `[u8;40]` element so slice length is
the record count. The output slot is 16 bytes with work, status and local index.
The host digest remains a separate 24-byte ordinal/work/status/zero transcript.

The pinned backend must lower `AtomicU64::fetch_or(Relaxed)` to native global
64-bit OR, without a non-atomic load/OR/store, hidden heap, unsupported helper or
CPU fallback. If it cannot, stop at this milestone. Do not translate to CUDA C.
Inspect actual PTX parameters, instructions, local frame and diagnostics. A
compiler success alone does not prove runtime alignment, race behavior or words.

Any later required probe execution needs a new host wrapper and authorization.
It must compare every outcome/sentinel/index and all zero-initialized activation
words at tolerance zero, including repeated OR into the same word, bits 63/64,
non-block-sized counts and invalid target/status cases. Per-thread work=1 is a
probe convention only; it is not the G1 query ledger or a CPU/GPU semantic result.

Future compile proposal: two fresh private directories, pinned existing offline
cuda-oxide/backend/Rust/LLVM/libdevice/cache, `--no-fmad --arch sm_80`, job1 and
bounded target/output/logs under explicit memory/wall guards. Resolve the new
package lock canonically offline; do not hand-edit a historical lock or claim
the current placeholder lock is usable. No compile or device call has run.

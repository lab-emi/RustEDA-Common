# Foreground CPU session v2 — qualified CPU component

The frozen Source05 component has passed the bounded build/self-controls,
independent zero-tolerance v2 manual/streaming checks (129 calls), and independent
new-v1 behavior checks (120 calls). See [SESSION_V2_QUALIFICATION.md](SESSION_V2_QUALIFICATION.md)
for exact identities and limits. This is an isolated CPU component qualification;
Road v2 capture, real large Ibex, GPU, production memory and physical signoff
remain unqualified. V1 per-child PID records are unavailable.

This documentation update is separate from the executed Source05 snapshot.
The original source proposal below is retained as historical design context;
its source-only statements describe that freeze point. Frozen Source05 files,
old live FileIDs, protocols and actual execution receipts were not rewritten.

## Original Source05 proposal

This isolated experiment extends the qualified v1 component with a single
foreground CPU session. It is source-only: no compilation, tests, native calls,
new ELF, Road capture, GPU work, or production dispatch has been qualified for
these bytes. The original v1 ELF/protocol and its frozen 40-case/120-call proof
remain separate evidence. Independent manual geometry and actual tiny Road bits
remain required external oracles; the new implementation is not its own sole
oracle.

## Source and API boundary

The new binary is `rusteda-rc-crossing-g1-session`. Cargo.lock and dependencies
are unchanged; MSRV remains 1.97. `lib.rs` adds two modules and mechanically
factors query-bound validation and the original integer query loop into a meter
which retains charged work on failure. The v1 wrapper still returns the same
work/error and uses the same traversal, geometry, marking and ordering. Its
existing source tests and actual outputs must be replayed after compilation.

`PreparedStatic<'view, 'budget>` borrows both immutable geometry and one
exclusive `&mut Budget`. It transfers that same budget borrow to
`Session<'view, 'budget>`. `push(queries)` and consuming `finish()` accept no
replacement ledger. While the session exists, only `Session::io` and
`Session::fixed_event` expose transport accounting. A failed transport charge
or query closes the session to further execution. Dropping preparation does
not allow another large validation walk on the same ledger. Completion/drop
returns the borrow to the caller, which verifies final input identities and
performs publication against that same budget.

The static input is mmap-backed and borrowed. A single Rayon pool owns one
private activation bitset per actual worker. A bounded pending-outcome vector
and one query-byte buffer are reused across windows. There is no query-by-shape
pair array, whole-tape heap allocation, per-window pool, daemon, stdin request,
or per-window work-budget reset. The exact i64/i128 geometry, strict overlap,
world-template bounds, first-common-bin traversal and ordinal bit assignment
remain the v1 core semantics.

The actual canonical five-section static size S is enforced before encoding,
decoding or preparation, including S exactly equal to the requested cap.
The old semantic admission's pessimistic six-section allowance is a different
quantity: only its internal allowance receives up to 6*63 padding bytes.
The requested S cap and hard 32MiB cap remain unchanged, and all file I/O is
charged using actual S. New stats expose both actual_static_bytes and
legacy_admission_bytes; v1 Stats retain their existing meaning.

## Inputs, framing and authority

The new exact job schema is `rusteda.g1-csr-session-inputs.v2`. It pins the
current worker ELF, six-part compiled protocol, static file, query tape, origin
record, limits, input workspace and `rev1`. The job is external to the producer
workspace, uniquely owned, readonly and in a sealed private directory. JSON is
bounded to 64KiB, 4096 nodes, depth32 and 64KiB decoded text, with duplicate keys,
unknown fields, nonfinite numbers and wrong integer types rejected.

The input namespace is exactly HEAD, workspace.lock and revisions/rev1, with
static.rbin, queries.rbin and origin.json for a manual origin. An actual Road
origin instead has capture.json plus legacy-bits.rbin. Those are five or six
regular files and three directories. Unknown entries, aliases, symlinks,
writable/shared files and unrecognized framing are errors. A shared producer
lock spans first verification through final confirmation; the static mapping
is dropped before that lock. There is no second large tape copy.

The origin's exact six fields and seven integer counts follow the frozen
producer ORIGIN_CONTRACT.json (6f812552...). Road provenance is a bounded opaque
object. The consumer does not accept that object, HEAD alone, or raw legacy
bits as proof of actual producer execution; independent receipt/source/argv
verification remains required. Observer-only bits are nevertheless fully
fingerprinted and charged. Manual origins require an explicit null observer.

Static `RG1STA02` has five canonical 64-byte-aligned sections. Tape `RG1QRY02`
has a 4096-byte header, Q fixed 40-byte queries and a 4096-byte `RG1END02` footer.
Metadata alone derives Q and admission before any large hash. A complete
payload SHA, footer, exact EOF and final file/HEAD/namespace checks are required
before publication. Reserved bytes and padding must be zero. Exact static
template-union world bounds are required; a larger generic cell bbox is not
an interchangeable producer input. Grid origin is in bin coordinates.

Output uses a fresh exclusive-locked workspace, --base-rev, create_new files
and one actual HEADrev1. Existing mismatched HEAD returns REVISION_CONFLICT;
existing same HEAD or partial files are not overwritten. Every writer is under
the private output root, checked for input/output containment before mkdir.
The 256-byte activation header and words, fixed 4096-byte receipt, HEAD and
bounded stdout/stderr have an aggregate 128KiB cap. A receipt or activation
without actual exit0 and HEADrev1 is not a completed execution.

## One global work ledger

Common uses b(n)=ceil(n/64); reads and SHA passes are separate. These units are
defined operations, not elapsed time, CPU instructions, Road legacy work, or
a claim that every parser instruction is separately metered. Bounded JSON,
fixed metadata/argument checks and field formatting have declared finite
control scopes. Prefixes never acquire a new 100B allowance.

Before large I/O, reserve remaining J2b, O6b, ELF4b, S4b, tape4b, observer L4b,
payload2b, actual header/footer reads and header SHA, reserved/padding scans,
six compiled protocol name/length/source SHA passes, fixed HEAD reads,
window/stack zero initialization, activation encoding/write/SHA, fixed receipt
encode/pad/write, HEAD write, stdout and a bounded failure tail. The job's
initial fingerprint/read/parsed-buffer SHA is an independently bounded 4b(J)
bootstrap, at J<=64KiB. ELF<=128MiB, S<=32MiB, tape<=512MiB/Q<=12M, O<=64KiB
and L<=128KiB are checked from admitted file metadata before those passes.
Opening a file must preserve its admitted size, inode and readonly/unique
ownership. Every hash/read stops at the admitted size plus a one-byte EOF probe.

The semantic/control reservation is:

`Kmax + 5Q + 2*Nmax*requested_workers + 64`, where
`Kmax=1+3Imax+2Tmax+Rmax+Bmax+Emax=5,318,433` and `Nmax=ceil(1,000,000/64)`.

Successful static validation consumes its actual existing validation_work and
releases Kmax minus that value. An unsuccessful static walk retains a reserved
upper bound and has no exact partial-work or success claim. IO pass charges may
precede an attempted pass, so a failed run does not call their aggregate an exact
completed lower bound. Only successful completion reconciles every charged pass.
Query window control consumes n+1 and releases n-1; each
validated record consumes1. A launched prefix consumes1+k and releases k-1.
Thus successful query control is exactly 3Q+windows+prefixes, bounded by5Q.
Persistent bit initialization plus final OR/popcount consume2*N*actual_workers;
unused requested-worker and Nmax allowances are released exactly once.

Before launching any prefix, sum query upper bounds: 1 for an absent layer,
otherwise 1+expanded_bin_span+E+I+V. The largest affordable nonempty prefix of
the validated pending window is launched once. Unlaunched suffixes remain in
that same window. Actual charged work includes every launched outcome even
after the earliest failure. The first original global ordinal wins; an invalid
lookahead waits behind its valid earlier prefix, and an earlier budget/runtime
failure wins. Failure terminates further prefixes without retry or fallback.

Byte-pass counters carry across windows, charging b(total_after)-b(total_before).
No per-record/window rounding or hidden rehash is substituted. The pending
and query byte windows are independent of tape length and reused in place.
After all checks, publication shrinks conservative held amounts to the exact
remaining tail. The receipt states conditional successful-completion work;
only actual exit0+HEAD proves that tail finished. Pre-receipt time is labeled
as such; external end-to-end timing must include input lowering/copy/hash,
pool/compute/IPC/output/exit and whole-scope cleanup.

## Memory, hard caps and lifetimes

Hard source limits are S32MiB, tape512MiB, Q12M, one window65536, workers1..4,
aux16MiB, aggregate output128KiB, query cells1M, global work100B and the existing
I/T/R/V/B/E geometry caps. Try-reserve and actual capacities are checked before
use. Query storage is at most 2,621,440 bytes; staging capacity is capped3MiB.
The pending/worker allocation admission includes retained capacities, one
activation allowance and fixed control slack. Each pool thread has a 2MiB stack.

The standalone expected working model remains <=1100MiB: tape file cache up to
512MiB, static mapping/cache32MiB, worker ELF/cache128MiB, aux16MiB, query
buffer3MiB, four stacks8MiB, bounded JSON/source/buffers plus conservative
runtime/allocator/output/cache allowances. This is a conservative work estimate,
not a proof of every opaque runtime allocation. An eventual owned2GiB/512MiB
scope, timeout, no AS limit, GNUtime and complete cleanup must contain failures.
No component native process is active now. New test fixtures estimate <256MiB each plus
two test workers/ELF clean cache; future tests must run with jobs1/testthreads2.
Build disk/peak resources require separate root admission; no target exists.

The intended lifetime is metadata/bootstrap -> reserve -> fingerprint/map ->
static validation -> pool/stream -> footer -> consuming finish (pool/pending
retire) -> final input verification -> output. Kernel file cache may remain
charged after mappings/buffers are dropped. This standalone model is not Road
production co-residency admission. Original extraction cap3,758,096,384B and
late headroom rev4=4,265,492B/rev56=5,893,652B stay unclosed. GPU architecture,
device runtime, real large counts and production speedup remain unselected.

## Authored controls and remaining gates

New source tests cover segmentation/worker determinism, Q65537 in one session,
empty and absent-layer cases, global invalid ordinal, prelaunch shrinking without
retry, retained failed work, exact S/S-1 and conservative padding, same exclusive
ledger and transport failure, single static attempt, framing/footer corruption,
CLI early budget rejection, strict origin fields/counts, aliases, unknown files,
partial outputs and revision conflicts. A compile-fail rustdoc shows that the
budget cannot be accessed between static preparation and transfer to Session.
These are authored controls, not executed evidence.

Required next gates are frozen independent source review; explicitly authorized
bounded fmt/test/clippy/doc/release build; unchanged v1 actual regression gates;
40 independent manual geometry cases across stream segmentations/workers;
actual tiny Road capture->v2 consumer bit equality at tolerance0; independently
decoded packets/receipts/ledger and negative controls; only then separately
authorized real capture and whole-session measurement. No GPU, production
integration, physical signoff or speedup is implied by any tiny component gate.

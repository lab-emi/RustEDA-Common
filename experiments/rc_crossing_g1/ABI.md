# Component ABI v1 and production streaming boundary

This documents the implemented single-batch CPU interface. It is not a production
Road integration or a newly selected GPU architecture.

## One-shot command and authority

The helper accepts exactly four option/value pairs:
`--workspace <fresh-output> --base-rev empty --inputs <job.json> --inputs-sha <sha256>`.
Handles use only ASCII alphanumeric characters, underscore, or hyphen. Paths
must have no symlink ancestors. The input workspace is private and locked shared;
job and view are unique regular readonly files. Input HEAD must match the job's
view_revision. The output workspace is locked exclusively and only an empty
workspace can publish rev1. Conflicting HEAD fails REVISION_CONFLICT. Actual exit0,
output HEAD=rev1, and all receipt/FileIDs are required together; a provisional
receipt from a nonzero/interrupted attempt proves no successful completion.

The strict job schema is `rusteda.g1-csr-worker-inputs.v1`, with backend cpu_ref
or cuda, view_workspace, view_revision, view/worker FileIDs, protocol_sha256,
and explicit Limits. Unsupported cuda returns GPU_UNVERIFIED without fallback.
The pinned worker ELF and protocol identify the implementation. Protocol SHA is
SHA256 of two length-prefixed embedded sources (lib.rs then format.rs, each
u64 little-endian length followed by its bytes). It changes even for a mechanical
source edit. The ELF independently binds worker main and dependencies.

## Immutable packet

All fields are little-endian; coordinates are i64 DBU. Magic `RG1CSR01`, version1,
header4096 bytes, exact byte length. Header offsets24/28 hold layer/virtual counts;
offsets32/40/48 are grid origin-x, origin-y and positive bin DBU. Origin is a
**bin index**, not a DBU coordinate. Width/height u32 are at56/60. Six (offset,count)
u64 pairs start at64. Sections are canonical64-byte-aligned, with zero padding,
no aliasing/trailing bytes; reserved header160..4096 is zero.

| Section | Record bytes | Meaning |
|---|---:|---|
| grid offsets | 4 | u32 CSR offsets, cells+1 |
| grid entries | 4 | sorted unique u32 instance IDs per bin |
| instances | 96 | at[2] i64, height i64, template/prefix u32, bounds[4] i64, orientation u32, reserved28 |
| template offsets | 4 | u32 ranges, templates+1 |
| local rectangles | 48 | box[4] i64, layer/ordinal u32, reserved8 |
| queries | 40 | box[4] i64, layer u32, reserved4 |

N=0 and FS=1 are the only orientations. Final transformed boxes must fit i64;
intermediate FS arithmetic uses i128. Instance bounds equal the exact transformed
union of its template rectangles, not merely an enclosing cell body box. Empty
templates have zero bounds and no CSR membership. Every nonempty instance must
appear exactly once in every bin touched by its inclusive bounds span. Complete
coverage, canonical prefixes/ordinals, positive rectangles and layer ranges are
validated before parallel work. A query expands by1DBU only for conservative
candidate selection; activation requires strict positive overlap of the original
query and transformed rectangle on the same layer. Edge/corner contact is empty.

The output `activation.rbin` has magic `RG1ACT01`, version1, header128 bytes and
canonical u64 words. Counts at16/24/32 are virtual shapes, queries, words; offset40
contains the successful-completion work total. Bit(instance.prefix+ordinal) means
at least one query overlaps. Unused tail bits stay zero. `report.json` is exactly
2048 bytes of JSON plus whitespace; it separates reserved work, completed work
before receipt, and work on successful completion. It reports integer activation
scope and unchecked DRC/LVS/legacy status. Ordered failure selection is implemented,
but the current CLI emits bounded text, not a query ordinal field.

## Hard single-batch limits

View64MiB; output aggregate128KiB; auxiliary vectors16MiB; workers1..4; instances
100,000; templates1,024; shared rectangles16,384; bins1,000,000; CSR entries4,000,000;
queries65,536; virtual shapes1,000,000; work100,000,000,000. Smaller Limits are
honored. Query span is also capped before scanning. A caller-owned memory/time
scope is mandatory; no RLIMIT_AS is imposed for a future CUDA-safe caller.

The current packet maps static geometry and one query batch together. It neither
accepts a query stream nor retains a GPU context. There is no actual RustROAD
lowering, original-work equivalence, partial capacitance accumulation, or SPEF
reuse claim in this component.

## Required future production stream (unimplemented)

Existing frozen `g1-design01/metadata-v2.json` derives 11,808,534 queries from the
Ibex profile (472,341,360 raw query-record bytes), or181 tiles at65,536. This is a
counter-derived design quantity, not a newly measured stream. Exact selected
V/template/bin inventory and the real legacy bridge remain outstanding. The
original profile has85,836 instances; other metadata bounds are conditional and
must not substitute for actual lowering admission.

Spawning181 cold helpers, resetting work budgets per tile, or repeatedly copying
all CSR geometry would not establish a production benefit. A later versioned
protocol must use one bounded foreground compute process for the entire stream:

- Keep one immutable geometry/index view and its actual workspace/HEAD/FileIDs
  pinned for the full interval. Prove every actual orientation/template/index
  invariant; do not pass a legacy cell bbox as an exact template-union bound.
- Lower queries incrementally into a bounded tile without constructing the
  complete query vector or any query-by-instance/shape pairs. Retain at most the
  declared tile/mmap/pinned buffers; account for producer and consumer coexistence.
- Carry one aggregate work/byte/output ledger across all tiles, including source
  lowering, hashing, IPC, mapping, synchronization and publication. Do not reset
  the original engine work cap or retain an unbounded receipt/result list.
- Accumulate one canonical integer activation bitset, publish only a whole
  successful result, and reject a tile failure without presenting partial data
  as checked. A global query/error position needs an explicit new schema.
- Use no daemon, prompt, detached scope, or floating-point partial reduction.
  Reuse only the original caller's owned cgroup/affinity and base-revision lock
  contract; measure full startup/lowering/transfer/sync/exit/cleanup costs.
- Retire worker-owned mappings/address space before late coupling. Parent/child
  ELF, allocator/pinned staging, driver/kernel/cache residual and late phase
  limits still require proof under the unchanged original cap; worker exit alone
  is not a complete charge-retirement proof.

This is an interface/acceptance boundary only. Implementing or launching a stream,
CUDA kernel/PTX, or native Road bridge requires separate coordination. Current
CPU8 crossing is only6.87043208s of a106.5425574s command; these tiny fixture
measurements neither predict a GPU gain nor close production admission.

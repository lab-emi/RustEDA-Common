# Proposed exact image and CPU gate (source only)

The eventual v3 `cuda_image` FileID names an immutable JSON image manifest, not
PTX text. Both remain separately hash bound. The CPU request keeps this value
null. No artifact or source in this tree is qualified by this document.

Manifest `rusteda.g1-cuda-image.v1` has exact keys: `schema`, `protocol_sha256`,
`source_manifest`, `entry`, `arch`, `device_abi`, `parameter_bytes`, `ptx`,
`cpu_gate`, `compiler_gate`, `compiler_tools`, `runtime_images`. Entry is
`rusteda_g1_crossings_v1`, arch `sm_80`, ABI1 and parameter bytes80. Manifest,
CPU gate and compiler gate are each at most65536 bytes under the existing strict
duplicate-aware lexical JSON parser. PTX is nonempty, ASCII, no NUL, <=8MiB.
Compiler tools have exactly the14 recorded role/FileID pairs, ordered by role;
their payload bytes are immutable build provenance, not reopened unused runtime
inputs. Runtime images are1..8 exact FileIDs, total<=256MiB, distinct paths and
inodes. They are read+SHA checked before any driver call and again after owned
device cleanup. System image roles may be root-owned/writable by root but never
group/world writable; first-party data/gates remain uniquely owned read-only.

CPU gate `rusteda.g1-cuda-cpu-gate.v1` has exact keys `schema`, `status`,
`protocol_sha256`, `source_manifest`, `cuda_image`, `worker`, `manual_review`,
`tiny_review`, `real_reviews`, `word_tolerance`, `outcome_tolerance`.
Status is `passed_fresh_cpu_component_only`, image is null, both tolerances are
integer0, two real review FileIDs are required. All review and worker leaves are
current fingerprinted inputs, each<=2MiB except worker<=128MiB. The gate worker
must be the exact actual runtime ELF: its CPU backend is qualified with image
null before that same executable may select CUDA. The Root-owned
runtime caller must independently join actual fresh CPU process/word/work/raw
status/rolling evidence before admitting this gate; JSON assertions alone do
not establish actual execution. Source manifest and protocol must match the
actual embedded complete ordered registry, so Root47 cannot certify new bytes.

Compiler gate `rusteda.g1-cuda-compiler-gate.v1` has exact keys `schema`, `status`,
`protocol_sha256`, `source_manifest`, `entry`, `device_abi`, `ptx`, `builds`,
`independent_review`. Status `passed_compiler_artifact_only`, two actual build
FileIDs, matching full PTX FileID and the Root-independent source/PTX inspection
are required. Root97's different probe entry and source are ineligible. The
gate is separate from the image manifest and neither gate references this new
manifest: final source -> fresh CPU gate and PTX builds -> image, with no hash
cycle. Compilation is not geometry, native device or performance qualification.

All FileIDs are exact path/byte/SHA objects. Auxiliary gate leaves are bounded
and pre-admitted from stat before fingerprinting. CPU and compiler source
manifest references are byte-identical to the runtime source registry reference.
Unselected optional references, unknown JSON keys, bool-as-integer and extra
compiler tool roles fail. The image loader has no network or source discovery.

The worker additionally requires recorded `CUDA_CACHE_DISABLE=1` before the
first native context. It reads bounded numeric-own-PID maps after context/JIT
and after final synchronization; every mapped CUDA/NVIDIA loader image must be
in the authenticated runtime-image set. Driver/JIT/context state remains in the
1GiB opaque reserve. Full actual touched input/tool/source/image cache, both old
and private replay payloads, is admitted by the external Root-owned3GiB/1GiB
guardian. No eviction credit or production3.5GiB co-residency is implied.

The adapter reserves bounded manifest bootstrap and all fingerprint/read/SHA,
maps, range, transfers, transcript and publication/error tails before large IO.
Every unsuccessful or uncertain device prefix stays held and cannot publish
HEAD. Actual PID/exit/reap/quiet and E2E timing remain external required joins.
Unused compiler rehash, if a Root run requires it, has separately reported time
and resources included in any claimed complete end-to-end GPU measurement.

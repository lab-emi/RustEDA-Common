# Phase B source implementation ledger (unqualified)

All source work is bounded by 64 MiB input text, 2 MiB new text and a 256 MiB
editor/checker model. No compiler or device execution is authorized by this file.
The existing CPU bodies remain the reference. New code below is not an oracle.

One `Budget` remains exclusively borrowed from `PreparedStatic` through range
preparation and `CudaSession`. Each launched prefix retains its entire upper
bound until all outcomes have synchronized and passed index/status/work checks.
An untrustworthy transfer retains that upper bound, poisons the session and
cannot publish a HEAD. A semantic error collects every launched row first.

Named additional logical work uses ceil(bytes/64) blocks. The rolling domain is
`RustEDA.G1.outcome.v1\0`; each 24-byte global-ordinal/work/status/zero record has
one encode unit and one SHA unit. Digest status is lossless device ABI status:
0 success, 1 range, 2 record/index, 3 work, 4 activation, 5 arithmetic. The CPU
outcome seam records the reason at the existing fallible operation without
changing public Error rendering or its work. Local index is separately
validated; driver failures are never manufactured query outcomes. A CUDA raw
range/record/activation/arithmetic error on host-admitted data is a failed query,
not a fallback. No digest or scalar is an exhaustive independent geometry proof.

Complete range preparation charges 8 grid + 8I + 4R + 5Q control units, plus
the separately named raw static reserved-byte scan and per-query 40-byte
canonical encode/SHA pass. Before context creation the range pass has completed
all Q queries and validated each complete expanded span, including out-of-grid
area. The execution pass must reproduce the same query sequence hash.

CUDA holds persistent static bytes and one activation bitset, one 40-byte query
tile and one 16-byte outcome tile, at most 65536 queries. Host upper/outcome
staging and transfer buffers are charged before allocation, within 16 MiB aux;
there is no worker pool, per-worker bitset, candidate vector or Q-length table.
Worst device payload is S<=32 MiB +40*65536 +16*65536 +8*ceil(V/64) <40 MiB.
Padded threads<=65536 reserve 256 bytes each for actual checked local storage.
The device allocation model includes a separate 256 MiB driver reserve and
64 MiB VRAM headroom. This is not the host 3 GiB cgroup admission proof.

Host control for CUDA reuses the original static upper, 5Q scheduling reserve,
and fixed-event reserve. The original CPU worker-bitset reserve is replaced by
exact single-bitmap initialization and final validation fees. Extra range and
outcome validation fees are separately reserved before large reads. Transport
fees include static H2D, activation zero/readback, query H2D, outcome sentinel
H2D and D2H, range hashing, rolling hashing and all retained error-tail work.
The adapter must add these named reserves to its existing bounded disk/source/
report/publication passes; it cannot relabel reservations as completed work.

Future pure tests use <=8 cells/64 shapes/16 queries except a declared single
65537-query segmentation fixture. The latter may use at most 3 MiB query bytes
and 2 MiB outcomes; <=4 test workers plus runtime images must remain below
256 MiB engineering state. Device tests are required and never skip-pass when
the driver/image/gate is missing.

## Recorded decisions

- `src/main.rs`, `src/bin/session.rs`, `tests/worker.rs` and
  `tests/session_worker.rs` are retired and no longer built. Their request types
  moved behind DTOs and ACT02 became ACT03. Evidence for them stays bound to the
  old ELFs; it does not transfer to this source.
- `raw_status` grows `QueryOutcome` by 8 B and `Pending` by 8 B, so CPU aux
  accounting rises by 8 B per tile slot (524,288 B at 65,536). Work, order and
  error text are unchanged. This is a deliberate re-baseline; no default limit
  is crossed.
- There is no fallback inside a session. `cuda_required` fails and the caller
  starts a separate `cpu_ref` job.

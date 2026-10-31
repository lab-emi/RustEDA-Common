# CPU component qualification, 2026-10-31

Source commit: `c4eecd6ee3c3cad2c0557516d3a47ff3dd2cd566`, branch `codex/ibex-g1-cpu-session`,
based on `f0fd551f13869278fdc861bcdc8a875c3f7889ff`. No push, root gitlink,
production dispatch or shared core change is included. The original qualified
prototype worktree at f0fd and a8/7e49 artifacts remain unchanged.

## Exact source and build scope

Executed source: `.work/perf-20261030/g1-cpu-session-source05/SOURCE_MANIFEST.json`,
SHA `86a3889a10188ad145c55b3a730e76f9b3f3e56e3e116e618561802d6ca8a0d2`.
The source commit contains the exact16 trackable crate files in that freeze.
The17th execution record is ignored local `HANDOFF.md`, a nonruntime note kept
in frozen evidence and not force-added to Git. All17 matched before this
separate documentation update. Current documentation FileIDs may differ from
those historical execution IDs; replay uses the immutable frozen inputs.

Qualified build: `.work/perf-20261030/g1-cpu-session-build04/BUILD_QUALIFIED.json`,
SHA `1f67d22bba122c624edfee2c4a67d9db3fb4f8c3004db079ef84931b21a11e85`.
Eight bounded stages passed: fmt, check, test --no-run, full tests, compile-fail
doctest, strict all-target/all-feature clippy, docs and release --bins. There
were35 unit/integration tests, one compile-fail doctest and11 retained new-v2
protocol calls. Three prior strict-lint failures and their source/targets/raw
logs remain preserved. Build04 was delegated attempt1 under Root's immutable
routine-repair policy; no separate per-attempt Root review is claimed.

Each stage used owned2GiB/512MiB/180s/Tasks128, CPUs28–29, jobs1/testthreads2,
incremental0/debug0, FSIZE512MiB and no AS cap. Maximum reaped GNU RSS was
341,212KiB (release); final target apparent/allocated bytes were217,179,967 /
222,199,808 under the1GiB ceiling. These are component build measurements,
not production co-residency admission.

| Worker | Frozen ELF SHA256 | Bytes | Protocol SHA256 |
|---|---|---:|---|
| v2 session | c17aaa847bbcc1458c1a1e125885b13b27414315f6ef234bbc481c26e4f8fb0d | 1288656 | 8be43d34ecde0b664f1a9f1bdae92b53dc8ec5964fde49f2c7d223b115c49673 |
| new v1 | 954dba8fb2f5080e5f9459711da2eec76b90c96eaf2cfd415cc5c6963f9ba5cf | 995504 | 42f39d25cf5bc5749e6f69fc89e789ec434e53ab121123cf0df6682378bdee5b |

Both binaries are under `g1-cpu-session-build04/frozen/bin`. New v1 is not the
original a8/7e49 identity. V2 hashes six named length-prefixed source parts;
v1 hashes the length-prefixed lib/format pair. Documentation changes do not
change either protocol.

## Independent runtime evidence

V2 manual closure: `.work/perf-20261030/g1-cpu-session-root-runtime01/MANUAL_V2_CLOSED.json`,
SHA `f728108c0d4ecb898eb40b86d790930953aa107c6cf8498a0c2cff526fe01c39`.
Independent READY `6abbad1d24000212fb8aef009bc8db3b3de4057c68340e1bde7092d463472489`:
40 base cases and a65,537-query boundary, workers1/2/4, tiles1/257/65536,
129 actual calls. All activation words and successful global-ledger components
were independently recomputed at tolerance0. Actual PID closure and unchanged
inputs were verified. Whole launcher14.111s, reaped RSS22,340KiB; these tiny
correctness timings are not a speedup benchmark.

New v1: `.work/perf-20261030/g1-session-v1-runtime-independent01/READY.json`,
SHA `4b678891ced181a80f9eade9c5010b4724aba3c099e1551fbb9237c54f2b5767`.
40 frozen independent geometries × workers1/2/4 yielded120 source-traced wait
exits, packets, jobs, raw stdout/stderr, HEAD, reports and exact activation/CSR/
ledger checks. All three1GiB/512MiB scopes cleaned up. Per-child PIDs were not
recorded and remain explicitly unavailable/unchecked; no unique-PID claim or
rerun was invented. This is bounded API behavior evidence for the new v1 ELF.

## Retained limits and remaining gates

The session retains32MiB static,512MiB tape,12M queries,65,536 staged queries,
1–4 workers,16MiB auxiliary state,128KiB aggregate output and one100B global
ledger. The budget borrow spans static preparation through consuming finish;
there is no per-window reset/retry. The13 executable/test/Cargo live files remain byte-exact to Source05 after
documentation updates. No dependency was added, and verification/shared.sha256
remains unchanged because shared foundation crates were not modified.

Actual Road v2 producer/origin, real-revision static/tape counts and large
whole-session performance remain unchecked. The original3,758,096,384B engine
cap and late headroom rev4=4,265,492B / rev56=5,893,652B are not production
integration admission. GPU architecture and physical/full-flow acceptance are
unselected. No default changes, publication or speedup claim follows from this
CPU component qualification.

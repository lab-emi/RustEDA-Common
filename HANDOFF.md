# Shared foundation handoff

## Initial extraction, 2026-10-25

- This is the private, canonical implementation source for four shared library crates and the
  shared CLI. Consumers select exact Git revisions.
- Core, workspace, formats and compute implementation bytes are preserved from the existing
  byte-matched RustSys/RustSTA foundation. Do not change algorithms during extraction.
- rusteda-cli-common adds a per-call version argument so product adapters retain their original
  --version output and workspace-history metadata. No mutable global version context is used.
- Synthetic fixture files and specifications required by existing shared tests are retained.
- Build/test work is serialized by the superproject scale lock with two build jobs and two test
  threads, an 8 GiB memory ceiling and explicit timeout; expected individual tests stay below
  2 GiB. See the parent phase-2 handoff for the upfront estimate.
- Do not treat this source relocation, compilation or unit tests as fresh EDA signoff evidence.

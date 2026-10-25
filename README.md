# RustEDA Common

The single implementation source for the shared RustEDA foundation:

- rusteda-core: typed arenas, symbols, diagnostics, library and netlist models.
- rusteda-workspace: disk revisions, binary tables, locks and CLI envelopes.
- rusteda-formats: Verilog, Liberty, SDC, SPEF, BLIF and AIGER readers/writers.
- rusteda-compute: deterministic CPU kernels and optional native CUDA support.
- rusteda-cli-common: the shared workspace CLI implementation.

Product repositories depend on an exact Git revision of this private repository. A small
product-owned rusteda-cli adapter preserves its binary name and product version; implementation
and core tests live here. Standalone consumers require authorized Git access.

The initial implementation comes from the byte-matched RustSys/RustSTA foundation at
d36d922c0201968745be72bd71aff57ace393c90 / 455c43c9bd6afa0e0a538822d3fc2822d6e26880.
This extraction changes dependency ownership, not EDA algorithms or persisted formats.

Use the RustEDA superproject's bounded development commands. Direct Cargo commands also work:

~~~sh
cargo test --workspace --locked
cargo test -p rusteda-compute --features cuda
~~~

Optional GPU and external-oracle checks are not implied by CPU test success.
The core crates have no Python or GUI dependency. See AGENTS.md and HANDOFF.md before changes.

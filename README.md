# Experimental G1 CPU session snapshot

This branch preserves the CPU session development independently of the later
CPU/CUDA refactor. It adds the original bounded integer crossing-session source
to the current production foundation. Existing component receipts keep their
original ELF identities; this composite source publication is not a new runtime
qualification or a production Road integration. GPU execution and whole-flow
acceleration remain unchecked here.

[Later CPU/CUDA session](https://github.com/lab-emi/RustEDA-Common/tree/codex/g1-crossing-session-20261101) ·
[Production foundation](https://github.com/lab-emi/RustEDA-Common)

---

<p align="center"><img src="docs/assets/rusteda-common-wordmark.svg" alt="RustEDA Common" width="360"></p>

<h1 align="center">The foundation beneath the design loop.</h1>

<p align="center"><a href="https://lab-emi.github.io/RustEDA/modules/rusteda-common/">Documentation</a> · <a href="https://github.com/lab-emi/RustEDA">RustEDA</a> · <a href="LICENSE">Apache-2.0</a></p>

<picture>
<source media="(prefers-reduced-motion: reduce)" srcset="docs/assets/rusteda-common-banner.svg">
<img src="docs/assets/rusteda-common-banner.gif" alt="Shared models, formats, disk workspaces and compute for RustEDA" width="1200">
</picture>

The single implementation source for the shared RustEDA foundation:

- rusteda-core: typed arenas, symbols, diagnostics, library and netlist models.
- rusteda-workspace: disk revisions, binary tables, locks and CLI envelopes.
- rusteda-formats: Verilog, Liberty, SDC, SPEF, BLIF and AIGER readers/writers.
- rusteda-compute: deterministic CPU kernels and optional native CUDA support.
- rusteda-cli-common: the shared workspace CLI implementation.

Product repositories depend on an exact Git revision of this repository. A small
product-owned rusteda-cli adapter preserves its binary name and product version; implementation
and core tests live here. Sources are available over HTTPS.

The initial implementation comes from the byte-matched RustSys/RustSTA foundation at
d36d922c0201968745be72bd71aff57ace393c90 / 455c43c9bd6afa0e0a538822d3fc2822d6e26880.
This extraction changes dependency ownership, not EDA algorithms or persisted formats.

Use the RustEDA superproject's bounded development commands. Direct Cargo commands also work:

~~~sh
cargo test --workspace --locked
cargo test -p rusteda-compute --features cuda
~~~

Optional GPU and external-oracle checks are not implied by CPU test success.
The core crates have no Python or GUI dependency. See the [development guide](https://lab-emi.github.io/RustEDA/development/).


## Latest development · 2026-11-01

The foundation now includes the IHP/SRAM interface work used by the qualified
Ibex flow: Liberty memory buses and pin metadata, explicit driving-cell
constraints, escaped mapped-netlist identifiers, and bounded connectivity
exports. The deterministic CPU RC charge-fitting implementation uses `f64`.

[Current source and qualification](docs/development-status.md) ·
[Latest complete-flow evidence](https://lab-emi.github.io/RustEDA/benchmarks/)

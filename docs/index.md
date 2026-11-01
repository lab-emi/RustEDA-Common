# Experimental G1 crossing session

This branch publishes the current CPU/CUDA crossing-query source for review.
It is based on the current shared foundation and adds a bounded integer geometry
session, explicit work/IO budgets, streamed v3 transport, raw device status and
separate CUDA/PTX image qualification. It is not selected by the production
RustEDA flow or Road extractor.

Recorded geometry captures report zero activation-bit differences between the
CPU reference and GPU. This component comparison does not establish full
worker/runtime-image qualification, production integration or a whole-flow GPU
speedup. The source checks for this publication are recorded separately; missing
checks remain unchecked. Preserve the strict CPU fallback and per-query diagnostics.

[Current production foundation](https://github.com/lab-emi/RustEDA-Common) ·
[Whole-flow scope](https://lab-emi.github.io/RustEDA/benchmarks/)

---

# RustEDA Common

Shared typed models, file formats, disk workspaces, CLI contracts and deterministic compute kernels.

[All RustEDA modules](https://lab-emi.github.io/RustEDA/)

<picture>
<source media="(prefers-reduced-motion: reduce)" srcset="assets/rusteda-common-banner.svg">
<img src="assets/rusteda-common-banner.gif" alt="Shared typed models, file formats, disk workspaces, CLI contracts and deterministic compute kernels." width="1200">
</picture>

- [Core models](specs/core/README.md)
- [Workspace](specs/workspace/README.md)
- [Compute](specs/compute/README.md)
- [Verilog](specs/verilog/verilog.md)
- [Liberty](specs/liberty/liberty.md)
- [SDC](specs/sdc/sdc.md)


## Development update · 2026-11-01

The foundation now includes the IHP/SRAM interface work used by the qualified
Ibex flow: Liberty memory buses and pin metadata, explicit driving-cell
constraints, escaped mapped-netlist identifiers, and bounded connectivity
exports. The deterministic CPU RC charge-fitting implementation uses `f64`.

[Source, evidence and remaining limits](development-status.md)

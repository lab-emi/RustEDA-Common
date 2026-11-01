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

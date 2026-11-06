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


## RustEDA Common at a glance

Ibex RTL-to-GDS on IHP SG13G2 with EZ130 cells and two SRAM macros, 2026-11-05: RustEDA and LibreLane 3.0.17 (OpenROAD, Yosys, KLayout, Magic, Netgen) ran one after the other on the same 4-vCPU host from the same prepared inputs, and the same signoff scripts checked both layouts.

- The formats, workspace and RC kernels in this repository sit under every RustEDA result: the flow finishes **4.2× sooner end to end** than LibreLane with a **5.2 % smaller, 7.4 % shorter-wired** layout.
- RustSTA, built on its SPEF, Liberty and workspace layers, runs **1.6×–2.0× faster than OpenSTA** with less peak memory; RustSys, on its netlist and Liberty models, synthesizes **35.4× faster than Yosys + ABC**.

LibreLane's layout fails the shared timing gate (112 maximum-capacitance and fanout violations at TT and SS) and its equivalence check does not complete, so the receipt keeps its own ratio fields null under its publication rule; the multiples here divide the recorded times. Stages are paired by phase, not as identical tasks. RustEDA is not faster everywhere: its repair before global routing takes 1.18× LibreLane's two pre-route resizer steps alone, OpenRCX runs 1.36× longer on RustEDA's DEF, and the latch-up check 1.11× longer on its layout. [All stages, gates and losses](https://lab-emi.github.io/RustEDA/benchmarks/)

## Development update · 2026-11-01

The foundation now includes the IHP/SRAM interface work used by the qualified
Ibex flow: Liberty memory buses and pin metadata, explicit driving-cell
constraints, escaped mapped-netlist identifiers, and bounded connectivity
exports. The deterministic CPU RC charge-fitting implementation uses `f64`.

[Source, evidence and remaining limits](development-status.md)

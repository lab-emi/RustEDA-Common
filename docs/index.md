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

Ibex RTL-to-GDS on IHP SG13G2 with EZ130 cells and two SRAM macros, 2026-11-13: RustEDA and LibreLane 3.0.17 (OpenROAD, Yosys, KLayout, Magic, Netgen) ran one after the other on the same 4-vCPU host from the same prepared inputs, and the same signoff scripts checked both layouts. Every RustEDA engine was built with Rust 1.100.0; RustSys and RustSTA were built against this repository at [`002f8f6`](https://github.com/lab-emi/RustEDA-Common/commit/002f8f69f6adcec4c75973dbdccb42f04cdd7a37).

- This repository has [no head-to-head of its own](https://lab-emi.github.io/RustEDA/benchmarks/#rusteda-common-2026-11-13): it is compiled into RustSys and RustSTA, including the `rusteda` workspace CLI used by the producer's frontend and timing steps and by the shared timing gate. In the flow, RustSys synthesis is **40.3× faster** than LibreLane's (3.10 s against 124.93 s), repair before detailed routing **3.5× faster** (13.85 s against 48.36 s) and static timing, RustSTA with its OpenSTA parity check, **5.2× faster** (26.96 s against 138.93 s).
- The whole flow finishes **4.4× sooner end to end** (751.54 s against 3,283.05 s with both flows' foundry checks on the same schedule; 3.5× with every check run one after another) with a **5.2 % smaller, 7.7 % shorter-wired** layout, 1.3 % fewer vias, zero timing violations where LibreLane's has 112, and synthesis and final equivalence proven.

LibreLane's layout fails the shared timing gate (112 maximum-capacitance and fanout violations at TT and SS) and its equivalence check does not complete, so the receipt keeps its own ratio fields null under its publication rule; the multiples here divide the recorded times. Stages are paired by phase, not as identical tasks. Against the three losses of 2026-11-05, in same-load runs: repair before global routing now takes 0.73× the time of LibreLane's two pre-route resizer steps (ranges do not overlap; 1.18× on 2026-11-05), OpenRCX on RustEDA's layout is level with its time on LibreLane's (1.00×, ranges overlap; 1.36× on 2026-11-05) and the latch-up check on RustEDA's layout takes 0.76× its time on LibreLane's (1.11× with the checker of that campaign). RustEDA is still not faster or not better everywhere: RustROAD's standalone extraction takes 1.22× OpenRCX's time at 4 threads, RustSTA's peak memory at 4 threads is 11.8 % above OpenSTA's, the shared timing gate, which uses this repository's `rusteda` CLI, is level (0.98×, ranges overlap) and its layout keeps less hold margin (TT 0.108 ns against 0.172 ns; both meet hold). [All stages, gates and losses](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-13) · [Where RustEDA still loses or is at parity](https://lab-emi.github.io/RustEDA/benchmarks/#where-rusteda-still-loses-or-is-at-parity-2026-11-13)

## PULPissimo benchmark scope

The [PULPissimo benchmark](https://lab-emi.github.io/RustEDA/benchmarks/pulpissimo/) is synthesis-only and exercises the shared foundation only through RustSys. It compares the PULPissimo Ibex core with a bounded Yosys 0.70/ABC reference on SKY130 HD; it adds no Common-specific speed or memory claim, and the full PULPissimo SoC still exceeds a 2 h synthesis budget on SKY130 with behavioral L2 memory; an IHP SG13G2 / EZ130 synthesis-only test with 80 official SRAM macros completes in 426 s, but neither is a physical signoff result.

## Development update · 2026-11-01

The foundation now includes the IHP/SRAM interface work used by the qualified
Ibex flow: Liberty memory buses and pin metadata, explicit driving-cell
constraints, escaped mapped-netlist identifiers, and bounded connectivity
exports. The deterministic CPU RC charge-fitting implementation uses `f64`.

[Source, evidence and remaining limits](development-status.md)

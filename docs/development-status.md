# Development status

Updated **2026-11-01**. This page describes the source published with the
RustEDA development update. Use the superproject’s exact gitlinks and the
repository lockfile when reproducing a run.

## Shared foundation and IHP interfaces

The foundation now includes the IHP/SRAM interface work used by the qualified
Ibex flow: Liberty memory buses and pin metadata, explicit driving-cell
constraints, escaped mapped-netlist identifiers, and bounded connectivity
exports. The deterministic CPU RC charge-fitting implementation uses `f64`.

The five shared crates remain independent of engines, Python and GUI packages.
Source manifests record the shared files; downstream repositories pin one exact
Common commit rather than copying implementations.

The G1 crossing-query CPU/CUDA session is a separate development branch. Device
comparisons on captured geometry do not qualify a GPU-enabled RTL-to-GDS flow;
production Road integration and its runtime-image gate remain unchecked.

[Whole-flow evidence and measurement scope](https://lab-emi.github.io/RustEDA/benchmarks/) ·
[RustEDA engine status](https://lab-emi.github.io/RustEDA/engine-status/)

## Publication checks

The isolated publication checkout passes 418 regular Rust tests (11 ignored),
format checks and all-target/all-feature Clippy. The ignored external/GPU/scale
checks are not counted as passes. These source checks do not create a new
complete-flow measurement.

## Experimental G1 publication checks

The isolated G1 snapshot passes CPU workspace type checking, formatting and
39 compute-library tests (2 ignored). This publication does not run GPU tests,
rebuild PTX or qualify runtime images. It adds the experimental integer crossing
session to the current production foundation; Road continues to use its existing
extraction path. Recorded capture comparisons are scoped component evidence.

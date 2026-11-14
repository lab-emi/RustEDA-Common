# Development status

Updated **2026-11-14**. This page describes the source published with the
RustEDA development update. Use the superproject’s exact gitlinks and the
repository lockfile when reproducing a run.

## Since 2026-11-01

The foundation now carries what the Ibex host campaigns of 2026-11-05 and
2026-11-13 ran on:

- **Import.** `rusteda import` parses each distinct SPEF source once and shares
  its text between corners, checks a source given again against its earlier
  text while reading it, and keeps the nodes of a net and the name map in a few
  large buffers; a count-only parse checks a source without keeping its nets.
  `--lib-cache DIR` shares compiled Liberty libraries between workspaces as
  copies bound to the recorded compile, never links (RCLI-REQ-0025), and the
  streaming netlist reader connects the bit pins of a bus.
- **RC.** The root and sink crossing bisections are replayed with a certified
  bound, and the crossings share their `expm1` and pole-ramp factors.
- **Command line.** `rusteda estimate-spef` is the native form of the Ibex
  recipe's placement RC estimate (RCLI-REQ-0026); `rusteda eco-round` plans and
  applies one ECO round of the native frontend (RCLI-REQ-0027); and
  `frontend-profiles`, `frontend-physical`, `frontend-pins` and
  `frontend-validate` are its other helpers (RCLI-REQ-0028). Each writes the
  reference helper's files byte for byte from the same inputs, checked against
  the helpers on a small run.
- **Toolchain.** Rust 1.100.0; fixed-size records are read with `as_chunks`.

[Ibex host campaign 2026-11-13](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-13) ·
[PULPissimo benchmark scope](https://lab-emi.github.io/RustEDA/benchmarks/pulpissimo/) (synthesis-only; no Common-specific claim)

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

On 2026-11-14 the checkout passes 445 regular Rust tests (11 ignored) in a
release build, format checks and all-target/all-feature Clippy with warnings
denied (418 regular tests on 2026-11-01). The ignored external/GPU/scale
checks are not counted as passes. These source checks do not create a new
complete-flow measurement.

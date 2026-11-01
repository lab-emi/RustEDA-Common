#!/usr/bin/env python3
"""Bind a built D0 PTX to bounded sources, CPU gate and read-only tools.

Peak scratch <8MiB; <=32 code files x1MiB, <=16 tools x1GiB, streamed hashes.
Does not compile kernels or create a CUDA context. Fresh output required.
"""
import argparse
import hashlib
import json
from pathlib import Path


def identity(path, cap=1 << 20):
    path = path.resolve(strict=True)
    before = path.stat()
    if not path.is_file() or before.st_size > cap:
        raise ValueError('manifest byte/type budget')
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(65536), b''):
            digest.update(block)
    after = path.stat()
    if (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError('input changed while hashing')
    return dict(path=str(path), sha256=digest.hexdigest(), bytes=after.st_size)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, required=True)
    parser.add_argument('--base-rev', choices=['empty'], required=True)
    parser.add_argument('--cpu-gate', type=Path, required=True)
    parser.add_argument('--ptx', type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    common = source.parents[1]
    gate = json.loads(args.cpu_gate.read_text())
    if gate['status'] != 'passed' or gate['tests'] != 4:
        raise ValueError('independent CPU gate is not passed')
    paths = [source / name for name in ('Cargo.toml', 'Cargo.lock', 'src/lib.rs', 'src/cuda.rs',
        'tests/exact.rs', 'tests/gpu.rs', 'tests/fixture/mod.rs', 'examples/compare.rs',
        'gpu/Cargo.toml', 'gpu/Cargo.lock', 'gpu/rust-toolchain.toml', 'gpu/src/main.rs', 'build_manifest.py')]
    predicate = common / 'crates/core/rusteda-compute/src/geometry/kernels/rect_pairs.rs'
    current_predicate = identity(predicate)
    if current_predicate not in gate['source']:
        raise ValueError('shared kernel differs from the passed CPU gate')
    paths.append(predicate)
    root = Path('/home/cgao/git/RustEDA')
    tools = [root / '.work/gpu/cuda-rust/cuda-oxide/target/debug/cargo-oxide',
        Path('/home/cgao/.cargo/cuda-oxide/librustc_codegen_cuda.so'), Path('/usr/bin/llc'),
        Path('/home/cgao/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc'),
        root / '.work/gpu/cuda-13.4/bin/nvcc', root / '.work/gpu/cuda-13.4/nvvm/libdevice/libdevice.10.bc',
        root / '.work/gpu/cuda-13.4/include/cuda.h', root / '.work/gpu/cuda-13.4/include/curand.h']
    output = args.workspace.resolve()
    if output.exists() or output == source or source in output.parents:
        raise ValueError('REVISION_CONFLICT: fresh external artifact directory required')
    output.mkdir(parents=True)
    result = dict(schema='rusteda.rc-geometry-d0-ptx.v1', status='built_unqualified', target='sm_80',
        cpu_gate_status='passed', cpu_gate=identity(args.cpu_gate),
        sources=[identity(path) for path in paths], tools=[identity(path, 1 << 30) for path in tools],
        ptx=identity(args.ptx, 8 << 20), integer_tolerance=0,
        cuda_rust_revision='a4a939a9b5f01f3bf712e92386922e1bc7c434a9',
        build_options=['--no-fmad', '--arch', 'sm_80'], gpu_status='unverified_before_required_differential')
    (output / 'manifest.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(identity(output / 'manifest.json')))


if __name__ == '__main__':
    main()

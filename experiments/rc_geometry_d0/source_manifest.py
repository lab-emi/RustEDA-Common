#!/usr/bin/env python3
"""Write a fresh source-preparation manifest; never build or touch a GPU.

Bound: <=32 files x1MiB, hashed with 64KiB scratch; metadata only, <32MiB RSS.
CPU/PTX build and device artifact hashes remain explicitly unchecked here.
"""
import argparse
import hashlib
import json
from pathlib import Path


def identity(path):
    path = path.resolve(strict=True)
    before = path.stat()
    assert path.is_file() and before.st_size <= 1 << 20
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(65536), b''):
            digest.update(block)
    after = path.stat()
    assert (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) == (
        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns)
    return dict(path=str(path), sha256=digest.hexdigest(), bytes=after.st_size)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, required=True)
    parser.add_argument('--base-rev', choices=['empty'], required=True)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    paths = sorted(p for p in source.rglob('*') if p.is_file()
                   and not any(part in ('target', '__pycache__') for part in p.relative_to(source).parts))
    assert len(paths) <= 32
    records = [identity(path) for path in paths]
    destination = args.workspace.resolve()
    if destination.exists() or destination == source or source in destination.parents:
        raise ValueError('REVISION_CONFLICT: source-preparation manifest needs a fresh external directory')
    destination.mkdir(parents=True)
    result = dict(schema='rusteda.rc-geometry-d0-source-preparation.v1',
        status='prepared_unverified', foundation_base='778fa8dfd52b52aa978743445c32174fa777880b',
        sources=records, cpu_tests='unchecked_timing_isolation', ptx_build='unchecked_timing_isolation',
        ptx_artifact=None, gpu_differential='unchecked_timing_isolation',
        toolchain_checkout='a4a939a9b5f01f3bf712e92386922e1bc7c434a9',
        scope='Source-only D0; no CPU correctness, GPU, extraction, signoff or speedup claim')
    (destination / 'manifest.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(identity(destination / 'manifest.json')))


if __name__ == '__main__':
    main()

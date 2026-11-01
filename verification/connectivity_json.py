#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 RustEDA contributors
"""Bounded black-box pin/net isomorphism check for native connectivity JSON.

Case JSON: {schema:rusteda.connectivity-case.v1, top, workspace, revision,
netlist:{path,sha256}, libraries:[{path,sha256}], cell_buses:{path,sha256}?}.
The case workspace is read-only. --workspace is a fresh evidence directory.
No reference source code is accessed; original Liberty/Verilog are passed to the
pinned external executable. Run under a <=2GiB cgroup plus wall-clock guard.
"""
import argparse
import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import resource
import re
import signal
import subprocess
import sys

MAX_BYTES = 64 << 20


def require(ok, text):
    if not ok:
        raise ValueError(text)


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def load(path, limit=MAX_BYTES):
    require(path.stat().st_size <= limit, 'input byte budget')
    return json.loads(path.read_text())


def word(path):
    value = str(path)
    require(not any(c in value for c in '\r\n\0'), 'unsupported reference path')
    return '"'+value.replace('\\', '\\\\').replace('"', '\\"')+'"'


def labels(name, definition, values=None):
    width = len(definition['bits'])
    bits = definition['bits'] if values is None else values
    require(len(bits) in (0, width), 'port width mismatch')
    if not bits:
        bits = [None]*width
    offset, upto = definition.get('offset', 0), definition.get('upto', 0)
    for i, bit in enumerate(bits):
        index = offset+(width-1-i if upto else i)
        # Yosys JSON does not always retain a [0:0] versus scalar flag.
        label = name if width == 1 and offset == 0 else f'{name}[{index}]'
        yield label, None if bit == 'x' else bit


def observations(document, top):
    modules = document['modules']; module = modules[top]
    require(len(module['cells']) <= 100000 and len(module['netnames']) <= 500000, 'graph entity budget')
    observed, masters, count = {}, {}, 0
    def add(key, bit, direction):
        nonlocal count
        count += 1
        require(count <= 1500000, 'observation work budget')
        require(key not in observed, 'duplicate graph observation')
        observed[key] = (bit, direction)
    for instance, cell in module['cells'].items():
        masters[instance] = cell['type']
        for port, definition in modules[cell['type']]['ports'].items():
            bits = cell['connections'].get(port, [])
            direction = cell['port_directions'].get(port, definition['direction'])
            require(direction == definition['direction'], 'instance/interface direction mismatch')
            for name, bit in labels(port, definition, bits):
                add(('pin', instance, name), bit, direction)
    for port, definition in module['ports'].items():
        for name, bit in labels(port, definition):
            add(('port', name), bit, definition['direction'])
    for net, definition in module['netnames'].items():
        for name, bit in labels(net, definition):
            add(('net', name), bit, None)
    return masters, observed


def compare(native, reference, top):
    nm, left = observations(native, top); rm, right = observations(reference, top)
    require(nm == rm, 'instance/master sets differ')
    require(left.keys() == right.keys(), 'named pin/port/net observation sets differ: '+str(list(left.keys()-right.keys())[:4])+' / '+str(list(right.keys()-left.keys())[:4]))
    forward, backward = {}, {}
    for key, (a, adir) in left.items():
        b, bdir = right[key]
        require(adir == bdir, 'direction mismatch: '+str(key))
        if isinstance(a, int) and isinstance(b, int):
            require(forward.setdefault(a, b) == b and backward.setdefault(b, a) == a, 'bit alias/connectivity mismatch: '+str(key))
        else:
            require(a == b, 'constant/unconnected mismatch: '+str(key))
    return dict(instances=len(nm), observations=len(left), physical_net_components=len(forward))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--workspace', type=Path, required=True); p.add_argument('--base-rev', required=True)
    p.add_argument('--case', type=Path, required=True)
    for name in ['rusteda', 'yosys']:
        p.add_argument('--'+name, type=Path, required=True); p.add_argument('--'+name+'-sha256', required=True)
    args = p.parse_args()
    resource.setrlimit(resource.RLIMIT_AS, (2 << 30, 2 << 30)); signal.alarm(300)
    out = args.workspace.resolve(); out.mkdir(parents=True, exist_ok=True)
    with (out/'workspace.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(args.base_rev == 'empty' and not any(p.name != 'workspace.lock' for p in out.iterdir()), 'REVISION_CONFLICT')
        case = load(args.case, 1 << 20); require(case['schema'] == 'rusteda.connectivity-case.v1', 'case schema')
        require(re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', case['top']) is not None, 'unsupported top identifier')
        inputs = {str(args.case.resolve()): sha(args.case)}
        def record(r, cap=MAX_BYTES):
            path = (args.case.resolve().parent/r['path']).resolve()
            require(path.is_file() and path.stat().st_size <= cap and sha(path) == r['sha256'], 'input binding: '+str(path))
            inputs[str(path)] = r['sha256']; return path
        net = record(case['netlist']); require(len(case['libraries']) <= 16, 'library count budget')
        libs = [record(r) for r in case['libraries']]
        buses = record(case['cell_buses'], 2 << 20) if 'cell_buses' in case else None
        tools = {}
        for name in ['rusteda', 'yosys']:
            path = getattr(args, name).resolve()
            if not path.is_file():
                (out/'result.json').write_text(json.dumps({'status':'unverified', 'missing_tool':name})+'\n'); return 3
            require(sha(path) == getattr(args, name+'_sha256'), 'tool hash mismatch')
            require(path.open('rb').read(4) == b'\x7fELF', 'direct native executable required')
            tools[name] = dict(path=str(path), sha256=sha(path))
        def command(name, argv):
            (out/(name+'-command.json')).write_text(json.dumps(argv, indent=2)+'\n')
            with (out/(name+'.stdout')).open('wb') as f, (out/(name+'.stderr')).open('wb') as e:
                proc = subprocess.Popen(argv, stdout=f, stderr=e, start_new_session=True, env={**os.environ, 'RAYON_NUM_THREADS':'2'})
                try:
                    code = proc.wait(timeout=120)
                except BaseException:
                    os.killpg(proc.pid, signal.SIGKILL); proc.wait(); raise
            require(code == 0, name+' failed: '+str(code))
        argv = [str(args.rusteda), '--format', 'json', 'export', '-w', case['workspace'], '--rev', case['revision'], '--kind', 'connectivity-json', '--netlist', str(out/'native.json')]
        if buses: argv += ['--cell-buses', str(buses)]
        command('native', argv)
        script = '\n'.join([*[f'read_liberty -lib {word(lib)}' for lib in libs], f'read_verilog {word(net)}', 'hierarchy -top '+case['top'], 'write_json '+word(out/'reference.json')])+'\n'
        (out/'reference.ys').write_text(script)
        command('reference', [str(args.yosys), '-Q', '-T', '-s', str(out/'reference.ys')])
        native, reference = load(out/'native.json'), load(out/'reference.json')
        metrics = compare(native, reference, case['top'])
        negatives = {}
        mutant = copy.deepcopy(reference); top = mutant['modules'][case['top']]
        bits = [b for cell in top['cells'].values() for conn in cell['connections'].values() for b in conn if isinstance(b, int)]
        require(len(set(bits)) > 1, 'nontrivial two-net negative control required')
        first = bits[0]; other = next(b for b in bits if b != first)
        for cell in top['cells'].values():
            hit = False
            for conn in cell['connections'].values():
                if first in conn: conn[conn.index(first)] = other; hit = True; break
            if hit: break
        try: compare(native, mutant, case['top'])
        except ValueError: negatives['rewired_pin'] = 'rejected'
        require(negatives.get('rewired_pin') == 'rejected', 'rewiring control was accepted')
        mutant = copy.deepcopy(reference); top = mutant['modules'][case['top']]
        port = next(iter(top['ports'])); top['ports'][port]['direction'] = 'output' if top['ports'][port]['direction'] == 'input' else 'input'
        try: compare(native, mutant, case['top'])
        except ValueError: negatives['wrong_direction'] = 'rejected'
        require(negatives.get('wrong_direction') == 'rejected', 'direction control was accepted')
        mutant = copy.deepcopy(reference); top = mutant['modules'][case['top']]
        vectors = [p['bits'] for p in top['ports'].values()]
        vectors += [bits for cell in top['cells'].values() for bits in cell['connections'].values()]
        changed = False
        for bits in vectors:
            if len(bits) > 1 and len(set(bits)) > 1:
                other = next(i for i in range(1, len(bits)) if bits[i] != bits[0])
                bits[0], bits[other] = bits[other], bits[0]; changed = True; break
        require(changed, 'nontrivial bus-order control required')
        try: compare(native, mutant, case['top'])
        except ValueError: negatives['bus_order'] = 'rejected'
        require(negatives.get('bus_order') == 'rejected', 'bus-order control was accepted')
        require(all(sha(path) == h for path, h in inputs.items()), 'original inputs changed')
        result = dict(status='passed', harness_sha256=sha(Path(__file__)), scope='named scalar pin, port and net-label isomorphism; no functional/physical/timing claim', tools=tools, inputs=inputs, metrics=metrics, negatives=negatives, native_receipt=load(out/'native.stdout', 1 << 20))
        (out/'result.json').write_text(json.dumps(result, indent=2)+'\n'); (out/'HEAD').write_text('rev1\n')
        print(json.dumps(metrics, sort_keys=True)); return 0


if __name__ == '__main__':
    try: raise SystemExit(main())
    except (ValueError, OSError, KeyError) as error:
        print(str(error), file=sys.stderr); raise SystemExit(2)

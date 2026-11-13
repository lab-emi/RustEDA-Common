#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 RustEDA contributors
"""Write the fixture of tests/frontend_flow.rs and its expected files with the reference helpers.

The fixture is a small, self-consistent frontend run of a design `mini` (a clock gate, eight
flip-flops, a net of fanout eight, four input ports and one output port, one SRAM macro beside
them): a synthetic two-corner Liberty and LEF, an SDC, recipe policies, a connectivity graph,
pin tables, timing reports of an ideal-wire and of an estimated-RC timing tag, a placed DEF, a
placement seed and an RC estimate. `expected/` holds what the reference helpers of the RustEDA
Ibex recipe write from it:

  native_frontend_inputs.py profiles | physical | pins | validate
  native_frontend_inputs.py bundle, then native_ecos.py --plan-only   (fanout, hold)

usage: reference.py --recipe <RustEDA>/integration/ibex_sg13g2_ez130
                    --road-scripts <RustROAD>/scripts --xxhsum <xxhsum>

The recipe's `recipe/*.py` files here are shims: the context of a run pins them by hash, and
only this script runs them (they load the real helpers named by the options above). Paths and
the digests that depend on them are written as placeholders, replaced by the test:
@FIX@ (this directory), @RUN@ (the run root), @XXHSUM@, @SHA:<label>@ and @BYTES:<label>@ of a
file whose bytes hold such a path (see VOLATILE), and the receipt's run time, peak memory and
planner hash.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

FIX = Path(__file__).resolve().parent
CORNERS = ('tt', 'ss')

# ---------------------------------------------------------------------------------------------
# The PDK: five profiled cells in Liberty, every cell in LEF and in the pin tables.

BUFFERS = {'BUFX1': (1.68, 0.0015, 0.10), 'BUFX2': (2.10, 0.0020, 0.20), 'BUFX4': (2.94, 0.0040, 0.40),
           'DLY1X2': (3.36, 0.0018, 0.15)}


def table(kind, base, corner):
    scale = 1.0 if corner == 'tt' else 1.4
    rows = []
    for i in range(3):
        rows.append(', '.join(f'{(base + 0.07 * i + 0.9 * j * (0.6 if "transition" in kind else 0.4)) * scale:.6f}'
                              for j in (0.001, 0.01, 0.1)))
    body = ', \\\n            '.join(f'"{r}"' for r in rows)
    return f'        {kind} (T3) {{\n          values ({body}) ;\n        }}\n'


def liberty(corner):
    out = [f'library (mini_{corner}) {{\n  time_unit : "1ns" ;\n  capacitive_load_unit (1, pf) ;\n'
           '  lu_table_template (T3) {\n    variable_1 : input_net_transition ;\n'
           '    variable_2 : total_output_net_capacitance ;\n    index_1 ("0.05, 0.2, 0.8") ;\n'
           '    index_2 ("0.001, 0.01, 0.1") ;\n  }\n']
    for name, (width, cap, limit) in BUFFERS.items():
        base = 0.30 if name.startswith('DLY') else 0.08
        cap = cap if corner == 'tt' else cap * 0.95
        out.append(f'  cell ({name}) {{\n    area : {width * 3.36:.4f} ;\n'
                   f'    pin (A) {{\n      direction : input ;\n      capacitance : {cap:.6f} ;\n    }}\n'
                   f'    pin (Y) {{\n      direction : output ;\n      function : "A" ;\n'
                   f'      max_capacitance : {limit} ;\n      timing () {{\n        related_pin : "A" ;\n'
                   '        timing_sense : positive_unate ;\n'
                   + table('cell_rise', base, corner) + table('cell_fall', base * 0.95, corner)
                   + table('rise_transition', 0.07, corner) + table('fall_transition', 0.06, corner)
                   + '      }\n    }\n  }\n')
    out.append('  cell (ANTENNA2) {\n    area : 2.8224 ;\n    pin (A) {\n      direction : input ;\n'
               '      capacitance : 0.001 ;\n    }\n  }\n}\n')
    return ''.join(out)


CELLS = {  # master: (width um, pins {name: (direction, clock)})
    'BUFX1': (1.68, {'A': 'I', 'Y': 'O'}), 'BUFX2': (2.10, {'A': 'I', 'Y': 'O'}),
    'BUFX4': (2.94, {'A': 'I', 'Y': 'O'}), 'DLY1X2': (3.36, {'A': 'I', 'Y': 'O'}),
    'INVX1': (1.26, {'A': 'I', 'Y': 'O'}), 'NAND2X1': (1.68, {'A': 'I', 'B': 'I', 'Y': 'O'}),
    'AND2X1': (2.10, {'A': 'I', 'B': 'I', 'Y': 'O'}),
    'DFFX1': (6.30, {'CK': 'I', 'D': 'I', 'RB': 'I', 'Q': 'O'}),
}


def lef():
    out = ['VERSION 5.8 ;\nBUSBITCHARS "[]" ;\nDIVIDERCHAR "/" ;\n\n']
    for name, (width, pins) in CELLS.items():
        out.append(f'MACRO {name}\n  CLASS CORE ;\n  ORIGIN 0 0 ;\n  SIZE {width} BY 3.36 ;\n  SITE CoreSite ;\n')
        for k, (pin, way) in enumerate(pins.items()):
            x = 0.21 + k * (width - 0.42) / max(len(pins) - 1, 1)
            area = ('    ANTENNAMODEL OXIDE1 ;\n      ANTENNAGATEAREA 0.1014 LAYER Metal1 ;\n' if way == 'I'
                    else '    ANTENNADIFFAREA 0.2652 LAYER Metal1 ;\n')
            out.append(f'  PIN {pin}\n    DIRECTION {"INPUT" if way == "I" else "OUTPUT"} ;\n    USE SIGNAL ;\n{area}'
                       f'    PORT\n      LAYER Metal1 ;\n        RECT {x - 0.08:.3f} 1.2 {x + 0.08:.3f} 1.8 ;\n    END\n  END {pin}\n')
        for rail, use in (('VDD', 'POWER'), ('VSS', 'GROUND')):
            y = 3.2 if rail == 'VDD' else -0.16
            out.append(f'  PIN {rail}\n    DIRECTION INOUT ;\n    USE {use} ;\n    PORT\n      LAYER Metal1 ;\n'
                       f'        RECT 0 {y} {width} {y + 0.32:.2f} ;\n    END\n  END {rail}\n')
        out.append(f'END {name}\n\n')
    out.append('MACRO ANTENNA2\n  CLASS CORE ANTENNACELL ;\n  ORIGIN 0 0 ;\n  SIZE 0.84 BY 3.36 ;\n  SITE CoreSite ;\n'
               '  PIN A\n    DIRECTION INPUT ;\n    USE SIGNAL ;\n    ANTENNADIFFAREA 0.5292 LAYER Metal1 ;\n'
               '    PORT\n      LAYER Metal1 ;\n        RECT 0.3 1.2 0.5 1.8 ;\n    END\n  END A\nEND ANTENNA2\n\n')
    out.append('MACRO RAM8\n  CLASS BLOCK ;\n  ORIGIN 0 0 ;\n  SIZE 60 BY 40 ;\n'
               '  PIN A_DIN\n    DIRECTION INPUT ;\n    USE SIGNAL ;\n    PORT\n      LAYER Metal2 ;\n'
               '        RECT 10 0 10.4 0.4 ;\n    END\n  END A_DIN\nEND RAM8\n\nEND LIBRARY\n')
    return ''.join(out)


def pin_table(corner, ram):
    rows = []
    if ram:
        rows.append(('RAM8', 'A_DIN', 'Input', '0.02', 'none', '1', 'false', 'none'))
    else:
        scale = 1.0 if corner == 'tt' else 0.95
        limits = {'BUFX1': 0.10, 'BUFX2': 0.20, 'BUFX4': 0.40, 'DLY1X2': 0.15, 'INVX1': 0.10, 'NAND2X1': 0.10,
                  'AND2X1': 0.12, 'DFFX1': 0.10}
        caps = {'BUFX1': 0.0015, 'BUFX2': 0.002, 'BUFX4': 0.004, 'DLY1X2': 0.0018, 'INVX1': 0.0014,
                'NAND2X1': 0.0016, 'AND2X1': 0.0017, 'DFFX1': 0.0019}
        for name, (_, pins) in CELLS.items():
            for pin, way in pins.items():
                if way == 'O':
                    rows.append((name, pin, 'Output', '0', repr(limits[name]), '0', 'false', 'none'))
                else:
                    rows.append((name, pin, 'Input', f'{caps[name] * scale:.8g}', 'none', '1',
                                 'true' if pin == 'CK' else 'false', 'none'))
        rows.append(('ANTENNA2', 'A', 'Input', '0.001', 'none', '1', 'false', 'none'))
    return ''.join('\t'.join(r) + '\n' for r in rows)


INPUTS, OUTPUT = ('clk_i', 'rst_ni', 'a_i', 'b_i'), 'q_o'
SDC = ('current_design mini\n'
       'create_clock -name clk_i -period 10.0000 [get_ports {clk_i}]\n'
       'set_clock_transition 0.1500 [get_clocks {clk_i}]\n'
       'set_clock_uncertainty 0.0500 clk_i\n'
       'set_propagated_clock [get_clocks {clk_i}]\n'
       + ''.join(f'set_input_delay 2.0000 -max -clock [get_clocks {{clk_i}}] -add_delay [get_ports {{{p}}}]\n'
                 for p in INPUTS[1:])
       + ''.join(f'set_driving_cell -lib_cell BUFX4 -pin {{Y}} -input_transition_rise 0.0000 '
                 f'-input_transition_fall 0.0000 [get_ports {{{p}}}]\n' for p in INPUTS)
       + 'set_load -pin_load 0.0060 [get_ports {q_o}]\nset_max_fanout 10.0000 [current_design]\n')

# ---------------------------------------------------------------------------------------------
# The design: instance -> (master, {pin: net}), in netlist order.

DESIGN = {'gate': ('AND2X1', {'A': 'clk_i', 'B': 'en', 'Y': 'gclk'}),
          'rbuf': ('BUFX1', {'A': 'rst_ni', 'Y': 'rstn'}),
          'u_hf': ('NAND2X1', {'A': 'a_i', 'B': 'b_i', 'Y': 'hf'}),
          'u_en': ('INVX1', {'A': 'q0', 'Y': 'en'}),
          'u_o': ('BUFX2', {'A': 'q1', 'Y': 'q_o'}),
          'r0': ('DFFX1', {'CK': 'clk_i', 'D': 'q7', 'RB': 'rst_ni', 'Q': 'r0q'}),
          'u_r0': ('INVX1', {'A': 'r0q', 'Y': 'r0n'})}
for _k in range(8):
    DESIGN[f'f{_k}'] = ('DFFX1', {'CK': 'gclk', 'D': f'd{_k}', 'RB': 'rstn', 'Q': f'q{_k}'})
    DESIGN[f'g{_k}'] = ('NAND2X1', {'A': 'hf', 'B': f'q{(_k + 1) % 8}', 'Y': f'd{_k}'})
NETS = list(dict.fromkeys(list(INPUTS) + [OUTPUT] + [n for _, pins in DESIGN.values() for n in pins.values()]))
BIT = {n: k + 2 for k, n in enumerate(NETS)}


def graph():
    modules = {'mini': {
        'ports': {**{p: {'direction': 'input', 'bits': [BIT[p]]} for p in INPUTS},
                  OUTPUT: {'direction': 'output', 'bits': [BIT[OUTPUT]]}},
        'cells': {inst: {'type': master,
                         'port_directions': {p: 'input' if CELLS[master][1][p] == 'I' else 'output' for p in pins},
                         'connections': {p: [BIT[n]] for p, n in pins.items()}}
                  for inst, (master, pins) in DESIGN.items()},
        'netnames': {n: {'hide_name': 0, 'bits': [BIT[n]]} for n in NETS}}}
    for master in dict.fromkeys(m for m, _ in DESIGN.values()):
        modules[master] = {'attributes': {'blackbox': '1'},
                           'ports': {p: {'direction': 'input' if way == 'I' else 'output', 'bits': [k + 2]}
                                     for k, (p, way) in enumerate(CELLS[master][1].items())},
                           'cells': {}, 'netnames': {}}
    return json.dumps({'creator': 'RustEDA native connectivity v1', 'modules': modules}, indent=1) + '\n'


def verilog(tag):
    lines = [f'// {tag}\nmodule mini (clk_i, rst_ni, a_i, b_i, q_o);\n input clk_i, rst_ni, a_i, b_i;\n output q_o;\n']
    lines += [f' wire {n};\n' for n in NETS if n not in INPUTS and n != OUTPUT]
    for inst, (master, pins) in DESIGN.items():
        lines.append(f' {master} {inst} (' + ', '.join(f'.{p}({n})' for p, n in pins.items()) + ');\n')
    return ''.join(lines) + 'endmodule\n'


def place():
    """instance -> (x, y) in DBU: rows of 3.36 um in a 200 x 120 um core, left of and below the macro."""
    out, x, y = {}, 12000, 10080
    for inst, (master, _) in DESIGN.items():
        width = round(CELLS[master][0] * 1000)
        if x + width > 100000:
            x, y = 12000, y + 3360 * 4
        out[inst] = (x, y)
        x += width + 8400
    return out


PORT_AT = {'clk_i': (0, 30000), 'rst_ni': (0, 60000), 'a_i': (50000, 0), 'b_i': (90000, 0), 'q_o': (199620, 50000)}


def placed_def(planning):
    at = place()
    comps = [f'- {i} {DESIGN[i][0]} + PLACED ( {x} {y} ) N ;\n' for i, (x, y) in at.items()]
    comps.append('- u_ram RAM8 + FIXED ( 120000 60000 ) N ;\n')
    if not planning:
        comps.append('- fill_0 BUFX1 + PLACED ( 150000 10080 ) FS ;\n')
    pins = [f'- {p} + NET {p} + DIRECTION {"OUTPUT" if p == OUTPUT else "INPUT"} + USE SIGNAL + PORT + LAYER Metal3 '
            f'( 0 0 ) ( 380 380 ) + {"PLACED" if planning else "FIXED"} ( {x} {y} ) N ;\n' for p, (x, y) in PORT_AT.items()]
    pins.append('- VDD + NET VDD + DIRECTION INOUT + USE POWER ;\n')
    nets = []
    for n in NETS:
        ends = ([('PIN', n)] if n in PORT_AT else []) + [(i, p) for i, (_, pins_) in DESIGN.items()
                                                       for p, net in pins_.items() if net == n]
        nets.append(f'- {n} ' + ' '.join(f'( {i} {p} )' for i, p in ends) + ' + USE SIGNAL ;\n')
    return ('VERSION 5.8 ;\nDIVIDERCHAR "/" ;\nBUSBITCHARS "[]" ;\nDESIGN mini ;\nUNITS DISTANCE MICRONS 1000 ;\n'
            'DIEAREA ( 0 0 ) ( 200000 120000 ) ;\n'
            f'COMPONENTS {len(comps)} ;\n' + ''.join(comps) + 'END COMPONENTS\n'
            f'PINS {len(pins)} ;\n' + ''.join(pins) + 'END PINS\n'
            f'NETS {len(nets)} ;\n' + ''.join(nets) + 'END NETS\nEND DESIGN\n')


def driver(net):
    if net in INPUTS:
        return net
    return next(f'{i}/{p}' for i, (m, pins) in DESIGN.items() for p, n in pins.items()
                if n == net and CELLS[m][1][p] == 'O')


def envelope(tag, corner, wire, sections, metrics):
    return json.dumps({
        'schema': 'rusteda.v1', 'ok': True, 'command': 'ruststa report',
        'workspace': f'@RUN@/frontend/sta-{tag}', 'base_rev': None, 'rev': 'rev2', 'new_rev': None,
        'scope': {'corner': corner, 'wire_model': wire, 'parasitics': 'spef' if wire == 'spef_rc' else None,
                  'clock_model': 'propagated', 'cppr': 'off', 'corners': list(CORNERS)},
        'completeness': {'constrained_endpoints': 18, 'clock_domains': ['clk_i']},
        'metrics': metrics, 'sections': sections, 'diagnostics': [], 'error': None}, indent=1) + '\n'


def drv(tag, corner, wire, rows):
    rows = [dict(pin=pin, handle=f'pin{k}', corner=corner, kind=kind, net=net, value=value, limit=limit,
                 slack=limit - value, unit='pf' if kind == 'max_capacitance' else 'fanout_load')
            for k, (pin, kind, net, value, limit) in enumerate(rows)]
    count = lambda kind: sum(r['kind'] == kind and r['slack'] < 0 for r in rows)
    metrics = dict(max_transition_violating=count('max_transition'), max_capacitance_violating=count('max_capacitance'),
                   max_fanout_violating=count('max_fanout'), worst_setup_slack_ns=7.25, worst_hold_slack_ns=0.01,
                   hold_violating_endpoints=0)
    section = dict(columns=['pin', 'kind', 'net', 'value', 'limit', 'slack'], total=len(rows) + 40, matched=len(rows),
                   shown=len(rows), rows=rows, stored=len(rows) + 40, truncated=0, unevaluated=0)
    return envelope(tag, corner, wire, {'drv': section}, metrics)


def endpoints(tag, corner):
    scale = 1.0 if corner == 'tt' else 1.5
    rows = []
    for k in range(8):
        rows.append(dict(endpoint=f'f{k}/D', kind='flip_flop', setup_slack_ns=7.25 + k, hold_slack_ns=0.30 * scale))
        rows.append(dict(endpoint=f'f{k}/RB', kind='async_recovery_removal', setup_slack_ns=8.5, hold_slack_ns=0.9))
    rows.append(dict(endpoint='r0/D', kind='flip_flop', setup_slack_ns=6.0, hold_slack_ns=1.1 * scale))
    rows.append(dict(endpoint='q_o', kind='output_port', setup_slack_ns=5.5, hold_slack_ns=None))
    section = dict(columns=['endpoint', 'kind'], total=len(rows), matched=len(rows), shown=len(rows), rows=rows)
    return envelope(tag, corner, 'spef_rc', {'endpoints': section}, dict(worst_setup_slack_ns=5.5))


# Hold endpoints of the estimated tag: endpoint -> (start point, slack tt, slack ss).
HOLD = {'f3/D': ('f4/CK', 0.010, 0.060), 'f5/D': ('f6/CK', 0.200, 0.300), 'f6/D': ('a_i', -0.020, 0.040),
        'f7/D': ('a_i', 0.030, 0.070), 'f0/D': ('r0/CK', 0.050, 0.090)}


def hold_report(tag):
    def detail(end, start, slack):
        inst, pin = end.split('/')
        net = DESIGN[inst][1][pin]
        gate = driver(net).split('/')[0]
        stages = []
        if '/' in start:
            reg = start.split('/')[0]
            stages.append(dict(pin=start, arc='launch', net=DESIGN[reg][1]['CK'], incr_ns=0.9))
            stages.append(dict(pin=f'{reg}/Q', arc='cell', net=DESIGN[reg][1]['Q'], incr_ns=0.35))
        else:
            stages.append(dict(pin=start, arc='launch', net=start, incr_ns=2.0))
            stages.append(dict(pin='u_hf/Y', arc='cell', net='hf', incr_ns=0.21))
        stages.append(dict(pin=f'{gate}/Y', arc='cell', net=net, incr_ns=0.12))
        stages.append(dict(pin=end, arc='net', net=net, incr_ns=0.0001))
        return dict(endpoint=end, handle='pin1', inst=inst, cell=DESIGN[inst][0], capture_clock='clk_i',
                    hold_slack_ns=slack, required_delay_ns=0.5 - slack, setup_slack_ns=8.0, net=net,
                    net_driver=driver(net), net_fanout=1, startpoint=start,
                    path=dict(handle='path1', startpoint=start, slack_ns=slack, stages=stages))
    corners = []
    for k, corner in enumerate(CORNERS):
        ends = [detail(end, start, slacks[k]) for end, (start, *slacks) in HOLD.items()]
        ends.sort(key=lambda d: d['hold_slack_ns'])
        corners.append(dict(corner=corner, hold_endpoints=18, hold_violating=sum(d['hold_slack_ns'] < 0 for d in ends),
                            listed=len(ends), shown=len(ends), endpoints=ends))
    merged = [dict(endpoint=end, handle='pin1', worst_hold_slack_ns=min(tt, ss),
                   hold_slack_by_corner_ns={'ss': ss, 'tt': tt}) for end, (_, tt, ss) in HOLD.items()]
    merged.sort(key=lambda d: d['worst_hold_slack_ns'])
    section = dict(target_margin_ns=0.5, corners=corners, merged_total=len(merged), merged=merged)
    return envelope(tag, 'tt', 'spef_rc', {'hold_fix': section}, dict(worst_hold_slack_ns=-0.02))


def xxh3(tool, path):
    out = subprocess.check_output([tool, '-H3', '--', str(path)], text=True)
    return 'xxh3:' + re.fullmatch(r'XXH3_([0-9a-fA-F]{16})[ \t]+[^\n]+\n?', out)[1].lower()


def sta_meta(tool, tag, netlist, spef):
    """meta.json of the STA revision that compiled the reports of `tag` (paths as placeholders)."""
    sources = [dict(table='constraints', path='@FIX@/pdk/constraints.sdc', hash=xxh3(tool, FIX / 'pdk/constraints.sdc')),
               dict(table='netlist', path='@RUN@/' + netlist, hash=xxh3(tool, FIX / 'run' / netlist))]
    if spef:
        sources += [dict(table='parasitics', path='@RUN@/' + spef, hash=xxh3(tool, FIX / 'run' / spef))] * 2
    libraries = [dict(name=f'{kind}_{co}', corner=co, hash='xxh3:0000000000000000', source=f'@FIX@/pdk/{kind}_{co}.lib',
                      compiled=f'cache/{kind}_{co}-0000000000000000.rlib') for co in CORNERS for kind in ('std', 'ram')]
    return json.dumps(dict(schema='rusteda.revision.v1', rev='rev2', parent='rev1', tool='ruststa', command='ruststa run',
                           libraries=libraries, sources=sources), indent=1) + '\n'


def write(rel, text):
    path = FIX / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def build(tool):
    """The fixture inputs: PDK, recipe policies and the template of the run tree."""
    for co in CORNERS:
        write(f'pdk/std_{co}.lib', liberty(co))
        write(f'pdk/ram_{co}.lib', f'library (ram_{co}) {{\n  time_unit : "1ns" ;\n  capacitive_load_unit (1, pf) ;\n'
                                   '  cell (RAM8) {\n    area : 2400 ;\n  }\n}\n')
    write('pdk/cells.lef', lef())
    write('pdk/constraints.sdc', SDC)
    selection = dict(
        top='mini', primary_clock='clk_i', clock_sink_pin='CK', gating_cells=['AND2X1'],
        buffers=[dict(cell=c, input='A', output='Y') for c in ('BUFX1', 'BUFX2', 'BUFX4')], primary_buffer='BUFX4',
        reset=dict(pin='RB', leaf='BUFX2', trunk='BUFX4'), delay_cells=['DLY1X2'], target_fanout=3, cap_fraction=0.8,
        hold_margin_ns=0.075, setup_margin_ns=0.05, port_diode=dict(cell='ANTENNA2', pin='A'),
        electrical=dict(driver_cells=['BUFX2', 'BUFX4'], sram_buffer='BUFX4', sram_clock_buffer='BUFX4',
                        sram_masters=['RAM8'], load_fraction=0.6, max_wire_um=1200, antenna_cells=['ANTENNA2'],
                        drv_margin=0.15, hold_drv_margin=0.1,
                        clock_balance=dict(cell='DLY1X2', max_stages=32, margin_ns=0.1, wire_pf=0.002)),
        launch_chain_max=32)
    write('recipe/selection.json', json.dumps(selection, indent=1) + '\n')
    area = lambda lx, ly, ux, uy: dict(lx=lx, ly=ly, ux=ux, uy=uy)
    buses = {'RAM8': {'A_DIN': [7, 0]}}
    write('recipe/floorplan.json', json.dumps(dict(name='mini', units_per_micron=1000, die_area=area(0, 0, 200000, 120000),
                                                  core_area=area(10080, 10080, 189840, 110880), pin_buses=buses),
                                             indent=1) + '\n')
    write('recipe/interfaces.json', json.dumps(dict(cell_buses=buses), indent=1) + '\n')
    write('recipe/physical.json', json.dumps(dict(
        power_net='VPWR', io_layer='Metal3', filler_cells=[f'FILL{k}' for k in range(9)], io_width=300, io_spacing=840,
        tap=dict(cell='WELLTAP', max_gap=42000, pattern=dict(kind='staggered', phase=21000, unpaired='cell_top'),
                 body_view='gds'),
        clock=dict(port='clk_i', root_net='', cell='BUFX4', max_fanout=8)), indent=1) + '\n')
    write('recipe/routing.json', json.dumps(dict(
        track_router=dict(min_routing_layer='Metal2', max_routing_layer='TopMetal2'),
        limits=dict(max_axis=1048576, replay=dict(path='{output}/backend/route-budget-error.json', net=None, round=None,
                                                  max_bytes=536870912, phase='post_budget_error'))), indent=1) + '\n')
    write('recipe/rc-policy.json', json.dumps(dict(
        schema='rusteda.rc-estimate-policy.v1', r_ohm_per_um=0.5, c_pf_per_um=0.0002, detour=1.2, max_segment_um=50,
        limits=dict(max_nets=1000, max_pins_per_net=100, max_nodes=100000),
        macro_blockage=dict(grid_um=5, max_searches=1000)), indent=1) + '\n')
    write('recipe/rustroad', 'placeholder of the pinned rustroad: the tests plan only\n')
    # The run tree.
    for co in CORNERS:
        write(f'run/logs/fe_caps_{co}_std.stdout', pin_table(co, False))
        write(f'run/logs/fe_caps_{co}_ram.stdout', pin_table(co, True))
    write('run/frontend/profiles/io.json', json.dumps(dict(
        sdc_sha256=sha(FIX / 'pdk/constraints.sdc'), drivers={p: dict(cell='BUFX4', pin='Y') for p in INPUTS},
        loads_pf={OUTPUT: 0.006}), indent=2) + '\n')
    for name in ('delay', 'sizing'):
        write(f'run/frontend/profiles/{name}.json', json.dumps(dict(note=f'{name} profile: hashed, read by apply only'),
                                                              indent=2) + '\n')
    write('run/frontend/profiles/cell-buses.json', json.dumps(buses, indent=2) + '\n')
    write('run/frontend/mini-graph.json', graph())
    write('run/frontend/mini.v', verilog('source'))
    write('run/logs/fe_est_export.stdout', verilog('timed'))
    write('run/logs/fe_ideal_export.stdout', verilog('timed ideal'))
    write('run/logs/fe_pins_def.stdout', placed_def(True))
    write('run/logs/fe_est_def.stdout', placed_def(False))
    at = place()
    write('run/logs/fe_est_seed.stdout', json.dumps(dict(
        schema='rustroad.placement-seed.v1',
        source=dict(design='mini', imported_netlist_sha256=sha(FIX / 'run/frontend/mini.v'), revision='rev3',
                    units_per_micron=1000),
        cells={i: list(at[i]) for i in sorted(at)}, clock_sinks=[f'f{k}' for k in range(8)],
        regroup={'native_fanout_0_0_1': [['g3', 'A'], ['f3', 'D']], 'native_fanout_0_0_0': [['g0', 'A'], ['g1', 'A']]}),
        indent=2) + '\n')
    write('run/frontend/estimate-est/estimate.spef', '*SPEF "IEEE 1481-1999"\n*DESIGN "mini"\n')
    write('run/frontend/estimate-est/result.json', json.dumps(dict(
        schema='rusteda.rc-estimate-result.v1',
        inputs=dict({'def': dict(sha256=sha(FIX / 'run/logs/fe_est_def.stdout'))},
                    policy=dict(sha256=sha(FIX / 'recipe/rc-policy.json')))), indent=2) + '\n')
    write('run/frontend/eco-electrical/weights.json', json.dumps(dict(
        schema='rustroad.net-weights.v1', source=dict(operation='electrical', carried=0), weights={'hf': 16000, 'en': 16000}),
        indent=1) + '\n')
    for co in CORNERS:
        write(f'run/logs/fe_ideal_{co}_drv.stdout',
              drv('ideal', co, 'ideal', [('u_hf/Y', 'max_fanout', 'hf', 8.0, 3.0), ('gate/Y', 'max_fanout', 'gclk', 8.0, 3.0),
                                         ('f1/Q', 'max_capacitance', 'q1', 0.004, 0.1)]))
        write(f'run/logs/fe_est_{co}_drv.stdout',
              drv('est', co, 'spef_rc', [('f1/Q', 'max_capacitance', 'q1', 0.104 if co == 'tt' else 0.098, 0.1),
                                         ('u_hf/Y', 'max_capacitance', 'hf', 0.093, 0.1),
                                         ('f2/Q', 'max_capacitance', 'q2', 0.02, 0.1),
                                         ('g4/Y', 'max_transition', 'd4', 0.2, 0.9)]))
        write(f'run/logs/fe_est_{co}_endpoints.stdout', endpoints('est', co))
    write('run/logs/fe_est_hold.stdout', hold_report('est'))
    for tag, netlist, spef in (('ideal', 'logs/fe_ideal_export.stdout', None),
                               ('est', 'logs/fe_est_export.stdout', 'frontend/estimate-est/estimate.spef')):
        write(f'run/frontend/sta-{tag}/workspace.lock', '')
        write(f'run/frontend/sta-{tag}/revisions/rev0002/meta.json', sta_meta(tool, tag, netlist, spef))
    write('run/frontend/bundle-reset/roots.json', json.dumps(dict(
        clock_root='gclk', reset_root='rstn', clock_driver=dict(instance='gate', cell='AND2X1', pin='Y'),
        reset_driver=dict(instance='rbuf', cell='BUFX1', pin='Y'), clock_sinks=8, reset_sinks=8,
        sdc=dict(path='@FIX@/pdk/constraints.sdc', sha256=sha(FIX / 'pdk/constraints.sdc'),
                 bytes=(FIX / 'pdk/constraints.sdc').stat().st_size)), indent=2) + '\n')
    # What frontend-validate reads beside the outputs of the other cases.
    write('run/step-receipts.json', json.dumps([dict(id=f'fe_{t}_cts', kind='command', exit_code=0)
                                               for t in ('temporary', 'repair', 'final')], indent=1) + '\n')
    for tag in ('temporary', 'repair', 'final'):
        write(f'run/frontend/road-{tag}/HEAD', 'rev3\n')
        write(f'run/logs/fe_{tag}_cts.stdout', json.dumps(dict(revision='rev3', clock_buffers=3,
                                                              clock_topology_status='unrouted_topology_checked')) + '\n')
    write('run/frontend/eco-primary-clock/netlist.v', verilog('source'))
    write('run/frontend/eco-hold/netlist.v', verilog('before clock tree'))
    write('run/logs/fe_final_export.stdout', verilog('final'))
    write('run/logs/fe_final_def.stdout', placed_def(False))
    write('run/logs/fe_temporary_seed.stdout', json.dumps(dict(
        schema='rustroad.placement-seed.v1',
        source=dict(design='mini', imported_netlist_sha256=sha(FIX / 'run/frontend/eco-primary-clock/netlist.v'),
                    revision='rev3', units_per_micron=1000), cells={}), indent=2) + '\n')
    for k, op in enumerate(('reset', 'fanout', 'primary-clock', 'electrical', 'hold')):
        write(f'run/frontend/eco-{op}/receipt.json', json.dumps(dict(
            schema='rusteda.native-eco-receipt.v1', operation=op, operations=3 * k + 1,
            scope='estimated_rc' if op in ('electrical', 'hold') else 'ideal_diagnostic'), indent=2) + '\n')
    write('run/frontend/estimate-final/result.json', json.dumps(dict(
        inputs={'def': dict(sha256=sha(FIX / 'run/logs/fe_final_def.stdout'))}), indent=2) + '\n')
    for co in CORNERS:
        write(f'run/logs/fe_final_{co}_drv.stdout', drv('final', co, 'spef_rc', [('f2/Q', 'max_capacitance', 'q2', 0.02, 0.1)]))


# ---------------------------------------------------------------------------------------------
# The reference run.

FILES = dict(sdc='pdk/constraints.sdc', std_tt='pdk/std_tt.lib', std_ss='pdk/std_ss.lib', ram_tt='pdk/ram_tt.lib',
             ram_ss='pdk/ram_ss.lib', cell_lef='pdk/cells.lef', std_lef='pdk/cells.lef', floorplan='recipe/floorplan.json',
             physical='recipe/physical.json', routing='recipe/routing.json', interfaces='recipe/interfaces.json',
             ecos='recipe/ecos.py', prepare='recipe/prepare.py', delay_helper='recipe/delay_helper.py',
             sizing_helper='recipe/sizing_helper.py', liberty_reader='recipe/liberty_lite.py',
             estimator='recipe/estimator.py', rc_policy='recipe/rc-policy.json')
# Files whose bytes hold a path: their digests are written as @SHA:<label>@ / @BYTES:<label>@.
VOLATILE = {'xxhsum': None, 'roots': 'frontend/bundle-reset/roots.json', 'routing': 'frontend/physical/routing.json',
            'pins-receipt': 'frontend/physical-pins/receipt.json', 'cache': 'frontend/sha256-cache.json',
            **{f'{tag}-meta': f'frontend/sta-{tag}/revisions/rev0002/meta.json' for tag in ('ideal', 'est')},
            **{f'{tag}-drv-{co}': f'logs/fe_{tag}_{co}_drv.stdout' for tag in ('ideal', 'est', 'final') for co in CORNERS},
            **{f'est-endpoints-{co}': f'logs/fe_est_{co}_endpoints.stdout' for co in CORNERS},
            'est-hold': 'logs/fe_est_hold.stdout',
            **{f'{op}-inputs': f'frontend/bundle-{op}/inputs.json' for op in ('fanout', 'hold')}}


def record(path):
    path = Path(path).resolve()
    return dict(path=str(path), sha256=sha(path), bytes=path.stat().st_size)


def instantiate(run, tool):
    """Copies the run-tree template to `run` with its placeholders replaced, and writes the context."""
    for src in sorted((FIX / 'run').rglob('*')):
        dst = run / src.relative_to(FIX / 'run')
        if src.is_dir():
            dst.mkdir(parents=True, exist_ok=True)
        else:
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.write_bytes(src.read_bytes().replace(b'@RUN@', str(run).encode()).replace(b'@FIX@', str(FIX).encode()))
    # The raw Liberty copies of the two STA workspaces.
    for tag in ('ideal', 'est'):
        cache = run / f'frontend/sta-{tag}/cache'
        cache.mkdir(parents=True, exist_ok=True)
        for co in CORNERS:
            for kind in ('std', 'ram'):
                shutil.copyfile(FIX / f'pdk/{kind}_{co}.lib', cache / f'{kind}_{co}-0000000000000000.lib')
    context = dict(schema='rusteda.native-frontend-context.v1', run_root=str(run),
                   files={k: record(FIX / v) for k, v in FILES.items()},
                   tools=dict(rustroad=record(FIX / 'recipe/rustroad'), xxhsum=record(tool)),
                   selection=json.loads((FIX / 'recipe/selection.json').read_text()))
    (run / 'frontend/context.json').write_text(json.dumps(context, indent=2) + '\n')


def normalize(text, run, tool):
    for label, rel in VOLATILE.items():
        path = Path(tool) if rel is None else run / rel
        if path.is_file():
            text = text.replace(sha(path), f'@SHA:{label}@')
            text = re.sub(r'("sha256": "@SHA:%s@",\n\s*"bytes": )%d\b' % (re.escape(label), path.stat().st_size),
                          r'\1"@BYTES:%s@"' % label, text)
    text = text.replace(str(run), '@RUN@').replace(str(FIX), '@FIX@').replace(str(Path(tool).resolve()), '@XXHSUM@')
    for key, mark in (('elapsed_seconds', '@ELAPSED@'), ('peak_rss_kib', '@PEAK@'), ('planner_sha256', '@PLANNER@')):
        text = re.sub(r'(\n  "%s": )[^\n,]+' % key, r'\1"%s"' % mark, text)
    return text


def keep(case, run, tool, folder):
    out = FIX / 'expected' / case
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    for path in sorted((run / folder).iterdir()):
        if path.name in ('workspace.lock',):
            continue
        (out / path.name).write_text(normalize(path.read_text(), run, tool))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--recipe', type=Path, required=True)
    parser.add_argument('--road-scripts', type=Path, required=True)
    parser.add_argument('--xxhsum', type=Path, required=True)
    args = parser.parse_args()
    tool = str(args.xxhsum.resolve())
    build(tool)
    env = dict(os.environ, RUSTEDA_REFERENCE_RECIPE=str(args.recipe.resolve()),
               RUSTEDA_REFERENCE_ROAD_SCRIPTS=str(args.road_scripts.resolve()), PYTHONDONTWRITEBYTECODE='1')
    with tempfile.TemporaryDirectory(prefix='rusteda-flow-reference-') as tmp:
        run = Path(tmp).resolve() / 'run'
        instantiate(run, tool)
        context = str(run / 'frontend/context.json')

        def helper(command, workspace, *extra):
            subprocess.run([sys.executable, str(FIX / 'recipe/prepare.py'), command, '--context', context,
                            '--workspace', str(run / workspace), '--base-rev', 'empty', *extra], check=True, env=env, cwd=run)

        def planned(op, tag, source, graph, timed):
            helper('bundle', f'frontend/bundle-{op}', '--operation', op, '--timing-tag', tag, '--source', str(run / source),
                   '--graph', str(run / graph), '--timed', str(run / timed))
            bundle = run / f'frontend/bundle-{op}'
            subprocess.run([sys.executable, str(FIX / 'recipe/ecos.py'), op, '--workspace', str(run / f'frontend/plan-{op}'),
                            '--base-rev', 'empty', '--inputs', str(bundle / 'inputs.json'), '--policy', str(bundle / 'policy.json'),
                            '--plan-only', '--digest-cache', str(run / 'frontend/sha256-cache.json')],
                           check=True, env=env, cwd=run, stdout=subprocess.DEVNULL)
            keep(f'bundle-{op}', run, tool, f'frontend/bundle-{op}')
            keep(f'plan-{op}', run, tool, f'frontend/plan-{op}')

        helper('profiles', 'frontend/profiles-out')
        keep('profiles', run, tool, 'frontend/profiles-out')
        helper('physical', 'frontend/physical', '--roots', str(run / 'frontend/bundle-reset/roots.json'))
        keep('physical', run, tool, 'frontend/physical')
        helper('pins', 'frontend/physical-pins', '--def', str(run / 'logs/fe_pins_def.stdout'))
        keep('physical-pins', run, tool, 'frontend/physical-pins')
        planned('fanout', 'ideal', 'frontend/mini.v', 'frontend/mini-graph.json', 'logs/fe_ideal_export.stdout')
        planned('hold', 'est', 'frontend/mini.v', 'frontend/mini-graph.json', 'logs/fe_est_export.stdout')
        helper('validate', 'frontend/result')
        keep('result', run, tool, 'frontend/result')
    print('expected files written under', FIX / 'expected')


if __name__ == '__main__':
    main()

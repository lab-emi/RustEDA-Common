// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Read-only, streaming Verilog export from validated mmap tables.
//!
//! C910 planning: ~720 MiB mapped data + <=200 MiB wire-group records + name
//! flags, a 1 MiB output buffer and small per-cell strings. No mutable netlist,
//! duplicated connection graph, or whole output String is materialized. General
//! escaped-name collision checking has an explicit auxiliary-memory budget.
//! All scans are linear in table size; no graph paths or cones are traversed.

use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};

use rusteda_core::names::unescape;
use rusteda_formats::verilog::write::{ident, is_simple_ident};
use rusteda_workspace::Error;
use rusteda_workspace::netlist::{ANONYMOUS, CONST0, CONST1, FrozenNetlist, SCALAR, module_entry};

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub max_nets: usize,
    pub max_instances: usize,
    pub max_strings: usize,
    pub max_cells: usize,
    pub max_table_bytes: u64,
    pub max_aux_bytes: u64,
    pub max_name_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nets: 20_000_000,
            max_instances: 10_000_000,
            max_strings: 32_000_000,
            max_cells: 65_536,
            max_table_bytes: 2 * 1024 * 1024 * 1024,
            max_aux_bytes: 1024 * 1024 * 1024,
            max_name_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy)]
struct Group {
    first: u32,
    last: u32,
    count: u32,
    min: i32,
    max: i32,
    order: u32,
}

impl Default for Group {
    fn default() -> Self {
        Self {
            first: u32::MAX,
            last: 0,
            count: 0,
            min: 0,
            max: 0,
            order: 3,
        }
    }
}

fn raw(name: &str) -> Cow<'_, str> {
    if name.contains('\\') {
        Cow::Owned(unescape(name))
    } else {
        Cow::Borrowed(name)
    }
}

fn input(message: impl Into<String>) -> Error {
    Error::input_rejected(message)
}

fn string<'f>(f: &'f FrozenNetlist<'_>, sym: u32) -> &'f str {
    f.string(sym).expect("validated frozen string")
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "port",
        2 => "net",
        _ => "instance",
    }
}

pub(super) struct Plan<'f, 'a> {
    f: &'f FrozenNetlist<'a>,
    groups: Vec<Group>,
    port_names: Vec<bool>,
    normal_ranges: bool,
    parallel: bool,
    aux_bytes: u128,
    max_aux_bytes: u128,
    cell_buses: HashMap<u32, Vec<CellPort>>,
}

/// Explicit interface metadata; a bracket in a scalar identifier is never guessed to be a bus.
pub(super) type CellBusSchemas = BTreeMap<String, BTreeMap<String, [i32; 2]>>;

enum CellPort {
    Scalar(usize),
    Bus { name: String, pins: Vec<usize> },
}

fn identifier(out: &mut impl Write, name: &str) -> io::Result<()> {
    if is_simple_ident(name) {
        out.write_all(name.as_bytes())
    } else {
        write!(out, "\\{name} ")
    }
}

struct Chunk(Vec<u8>);
impl Write for Chunk {
    fn write(&mut self, value: &[u8]) -> io::Result<usize> {
        const LIMIT: usize = 2 * 1024 * 1024;
        let needed = self
            .0
            .len()
            .checked_add(value.len())
            .filter(|n| *n <= LIMIT)
            .ok_or_else(|| io::Error::other("EXPORT_CHUNK_BUDGET"))?;
        if needed > self.0.capacity() {
            self.0
                .reserve_exact(needed.next_power_of_two().min(LIMIT) - self.0.len());
        }
        self.0.extend_from_slice(value);
        Ok(value.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'f, 'a> Plan<'f, 'a> {
    /// `f` must have passed the workspace decoder's complete table validation.
    pub fn new(f: &'f FrozenNetlist<'a>, limits: Limits) -> Result<Self, Error> {
        let strings = f.str_offs.len().saturating_sub(1);
        if f.nets.len() > limits.max_nets
            || f.instances.len() > limits.max_instances
            || strings > limits.max_strings
            || f.cells.len() > limits.max_cells
        {
            return Err(input(
                "EXPORT_BUDGET: frozen netlist exceeds entity-count limits",
            ));
        }
        if f.str_offs
            .windows(2)
            .any(|w| (w[1] - w[0]) as usize > limits.max_name_bytes)
        {
            return Err(input("EXPORT_BUDGET: name exceeds the per-name byte limit"));
        }
        let group_count = f
            .nets
            .iter()
            .filter(|n| n.name != ANONYMOUS)
            .map(|n| n.name as usize + 1)
            .max()
            .unwrap_or(0);
        let escaped = f.str_bytes.contains(&b'\\');
        let mut aux = group_count as u128 * (size_of::<Group>() + size_of::<bool>()) as u128
            + strings as u128;
        aux += f.cells.len() as u128 * size_of::<(String, usize)>() as u128
            + f.cells
                .iter()
                .map(|&name| string(f, name).len() as u128 + 2)
                .sum::<u128>();
        aux += f.cell_pin_name.len() as u128 * size_of::<String>() as u128
            + f.cell_pin_name
                .iter()
                .map(|&name| string(f, name).len() as u128 + 2)
                .sum::<u128>();
        if escaped {
            // Cow keys borrow ordinary names. Count all possible owned raw
            // names and generous hash-table capacity before allocating it.
            let decls = (f.nets.len() + f.instances.len() + f.ports.len()) as u128;
            aux += decls
                .checked_next_power_of_two()
                .unwrap_or(u128::MAX)
                .saturating_mul(64)
                + f.str_bytes.len() as u128;
        }
        if aux > limits.max_aux_bytes as u128 {
            return Err(input(
                "EXPORT_BUDGET: auxiliary name/group storage exceeds its limit",
            ));
        }
        let mut groups = vec![Group::default(); group_count];
        let mut port_names = vec![false; group_count];
        let mut seen = vec![0u8; strings];
        let mut taken: Option<HashMap<Cow<'_, str>, (u32, u8)>> = escaped.then(HashMap::new);
        let mut declare = |sym: u32, kind: u8| -> Result<(), Error> {
            let stored = string(f, sym);
            if seen[sym as usize] != 0 {
                return Err(input(format!(
                    "{} '{stored}' has the name of a {}; Verilog needs distinct names",
                    kind_name(kind),
                    kind_name(seen[sym as usize])
                )));
            }
            if let Some(taken) = &mut taken {
                let name = raw(stored);
                if let Some(&(old, old_kind)) = taken.get(name.as_ref()) {
                    return Err(input(format!(
                        "{} '{stored}' and {} '{}' are both written as '{name}'; Verilog needs distinct names",
                        kind_name(kind),
                        kind_name(old_kind),
                        string(f, old)
                    )));
                }
                taken.insert(name, (sym, kind));
            }
            seen[sym as usize] = kind;
            Ok(())
        };
        let mut normal_ranges = true;
        for port in f.ports.iter() {
            declare(port.name, 1)?;
            port_names[port.name as usize] = true;
            normal_ranges &=
                port.dir != 2 && (port.is_bus == 0 || (port.msb >= 0 && port.lsb == 0));
        }
        for (id, net) in f.nets.iter().enumerate() {
            if net.name == ANONYMOUS {
                return Err(input(format!(
                    "net{id} has no name; the writer needs a name for every net"
                )));
            }
            if port_names[net.name as usize] {
                continue;
            }
            let group = &mut groups[net.name as usize];
            if group.count == 0 {
                declare(net.name, 2)?;
                group.first = id as u32;
                group.min = net.index;
                group.max = net.index;
            } else {
                let previous = f.nets[group.last as usize].index as i64;
                if net.index as i64 != previous + 1 {
                    group.order &= !1;
                }
                if net.index as i64 != previous - 1 {
                    group.order &= !2;
                }
                group.min = group.min.min(net.index);
                group.max = group.max.max(net.index);
            }
            group.last = id as u32;
            group.count += 1;
        }
        for group in groups.iter().filter(|g| g.count > 0) {
            normal_ranges &= group.min == SCALAR
                || (group.min == 0 && group.max as u64 + 1 == group.count as u64);
        }
        if !normal_ranges {
            for group in groups.iter().filter(|g| g.count > 0 && g.min != SCALAR) {
                if group.order == 0 || group.max as i64 - group.min as i64 + 1 != group.count as i64
                {
                    return Err(input("wire bits do not form a contiguous bus range"));
                }
            }
        }
        let module = string(f, f.module[module_entry::NAME]);
        for instance in f.instances.iter() {
            declare(instance.name, 3)?;
            if string(f, f.cells[instance.cell as usize]) == module {
                return Err(input(format!(
                    "instance '{}' has the module's name '{module}' as its cell",
                    string(f, instance.name)
                )));
            }
        }
        Ok(Self {
            f,
            groups,
            port_names,
            normal_ranges,
            parallel: aux + 32 * 1024 * 1024 <= limits.max_aux_bytes as u128,
            aux_bytes: aux,
            max_aux_bytes: limits.max_aux_bytes as u128,
            cell_buses: HashMap::new(),
        })
    }

    /// At most 256 cells, 65536 covered pins and 8 MiB of generated pin names.
    /// Builds one table per cell, not per instance; no graph traversal is needed.
    pub fn set_cell_buses(&mut self, schemas: &CellBusSchemas) -> Result<(), Error> {
        const MAX_PINS: usize = 65536;
        // Includes input JSON, transient name/owner maps, port plans, and the
        // 16 maximum parallel chunks' per-instance net scratch. Count scalar
        // pins too: a cell with one bus may have many scalar ports.
        const SCHEMA_AUX_BYTES: u128 = 64 * 1024 * 1024;
        if schemas.len() > 256 {
            return Err(input("CELL_BUS_SCHEMA: more than 256 cells"));
        }
        if self.aux_bytes + SCHEMA_AUX_BYTES > self.max_aux_bytes {
            return Err(input("CELL_BUS_SCHEMA: auxiliary memory budget exceeded"));
        }
        let mut covered = 0usize;
        let mut expanded = 0usize;
        let mut name_bytes = 0usize;
        let mut plans = HashMap::new();
        for (cell_name, buses) in schemas {
            let cell = self
                .f
                .cells
                .iter()
                .position(|&s| string(self.f, s) == cell_name)
                .ok_or_else(|| input(format!("CELL_BUS_SCHEMA: cell {cell_name} is absent")))?;
            let start = self.f.cell_pin_start[cell] as usize;
            let end = self.f.cell_pin_start[cell + 1] as usize;
            covered = covered
                .checked_add(end - start)
                .ok_or_else(|| input("CELL_BUS_SCHEMA: covered pin budget overflow"))?;
            if covered > MAX_PINS || buses.len() > 4096 {
                return Err(input("CELL_BUS_SCHEMA: cell pin/bus budget exceeded"));
            }
            let names: HashMap<_, _> = (start..end)
                .map(|p| (string(self.f, self.f.cell_pin_name[p]), p - start))
                .collect();
            let mut owner = vec![None; end - start];
            let mut groups = Vec::new();
            for (name, &[msb, lsb]) in buses {
                if name.is_empty() || name.len() > 4096 || names.contains_key(name.as_str()) {
                    return Err(input(format!(
                        "CELL_BUS_SCHEMA: invalid or conflicting port {name}"
                    )));
                }
                let width = (i64::from(msb) - i64::from(lsb)).unsigned_abs() + 1;
                let width =
                    usize::try_from(width).map_err(|_| input("CELL_BUS_SCHEMA: range overflow"))?;
                expanded = expanded
                    .checked_add(width)
                    .ok_or_else(|| input("CELL_BUS_SCHEMA: range overflow"))?;
                name_bytes = name_bytes
                    .checked_add(width.saturating_mul(name.len() + 16))
                    .ok_or_else(|| input("CELL_BUS_SCHEMA: name budget overflow"))?;
                if expanded > MAX_PINS || name_bytes > 8 * 1024 * 1024 {
                    return Err(input("CELL_BUS_SCHEMA: expanded pin/name budget exceeded"));
                }
                let mut pins = Vec::with_capacity(width);
                let mut direction = None;
                let step = if msb <= lsb { 1i64 } else { -1 };
                for k in 0..width {
                    let bit = i64::from(msb) + step * k as i64;
                    let key = format!("{name}[{bit}]");
                    let &pin = names.get(key.as_str()).ok_or_else(|| {
                        input(format!("CELL_BUS_SCHEMA: missing {cell_name}.{key}"))
                    })?;
                    let dir = self.f.cell_pin_dir[start + pin];
                    if dir > 2 || direction.is_some_and(|previous| previous != dir) {
                        return Err(input(format!(
                            "CELL_BUS_SCHEMA: mixed or unsupported direction in {cell_name}.{name}"
                        )));
                    }
                    direction = Some(dir);
                    if owner[pin].replace(groups.len()).is_some() {
                        return Err(input("CELL_BUS_SCHEMA: overlapping bus members"));
                    }
                    pins.push(pin);
                }
                groups.push(Some(CellPort::Bus {
                    name: ident(name),
                    pins,
                }));
            }
            let mut ports = Vec::with_capacity(end - start);
            for (pin, group) in owner.into_iter().enumerate() {
                if let Some(group) = group {
                    if let Some(port) = groups[group].take() {
                        ports.push(port);
                    }
                } else {
                    ports.push(CellPort::Scalar(pin));
                }
            }
            plans.insert(cell as u32, ports);
        }
        self.cell_buses = plans;
        self.parallel = self.aux_bytes + SCHEMA_AUX_BYTES + 32 * 1024 * 1024 <= self.max_aux_bytes;
        Ok(())
    }

    fn reference(&self, out: &mut impl Write, conn: u32) -> io::Result<()> {
        match conn {
            CONST0 => write!(out, "1'b0"),
            CONST1 => write!(out, "1'b1"),
            id => {
                let net = &self.f.nets[id as usize];
                identifier(out, &raw(string(self.f, net.name)))?;
                if net.index != SCALAR {
                    write!(out, "[{}]", net.index)?;
                }
                Ok(())
            }
        }
    }

    fn instance(
        &self,
        out: &mut impl Write,
        id: usize,
        cells: &[(String, usize)],
        pins: &[String],
    ) -> io::Result<()> {
        let f = self.f;
        let instance = &f.instances[id];
        let (cell, pin_start) = &cells[instance.cell as usize];
        write!(out, "  {cell} ")?;
        identifier(out, &raw(string(f, instance.name)))?;
        out.write_all(b" (")?;
        let range = f.instance_pins(id as u32);
        if let Some(ports) = self.cell_buses.get(&instance.cell) {
            let pin_count = f.cell_pin_start[instance.cell as usize + 1] as usize - pin_start;
            let mut nets = vec![None; pin_count];
            for at in range {
                nets[f.pin_cellpin[at] as usize] = Some(f.pin_net[at]);
            }
            let mut first = true;
            for port in ports {
                match port {
                    CellPort::Scalar(pin) => {
                        let Some(net) = nets[*pin] else { continue };
                        if !first {
                            out.write_all(b", ")?;
                        }
                        first = false;
                        write!(out, ".{}(", pins[pin_start + pin])?;
                        self.reference(out, net)?;
                        out.write_all(b")")?;
                    }
                    CellPort::Bus {
                        name,
                        pins: members,
                    } => {
                        if !first {
                            out.write_all(b", ")?;
                        }
                        first = false;
                        write!(out, ".{name}({{")?;
                        for (at, &pin) in members.iter().enumerate() {
                            if at != 0 {
                                out.write_all(b", ")?;
                            }
                            let net = nets[pin].ok_or_else(|| {
                                io::Error::other("CELL_BUS_SCHEMA: unconnected bus member")
                            })?;
                            self.reference(out, net)?;
                        }
                        out.write_all(b"})")?;
                    }
                }
            }
            return out.write_all(b");\n");
        }
        for at in range.clone() {
            if at != range.start {
                out.write_all(b", ")?;
            }
            write!(out, ".{}(", pins[pin_start + f.pin_cellpin[at] as usize])?;
            self.reference(out, f.pin_net[at])?;
            out.write_all(b")")?;
        }
        out.write_all(b");\n")
    }

    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        let f = self.f;
        writeln!(
            out,
            "module {}(",
            ident(string(f, f.module[module_entry::NAME]))
        )?;
        for (id, port) in f.ports.iter().enumerate() {
            let direction = match port.dir {
                0 => "input",
                1 => "output",
                _ => "inout",
            };
            write!(out, "  {direction} wire")?;
            if port.is_bus != 0 {
                write!(out, " [{}:{}]", port.msb, port.lsb)?;
            }
            let sep = if id + 1 == f.ports.len() { "" } else { "," };
            writeln!(out, " {}{sep}", ident(&raw(string(f, port.name))))?;
        }
        writeln!(out, ");")?;
        for (id, net) in f.nets.iter().enumerate() {
            if self.port_names[net.name as usize] {
                continue;
            }
            let group = self.groups[net.name as usize];
            if group.first as usize != id {
                continue;
            }
            write!(out, "  wire")?;
            if net.index != SCALAR {
                let (msb, lsb) = if self.normal_ranges {
                    ((group.count - 1) as i32, 0)
                } else {
                    (net.index, f.nets[group.last as usize].index)
                };
                write!(out, " [{msb}:{lsb}]")?;
            }
            writeln!(out, " {};", ident(&raw(string(f, net.name))))?;
        }
        let cells: Vec<_> = f
            .cells
            .iter()
            .enumerate()
            .map(|(id, &name)| (ident(string(f, name)), f.cell_pin_start[id] as usize))
            .collect();
        let pins: Vec<_> = f
            .cell_pin_name
            .iter()
            .map(|&name| ident(string(f, name)))
            .collect();
        for first in (0..f.instances.len()).step_by(8192) {
            let end = (first + 8192).min(f.instances.len());
            if self.parallel && end - first >= 1024 && rayon::current_num_threads() > 1 {
                let starts: Vec<_> = (first..end).step_by(512).collect();
                let chunks: Vec<_> = starts
                    .par_iter()
                    .map(|&from| {
                        let mut chunk = Chunk(Vec::with_capacity(65_536));
                        for id in from..(from + 512).min(end) {
                            self.instance(&mut chunk, id, &cells, &pins).ok()?;
                        }
                        Some(chunk.0)
                    })
                    .collect();
                for (from, chunk) in starts.into_iter().zip(chunks) {
                    if let Some(bytes) = chunk {
                        out.write_all(&bytes)?;
                    } else {
                        for id in from..(from + 512).min(end) {
                            self.instance(out, id, &cells, &pins)?;
                        }
                    }
                }
            } else {
                for id in first..end {
                    self.instance(out, id, &cells, &pins)?;
                }
            }
        }
        for assign in f.assigns.iter() {
            write!(out, "  assign ")?;
            self.reference(out, assign.lhs)?;
            write!(out, " = ")?;
            self.reference(out, assign.rhs)?;
            writeln!(out, ";")?;
        }
        writeln!(out, "endmodule")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusteda_core::netlist::{Conn, GateNetlist, PinDir, PortDir};

    #[test]
    fn explicit_cell_bus_schema_preserves_direction_and_scalar_identifiers() {
        let mut g = GateNetlist::new("top").unwrap();
        let cell = g
            .add_cell_type(
                "memory",
                &[
                    ("A[4]", PinDir::Input),
                    ("A[3]", PinDir::Input),
                    ("Q[3]", PinDir::Output),
                    ("Q[4]", PinDir::Output),
                ],
            )
            .unwrap();
        let u = g.add_instance("u", cell).unwrap();
        for (pin, value) in [("A[4]", true), ("A[3]", false)] {
            g.connect(u, pin, Conn::Const(value)).unwrap();
        }
        g.add_port("y", PortDir::Output, Some((1, 0))).unwrap();
        g.connect(u, "Q[3]", Conn::Net(g.find_net("y", Some(0)).unwrap()))
            .unwrap();
        g.connect(u, "Q[4]", Conn::Net(g.find_net("y", Some(1)).unwrap()))
            .unwrap();
        let f = g.freeze();
        f.validate().unwrap();
        let mut plan = Plan::new(&f, Limits::default()).unwrap();
        let mut raw = Vec::new();
        plan.write(&mut raw).unwrap();
        assert!(String::from_utf8(raw).unwrap().contains(".\\A[4] (1'b1)"));
        let schema = BTreeMap::from([(
            "memory".into(),
            BTreeMap::from([("A".into(), [4, 3]), ("Q".into(), [3, 4])]),
        )]);
        plan.set_cell_buses(&schema).unwrap();
        let mut out = Vec::new();
        plan.write(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(".A({1'b1, 1'b0})"));
        assert!(out.contains(".Q({y[0], y[1]})"));
        let invalid = BTreeMap::from([(
            "memory".into(),
            BTreeMap::from([("A".into(), [i32::MIN, i32::MAX])]),
        )]);
        assert!(plan.set_cell_buses(&invalid).is_err());
        let missing = BTreeMap::from([("memory".into(), BTreeMap::from([("A".into(), [5, 3])]))]);
        assert!(plan.set_cell_buses(&missing).is_err());
        let mut limited = Plan::new(
            &f,
            Limits {
                max_aux_bytes: 64 * 1024 * 1024,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(limited.set_cell_buses(&schema).is_err());
    }

    #[test]
    fn cell_bus_schema_rejects_mixed_pin_directions() {
        let mut g = GateNetlist::new("top").unwrap();
        let cell = g
            .add_cell_type(
                "mixed",
                &[("B[0]", PinDir::Input), ("B[1]", PinDir::Output)],
            )
            .unwrap();
        let u = g.add_instance("u", cell).unwrap();
        g.connect(u, "B[0]", Conn::Const(true)).unwrap();
        g.add_port("y", PortDir::Output, None).unwrap();
        g.connect(u, "B[1]", Conn::Net(g.find_net("y", None).unwrap()))
            .unwrap();
        let f = g.freeze();
        f.validate().unwrap();
        let mut plan = Plan::new(&f, Limits::default()).unwrap();
        let schema = BTreeMap::from([("mixed".into(), BTreeMap::from([("B".into(), [1, 0])]))]);
        assert!(plan.set_cell_buses(&schema).is_err());
    }

    //2048 cells, O(V+E) storage <16MiB plus <=32MiB temporary text; no path traversal.
    #[test]
    fn parallel_frozen_export_preserves_text_and_bounded_fallback() {
        let mut g = GateNetlist::new("top").unwrap();
        let cell = g
            .add_cell_type("inv", &[("A", PinDir::Input), ("Y", PinDir::Output)])
            .unwrap();
        g.add_port("a", PortDir::Input, None).unwrap();
        g.add_port("y", PortDir::Output, None).unwrap();
        let mut previous = g.find_net("a", None).unwrap();
        for i in 0..2048 {
            let n = g.add_net(Some(&format!("net.{i}")), None).unwrap();
            let inst = g.add_instance(&format!("unit.{i}"), cell).unwrap();
            g.connect(inst, "A", Conn::Net(previous)).unwrap();
            g.connect(inst, "Y", Conn::Net(n)).unwrap();
            previous = n;
        }
        g.add_assign(g.find_net("y", None).unwrap(), Conn::Net(previous))
            .unwrap();
        let f = g.freeze();
        f.validate().unwrap();
        let mut plan = Plan::new(&f, Limits::default()).unwrap();
        let render = |plan: &Plan<'_, '_>, threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    let mut out = Vec::new();
                    plan.write(&mut out).unwrap();
                    out
                })
        };
        let serial = render(&plan, 1);
        assert_eq!(serial, render(&plan, 16));
        plan.parallel = false;
        assert_eq!(serial, render(&plan, 16));
        let mut chunk = Chunk(Vec::new());
        assert!(chunk.write_all(&vec![0; 2 * 1024 * 1024 + 1]).is_err());
        assert!(chunk.0.is_empty());
    }

    #[test]
    fn frozen_output_matches_legacy_without_thawing_or_output_buffering() {
        for range in [None, Some((3, 0)), Some((0, 3)), Some((9, 6))] {
            let mut g = GateNetlist::new("top").unwrap();
            let cell = g
                .add_cell_type("inv", &[("A", PinDir::Input), ("Y", PinDir::Output)])
                .unwrap();
            g.add_port("a", PortDir::Input, range).unwrap();
            g.add_port("y", PortDir::Output, None).unwrap();
            let n = g.add_net(Some(r"u\/wire"), None).unwrap();
            let u = g.add_instance(r"u\/inst", cell).unwrap();
            g.connect(u, "Y", Conn::Net(n)).unwrap();
            g.connect(u, "A", Conn::Const(true)).unwrap();
            g.add_assign(g.find_net("y", None).unwrap(), Conn::Net(n))
                .unwrap();
            let f = g.freeze();
            f.validate().unwrap();
            let plan = Plan::new(&f, Limits::default()).unwrap();
            let mut output = Vec::new();
            plan.write(&mut output).unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                super::super::format_gate_netlist(&g).unwrap()
            );
            assert!(
                Plan::new(
                    &f,
                    Limits {
                        max_aux_bytes: 0,
                        ..Limits::default()
                    }
                )
                .is_err()
            );
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! Bounded, streaming connectivity projection from validated immutable tables.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};

use rusteda_core::names::unescape;
use rusteda_workspace::Error;
use rusteda_workspace::netlist::{ANONYMOUS, CONST0, CONST1, FrozenNetlist, SCALAR, module_entry};

use super::frozen::CellBusSchemas;

#[cfg(test)]
mod tests;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORK: u64 = 10_000_000;
const MAX_PINS: usize = 500_000;

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub aux_bytes: u64,
    pub output_bytes: u64,
    pub work: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            aux_bytes: MAX_BYTES,
            output_bytes: MAX_BYTES,
            work: MAX_WORK,
        }
    }
}
fn input(message: impl Into<String>) -> Error {
    Error::input_rejected(message)
}
fn budget() -> Error {
    input("CONNECTIVITY_BUDGET: entity, auxiliary-memory or work ceiling exceeded")
}
fn string<'f>(f: &'f FrozenNetlist<'_>, id: u32) -> &'f str {
    f.string(id).expect("validated frozen string")
}
fn raw(name: &str) -> Cow<'_, str> {
    if name.contains('\\') {
        Cow::Owned(unescape(name))
    } else {
        Cow::Borrowed(name)
    }
}
fn direction(dir: u8) -> Result<&'static str, Error> {
    match dir {
        0 => Ok("input"),
        1 => Ok("output"),
        2 => Ok("inout"),
        _ => Err(input(
            "CONNECTIVITY_DIRECTION: unsupported internal cell pin direction",
        )),
    }
}
fn tick(remaining: &mut u64, amount: usize) -> Result<(), Error> {
    *remaining = remaining.checked_sub(amount as u64).ok_or_else(budget)?;
    Ok(())
}

// Union by rank, path compression and compact IDs bound alias storage to O(N).
struct Aliases {
    parent: Vec<u32>,
    rank: Vec<u8>,
    minimum: Vec<u32>,
    value: Vec<u8>,
}
impl Aliases {
    fn find(&mut self, start: usize, remaining: &mut u64) -> Result<usize, Error> {
        let mut node = start;
        while self.parent[node] as usize != node {
            tick(remaining, 1)?;
            let next = self.parent[node] as usize;
            self.parent[node] = self.parent[next];
            node = next;
        }
        Ok(node)
    }
    fn new(f: &FrozenNetlist<'_>, remaining: &mut u64) -> Result<Self, Error> {
        let count = f.nets.len() + 2;
        let mut a = Self {
            parent: (0..count as u32).collect(),
            rank: vec![0; count],
            minimum: (0..count as u32).collect(),
            value: vec![0; count],
        };
        a.value[count - 2] = 1;
        a.value[count - 1] = 2;
        for assignment in f.assigns.iter() {
            tick(remaining, 1)?;
            let rhs = match assignment.rhs {
                CONST0 => count - 2,
                CONST1 => count - 1,
                n => n as usize,
            };
            let mut x = a.find(assignment.lhs as usize, remaining)?;
            let mut y = a.find(rhs, remaining)?;
            if x == y {
                continue;
            }
            if a.rank[x] < a.rank[y] {
                std::mem::swap(&mut x, &mut y);
            }
            let value = a.value[x] | a.value[y];
            if value == 3 {
                return Err(input("CONNECTIVITY_ALIAS: contradictory constant drivers"));
            }
            a.parent[y] = x as u32;
            a.minimum[x] = a.minimum[x].min(a.minimum[y]);
            a.value[x] = value;
            if a.rank[x] == a.rank[y] {
                a.rank[x] += 1;
            }
        }
        for id in 0..count {
            tick(remaining, 1)?;
            let root = a.find(id, remaining)?;
            a.parent[id] = root as u32;
        }
        Ok(a)
    }
    fn bit(&self, net: u32) -> Bit {
        match net {
            CONST0 => Bit::Constant("0"),
            CONST1 => Bit::Constant("1"),
            id => {
                let root = self.parent[id as usize] as usize;
                match self.value[root] {
                    1 => Bit::Constant("0"),
                    2 => Bit::Constant("1"),
                    _ => Bit::Net(self.minimum[root] + 2),
                }
            }
        }
    }
}
enum Bit {
    Net(u32),
    Constant(&'static str),
}

struct WireGroup {
    ids: Vec<u32>,
    range: Option<[i32; 2]>,
}
struct Formal {
    name: String,
    pins: Vec<usize>,
    range: Option<[i32; 2]>,
    dir: u8,
}

pub(super) struct Plan<'f, 'a> {
    f: &'f FrozenNetlist<'a>,
    aliases: Aliases,
    groups: BTreeMap<u32, WireGroup>,
    formals: Vec<Vec<Formal>>,
    pub output_limit: u64,
    pub estimated_aux_bytes: u64,
    pub work_used: u64,
}
impl<'f, 'a> Plan<'f, 'a> {
    pub fn new(
        f: &'f FrozenNetlist<'a>,
        schemas: &CellBusSchemas,
        limits: Limits,
    ) -> Result<Self, Error> {
        if limits.aux_bytes == 0
            || limits.aux_bytes > MAX_BYTES
            || limits.output_bytes == 0
            || limits.output_bytes > MAX_BYTES
            || limits.work == 0
            || limits.work > MAX_WORK
            || f.nets.len() > 500_000
            || f.instances.len() > 100_000
            || f.pin_net.len() > MAX_PINS
            || f.cell_pin_name.len() > 65_536
            || f.str_offs.len() > 1_000_001
            || schemas.len() > 256
        {
            return Err(budget());
        }
        let declared = f.instances.iter().try_fold(0usize, |sum, inst| {
            sum.checked_add(f.cell_pins(inst.cell).len())
                .ok_or_else(budget)
        })?;
        if declared > MAX_PINS || f.str_offs.windows(2).any(|w| w[1] - w[0] > 1_048_576) {
            return Err(budget());
        }
        let count = f.nets.len() + f.instances.len() + f.ports.len();
        // Include tree/hash slots, geometric Vec capacity and allocator alignment,
        // not just record payloads. The fixed reserve covers schema and per-cell
        // name/owner maps while immutable port plans are being constructed.
        let aux = (f.nets.len() + 2) as u128 * 16
            + f.nets.len() as u128 * 192
            + f.str_offs.len() as u128 * 8
            + f.cell_pin_name.len() as u128 * 192
            + count.checked_next_power_of_two().unwrap_or(usize::MAX) as u128 * 64
            + f.str_bytes.len() as u128 * 2
            + 8 * 1024 * 1024;
        if aux > u128::from(limits.aux_bytes) {
            return Err(budget());
        }
        let mut remaining = limits.work;
        tick(&mut remaining, f.instances.len() + f.cell_pin_name.len())?;
        let mut taken = HashMap::<Cow<'_, str>, ()>::new();
        let mut declare = |name: &'f str| -> Result<(), Error> {
            if taken.insert(raw(name), ()).is_some() {
                return Err(input("CONNECTIVITY_NAME: colliding raw declaration names"));
            }
            Ok(())
        };
        let mut groups = BTreeMap::<u32, WireGroup>::new();
        for (id, net) in f.nets.iter().enumerate() {
            tick(&mut remaining, 1)?;
            if net.name == ANONYMOUS {
                return Err(input("CONNECTIVITY_NAME: anonymous net unsupported"));
            }
            let group = groups.entry(net.name).or_insert_with(|| WireGroup {
                ids: Vec::new(),
                range: None,
            });
            group.ids.push(id as u32);
        }
        for (&name, group) in &mut groups {
            tick(&mut remaining, group.ids.len())?;
            declare(string(f, name))?;
            let first = f.nets[group.ids[0] as usize].index;
            if first == SCALAR {
                if group.ids.len() != 1 {
                    return Err(input("CONNECTIVITY_RANGE: mixed scalar/vector net"));
                }
            } else {
                let last = f.nets[*group.ids.last().expect("nonempty net group") as usize].index;
                let step = if first <= last { 1i64 } else { -1 };
                if (i64::from(first) - i64::from(last)).unsigned_abs() + 1 != group.ids.len() as u64
                    || group.ids.iter().enumerate().any(|(k, &id)| {
                        i64::from(f.nets[id as usize].index) != i64::from(first) + step * k as i64
                    })
                {
                    return Err(input(
                        "CONNECTIVITY_RANGE: noncontiguous or mixed-order wire bits",
                    ));
                }
                group.range = Some([first, last]);
            }
        }
        for port in f.ports.iter() {
            tick(&mut remaining, 1)?;
            direction(port.dir)?;
            if !groups.contains_key(&port.name) {
                return Err(input("CONNECTIVITY_RANGE: port has no net group"));
            }
        }
        let top = string(f, f.module[module_entry::NAME]);
        for instance in f.instances.iter() {
            declare(string(f, instance.name))?;
            if string(f, f.cells[instance.cell as usize]) == top {
                return Err(input(
                    "CONNECTIVITY_NAME: recursive cell matches top module",
                ));
            }
        }
        let mut formals = Vec::with_capacity(f.cells.len());
        let mut schemas_seen = 0;
        for (cell, &name) in f.cells.iter().enumerate() {
            let range = f.cell_pins(cell as u32);
            let names: HashMap<_, _> = range
                .clone()
                .enumerate()
                .map(|(local, pin)| (string(f, f.cell_pin_name[pin]), local))
                .collect();
            let mut owner = vec![None; range.len()];
            let mut buses = Vec::new();
            if let Some(schema) = schemas.get(string(f, name)) {
                schemas_seen += 1;
                for (port, &[msb, lsb]) in schema {
                    if port.is_empty() || port.len() > 4096 || names.contains_key(port.as_str()) {
                        return Err(input("CELL_BUS_SCHEMA: conflicting or invalid formal bus"));
                    }
                    let width = (i64::from(msb) - i64::from(lsb)).unsigned_abs() + 1;
                    if width > 65_536 || width > range.len() as u64 {
                        return Err(budget());
                    }
                    tick(&mut remaining, width as usize)?;
                    let step = if msb <= lsb { -1i64 } else { 1 };
                    let mut pins = Vec::with_capacity(width as usize);
                    let mut dir = None;
                    for k in 0..width {
                        let key = format!("{port}[{}]", i64::from(lsb) + step * k as i64);
                        let &pin = names.get(key.as_str()).ok_or_else(|| {
                            input(format!(
                                "CELL_BUS_SCHEMA: missing {}.{key}",
                                string(f, name)
                            ))
                        })?;
                        let d = f.cell_pin_dir[range.start + pin];
                        direction(d)?;
                        if dir.is_some_and(|old| old != d)
                            || owner[pin].replace(buses.len()).is_some()
                        {
                            return Err(input(
                                "CELL_BUS_SCHEMA: overlapping or mixed-direction bus",
                            ));
                        }
                        dir = Some(d);
                        pins.push(pin);
                    }
                    buses.push(Some(Formal {
                        name: port.clone(),
                        pins,
                        range: Some([msb, lsb]),
                        dir: dir.expect("nonempty validated bus"),
                    }));
                }
            }
            let mut ports = Vec::new();
            for (local, group) in owner.into_iter().enumerate() {
                if let Some(group) = group {
                    if let Some(port) = buses[group].take() {
                        ports.push(port);
                    }
                } else {
                    let at = range.start + local;
                    direction(f.cell_pin_dir[at])?;
                    ports.push(Formal {
                        name: string(f, f.cell_pin_name[at]).into(),
                        pins: vec![local],
                        range: None,
                        dir: f.cell_pin_dir[at],
                    });
                }
            }
            formals.push(ports);
        }
        if schemas_seen != schemas.len() {
            return Err(input("CELL_BUS_SCHEMA: schema includes an absent cell"));
        }
        tick(&mut remaining, f.pin_net.len() + declared + f.nets.len())?;
        let aliases = Aliases::new(f, &mut remaining)?;
        Ok(Self {
            f,
            aliases,
            groups,
            formals,
            output_limit: limits.output_bytes,
            estimated_aux_bytes: aux as u64,
            work_used: limits.work - remaining,
        })
    }

    fn bit(&self, out: &mut impl Write, net: u32) -> io::Result<()> {
        match self.aliases.bit(net) {
            Bit::Net(id) => write!(out, "{id}"),
            Bit::Constant(value) => json(out, value),
        }
    }
    fn bits(&self, out: &mut impl Write, nets: impl IntoIterator<Item = u32>) -> io::Result<()> {
        out.write_all(b"[")?;
        for (i, id) in nets.into_iter().enumerate() {
            if i != 0 {
                out.write_all(b",")?;
            }
            self.bit(out, id)?;
        }
        out.write_all(b"]")
    }
    fn ports(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(b"{")?;
        for (id, port) in self.f.ports.iter().enumerate() {
            if id != 0 {
                out.write_all(b",")?;
            }
            field(out, &raw(string(self.f, port.name)))?;
            out.write_all(b"{\"direction\":")?;
            json(out, direction(port.dir).expect("validated port direction"))?;
            range(out, (port.is_bus != 0).then_some([port.msb, port.lsb]))?;
            out.write_all(b",\"bits\":")?;
            self.bits(out, self.f.port_bits(id as u32).iter().rev().copied())?;
            out.write_all(b"}")?;
        }
        out.write_all(b"}")
    }
    fn instance(&self, out: &mut impl Write, id: usize) -> io::Result<()> {
        let inst = self.f.instances[id];
        let ports = &self.formals[inst.cell as usize];
        let count = self.f.cell_pins(inst.cell).len();
        let mut nets = vec![None; count];
        for at in self.f.instance_pins(id as u32) {
            nets[self.f.pin_cellpin[at] as usize] = Some(self.f.pin_net[at]);
        }
        out.write_all(b"{\"type\":")?;
        json(out, string(self.f, self.f.cells[inst.cell as usize]))?;
        out.write_all(b",\"port_directions\":{")?;
        for (k, p) in ports.iter().enumerate() {
            if k != 0 {
                out.write_all(b",")?;
            }
            field(out, &p.name)?;
            json(out, direction(p.dir).expect("validated formal direction"))?;
        }
        out.write_all(b"},\"connections\":{")?;
        for (k, p) in ports.iter().enumerate() {
            if k != 0 {
                out.write_all(b",")?;
            }
            field(out, &p.name)?;
            out.write_all(b"[")?;
            for (j, &pin) in p.pins.iter().enumerate() {
                if j != 0 {
                    out.write_all(b",")?;
                }
                if let Some(net) = nets[pin] {
                    self.bit(out, net)?;
                } else {
                    json(out, "x")?;
                }
            }
            out.write_all(b"]")?;
        }
        out.write_all(b"}}")
    }
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(b"{\"creator\":\"RustEDA native connectivity v1\",\"modules\":{")?;
        field(out, string(self.f, self.f.module[module_entry::NAME]))?;
        out.write_all(b"{\"ports\":")?;
        self.ports(out)?;
        out.write_all(b",\"cells\":{")?;
        for (id, inst) in self.f.instances.iter().enumerate() {
            if id != 0 {
                out.write_all(b",")?;
            }
            field(out, &raw(string(self.f, inst.name)))?;
            self.instance(out, id)?;
        }
        out.write_all(b"},\"netnames\":{")?;
        for (k, (&name, group)) in self.groups.iter().enumerate() {
            if k != 0 {
                out.write_all(b",")?;
            }
            field(out, &raw(string(self.f, name)))?;
            out.write_all(b"{\"hide_name\":0")?;
            range(out, group.range)?;
            out.write_all(b",\"bits\":")?;
            self.bits(out, group.ids.iter().rev().copied())?;
            out.write_all(b"}")?;
        }
        out.write_all(b"}}")?;
        for (id, &cell) in self.f.cells.iter().enumerate() {
            out.write_all(b",")?;
            field(out, string(self.f, cell))?;
            out.write_all(b"{\"attributes\":{\"blackbox\":\"1\"},\"ports\":{")?;
            for (k, port) in self.formals[id].iter().enumerate() {
                if k != 0 {
                    out.write_all(b",")?;
                }
                field(out, &port.name)?;
                out.write_all(b"{\"direction\":")?;
                json(
                    out,
                    direction(port.dir).expect("validated formal direction"),
                )?;
                range(out, port.range)?;
                out.write_all(b",\"bits\":[")?;
                for (j, pin) in port.pins.iter().enumerate() {
                    if j != 0 {
                        out.write_all(b",")?;
                    }
                    write!(out, "{}", pin + 2)?;
                }
                out.write_all(b"]}")?;
            }
            out.write_all(b"},\"cells\":{},\"netnames\":{}}")?;
        }
        out.write_all(b"}}\n")
    }
}
fn json(out: &mut impl Write, text: &str) -> io::Result<()> {
    serde_json::to_writer(out, text).map_err(io::Error::other)
}
fn field(out: &mut impl Write, text: &str) -> io::Result<()> {
    json(out, text)?;
    out.write_all(b":")
}
fn range(out: &mut impl Write, range: Option<[i32; 2]>) -> io::Result<()> {
    if let Some([msb, lsb]) = range {
        write!(
            out,
            ",\"offset\":{},\"upto\":{},\"is_bus\":1",
            msb.min(lsb),
            u8::from(msb < lsb)
        )?;
    }
    Ok(())
}

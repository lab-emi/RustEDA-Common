// SPDX-License-Identifier: Apache-2.0
//! Canonical little-endian file framing. Byte views borrow mapped storage.
use crate::{Error, Grid, Limits, Record, Result, Table, View};
pub const HEADER: usize = 4096;
const MAGIC: &[u8; 8] = b"RG1CSR01";
const WIDTHS: [usize; 6] = [4, 4, 96, 4, 48, 40];
use rusteda_compute::geometry::crossings::device_abi::{i64_at, part, put, u32_at, u64_at, zeros};
fn align(n: usize) -> Result<usize> {
    n.checked_add(63)
        .map(|v| v & !63)
        .ok_or(Error::Budget("section alignment"))
}
fn lengths(view: View<'_>) -> [usize; 6] {
    [
        view.offsets.len(),
        view.entries.len(),
        view.instances.len(),
        view.template_offsets.len(),
        view.rectangles.len(),
        view.queries.len(),
    ]
}
fn section_layout(counts: [usize; 6]) -> Result<([(usize, usize); 6], usize)> {
    let mut ranges = [(0, 0); 6];
    let mut end = HEADER;
    for i in 0..6 {
        let start = align(end)?;
        let n = counts[i]
            .checked_mul(WIDTHS[i])
            .ok_or(Error::Budget("section bytes"))?;
        end = start.checked_add(n).ok_or(Error::Budget("section end"))?;
        ranges[i] = (start, end);
    }
    Ok((ranges, end))
}
/// Parse only bounded framing; `admit` validates all semantic/inventory claims.
pub fn decode(bytes: &[u8], limits: Limits) -> Result<View<'_>> {
    limits.validate()?;
    if bytes.len() < HEADER
        || bytes.len() as u64 > limits.max_view_bytes
        || &bytes[..8] != MAGIC
        || u32_at(bytes, 8)? != 1
        || u32_at(bytes, 12)? != HEADER as u32
        || u64_at(bytes, 16)? != bytes.len() as u64
    {
        return Err(Error::Invalid("view framing/version/bytes"));
    }
    zeros(&bytes[160..HEADER])?;
    let mut counts = [0usize; 6];
    let mut starts = [0usize; 6];
    for i in 0..6 {
        starts[i] = usize::try_from(u64_at(bytes, 64 + 16 * i)?)
            .map_err(|_| Error::Budget("section offset"))?;
        counts[i] = usize::try_from(u64_at(bytes, 72 + 16 * i)?)
            .map_err(|_| Error::Budget("section count"))?;
    }
    let (ranges, end) = section_layout(counts)?;
    if end != bytes.len() {
        return Err(Error::Invalid("trailing/missing view bytes"));
    }
    let mut previous = HEADER;
    for i in 0..6 {
        if starts[i] != ranges[i].0 {
            return Err(Error::Invalid("noncanonical/aliased section"));
        }
        zeros(part(bytes, previous, ranges[i].0 - previous)?)?;
        previous = ranges[i].1;
    }
    let section = |i: usize| part(bytes, ranges[i].0, ranges[i].1 - ranges[i].0);
    Ok(View {
        grid: Grid {
            origin: [i64_at(bytes, 32)?, i64_at(bytes, 40)?],
            bin: i64_at(bytes, 48)?,
            width: u32_at(bytes, 56)?,
            height: u32_at(bytes, 60)?,
        },
        layer_count: u32_at(bytes, 24)?,
        virtual_count: u32_at(bytes, 28)? as usize,
        offsets: Table::bytes(section(0)?)?,
        entries: Table::bytes(section(1)?)?,
        instances: Table::bytes(section(2)?)?,
        template_offsets: Table::bytes(section(3)?)?,
        rectangles: Table::bytes(section(4)?)?,
        queries: Table::bytes(section(5)?)?,
    })
}
fn emit<T: Record>(table: Table<'_, T>, bytes: &mut [u8]) -> Result<()> {
    for (i, out) in bytes.chunks_exact_mut(T::BYTES).enumerate() {
        table.get(i)?.encode(out)?;
    }
    Ok(())
}
/// Bounded parent-side serializer. The worker decodes the same view by mmap.
pub fn encode(view: View<'_>, limits: Limits) -> Result<Vec<u8>> {
    crate::admit(view, limits)?;
    let counts = lengths(view);
    let (ranges, end) = section_layout(counts)?;
    if end as u64 > limits.max_view_bytes {
        return Err(Error::Budget("serialized view"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(end)
        .map_err(|_| Error::Budget("view allocation"))?;
    if bytes.capacity() as u64 > limits.max_view_bytes {
        return Err(Error::Budget("actual view capacity"));
    }
    bytes.resize(end, 0);
    put(&mut bytes, 0, MAGIC)?;
    put(&mut bytes, 8, &1u32.to_le_bytes())?;
    put(&mut bytes, 12, &(HEADER as u32).to_le_bytes())?;
    put(&mut bytes, 16, &(end as u64).to_le_bytes())?;
    put(&mut bytes, 24, &view.layer_count.to_le_bytes())?;
    put(
        &mut bytes,
        28,
        &u32::try_from(view.virtual_count)
            .map_err(|_| Error::Budget("virtual count"))?
            .to_le_bytes(),
    )?;
    put(&mut bytes, 32, &view.grid.origin[0].to_le_bytes())?;
    put(&mut bytes, 40, &view.grid.origin[1].to_le_bytes())?;
    put(&mut bytes, 48, &view.grid.bin.to_le_bytes())?;
    put(&mut bytes, 56, &view.grid.width.to_le_bytes())?;
    put(&mut bytes, 60, &view.grid.height.to_le_bytes())?;
    for i in 0..6 {
        put(&mut bytes, 64 + 16 * i, &(ranges[i].0 as u64).to_le_bytes())?;
        put(&mut bytes, 72 + 16 * i, &(counts[i] as u64).to_le_bytes())?;
    }
    emit(view.offsets, &mut bytes[ranges[0].0..ranges[0].1])?;
    emit(view.entries, &mut bytes[ranges[1].0..ranges[1].1])?;
    emit(view.instances, &mut bytes[ranges[2].0..ranges[2].1])?;
    emit(view.template_offsets, &mut bytes[ranges[3].0..ranges[3].1])?;
    emit(view.rectangles, &mut bytes[ranges[4].0..ranges[4].1])?;
    emit(view.queries, &mut bytes[ranges[5].0..ranges[5].1])?;
    Ok(bytes)
}

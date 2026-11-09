// SPDX-License-Identifier: Apache-2.0
//! Strict static/tape v2 framing. Queries remain fixed-endian borrowed records.
use crate::session::SessionLimits;
use crate::{Error, Grid, Query, Record, Result, Table, View, admit};

pub const HEADER_BYTES: usize = 4096;
pub const ACTIVATION_HEADER_BYTES: usize = 256;
fn bytes(raw: &[u8], at: usize, n: usize) -> Result<&[u8]> {
    raw.get(at..at.checked_add(n).ok_or(Error::Budget("frame offset"))?)
        .ok_or(Error::Invalid("frame extent"))
}
fn u32_at(raw: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes(raw, at, 4)?
            .try_into()
            .map_err(|_| Error::Invalid("u32"))?,
    ))
}
fn u64_at(raw: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes(raw, at, 8)?
            .try_into()
            .map_err(|_| Error::Invalid("u64"))?,
    ))
}
fn i64_at(raw: &[u8], at: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(
        bytes(raw, at, 8)?
            .try_into()
            .map_err(|_| Error::Invalid("i64"))?,
    ))
}
fn digest_at(raw: &[u8], at: usize) -> Result<[u8; 32]> {
    bytes(raw, at, 32)?
        .try_into()
        .map_err(|_| Error::Invalid("digest"))
}
fn put(raw: &mut [u8], at: usize, value: &[u8]) -> Result<()> {
    let end = at
        .checked_add(value.len())
        .ok_or(Error::Budget("frame write"))?;
    raw.get_mut(at..end)
        .ok_or(Error::Invalid("frame write extent"))?
        .copy_from_slice(value);
    Ok(())
}
fn zero(raw: &[u8]) -> Result<()> {
    if raw.iter().any(|b| *b != 0) {
        return Err(Error::Invalid("noncanonical reserved bytes"));
    }
    Ok(())
}
pub use rusteda_compute::geometry::crossings::device_abi::canonical_static_bytes;
use rusteda_compute::geometry::crossings::device_abi::layout;

#[derive(Clone, Copy, Debug)]
pub struct DecodedStatic<'view> {
    pub view: View<'view>,
    pub zero_scan_bytes: u64,
}
pub fn decode_static(raw: &[u8], limits: SessionLimits) -> Result<DecodedStatic<'_>> {
    limits.validate()?;
    if raw.len() < HEADER_BYTES
        || raw.len() as u64 > limits.max_static_bytes
        || bytes(raw, 0, 8)? != b"RG1STA02"
        || u32_at(raw, 8)? != 2
        || u32_at(raw, 12)? != HEADER_BYTES as u32
        || u64_at(raw, 16)? != raw.len() as u64
    {
        return Err(Error::Invalid("static framing/version/bytes"));
    }
    zero(&raw[144..HEADER_BYTES])?;
    let mut counts = [0; 5];
    let mut starts = [0; 5];
    for i in 0..5 {
        starts[i] = usize::try_from(u64_at(raw, 64 + 16 * i)?)
            .map_err(|_| Error::Budget("static offset"))?;
        counts[i] = usize::try_from(u64_at(raw, 72 + 16 * i)?)
            .map_err(|_| Error::Budget("static count"))?;
    }
    let (ranges, end) = layout(counts)?;
    if end != raw.len() {
        return Err(Error::Invalid("static missing/trailing bytes"));
    }
    let mut previous = HEADER_BYTES;
    let mut scanned = (HEADER_BYTES - 144) as u64;
    for i in 0..5 {
        if starts[i] != ranges[i].0 {
            return Err(Error::Invalid("static aliased/noncanonical section"));
        }
        zero(bytes(raw, previous, ranges[i].0 - previous)?)?;
        scanned += (ranges[i].0 - previous) as u64;
        previous = ranges[i].1;
    }
    let section = |i: usize| bytes(raw, ranges[i].0, ranges[i].1 - ranges[i].0);
    Ok(DecodedStatic {
        view: View {
            grid: Grid {
                origin: [i64_at(raw, 32)?, i64_at(raw, 40)?],
                bin: i64_at(raw, 48)?,
                width: u32_at(raw, 56)?,
                height: u32_at(raw, 60)?,
            },
            layer_count: u32_at(raw, 24)?,
            virtual_count: u32_at(raw, 28)? as usize,
            offsets: Table::bytes(section(0)?)?,
            entries: Table::bytes(section(1)?)?,
            instances: Table::bytes(section(2)?)?,
            template_offsets: Table::bytes(section(3)?)?,
            rectangles: Table::bytes(section(4)?)?,
            queries: Table::Native(&[]),
        },
        zero_scan_bytes: scanned,
    })
}
fn emit<T: Record>(table: Table<'_, T>, raw: &mut [u8]) -> Result<()> {
    for (i, slot) in raw.chunks_exact_mut(T::BYTES).enumerate() {
        table.get(i)?.encode(slot)?;
    }
    Ok(())
}
/// Bounded fixture/producer encoder. Consumer decoding never clones geometry.
pub fn encode_static(view: View<'_>, limits: SessionLimits) -> Result<Vec<u8>> {
    limits.validate()?;
    if !view.queries.is_empty() {
        return Err(Error::Invalid("static has queries"));
    }
    if canonical_static_bytes(view)? > limits.max_static_bytes {
        return Err(Error::Budget("static serialized bytes"));
    }
    admit(view, limits.core_limits())?;
    let counts = [
        view.offsets.len(),
        view.entries.len(),
        view.instances.len(),
        view.template_offsets.len(),
        view.rectangles.len(),
    ];
    let (ranges, end) = layout(counts)?;
    if end as u64 > limits.max_static_bytes {
        return Err(Error::Budget("static serialized bytes"));
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(end)
        .map_err(|_| Error::Budget("static allocation"))?;
    if raw.capacity() as u64 > limits.max_static_bytes {
        return Err(Error::Budget("static actual capacity"));
    }
    raw.resize(end, 0);
    put(&mut raw, 0, b"RG1STA02")?;
    put(&mut raw, 8, &2u32.to_le_bytes())?;
    put(&mut raw, 12, &(HEADER_BYTES as u32).to_le_bytes())?;
    for (at, n) in [
        (16, end as u64),
        (
            24,
            u64::from(view.layer_count) | ((view.virtual_count as u64) << 32),
        ),
        (32, view.grid.origin[0] as u64),
        (40, view.grid.origin[1] as u64),
        (48, view.grid.bin as u64),
        (
            56,
            u64::from(view.grid.width) | (u64::from(view.grid.height) << 32),
        ),
    ] {
        put(&mut raw, at, &n.to_le_bytes())?;
    }
    for i in 0..5 {
        put(&mut raw, 64 + 16 * i, &(ranges[i].0 as u64).to_le_bytes())?;
        put(&mut raw, 72 + 16 * i, &(counts[i] as u64).to_le_bytes())?;
    }
    emit(view.offsets, &mut raw[ranges[0].0..ranges[0].1])?;
    emit(view.entries, &mut raw[ranges[1].0..ranges[1].1])?;
    emit(view.instances, &mut raw[ranges[2].0..ranges[2].1])?;
    emit(view.template_offsets, &mut raw[ranges[3].0..ranges[3].1])?;
    emit(view.rectangles, &mut raw[ranges[4].0..ranges[4].1])?;
    Ok(raw)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeLayout {
    pub queries: u64,
    pub payload_bytes: u64,
    pub total_bytes: u64,
}
impl TapeLayout {
    /// File metadata alone bounds admission before any large fingerprint read.
    pub fn from_file_bytes(total: u64, limits: SessionLimits) -> Result<Self> {
        limits.validate()?;
        let payload = total
            .checked_sub(2 * HEADER_BYTES as u64)
            .ok_or(Error::Invalid("tape minimum bytes"))?;
        if total > limits.max_tape_bytes || payload % Query::BYTES as u64 != 0 {
            return Err(Error::Invalid("tape extent/stride"));
        }
        let queries = payload / Query::BYTES as u64;
        if queries > limits.max_queries {
            return Err(Error::Budget("tape query count"));
        }
        Ok(Self {
            queries,
            payload_bytes: payload,
            total_bytes: total,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeHeader {
    pub layout: TapeLayout,
    pub static_sha: [u8; 32],
    pub payload_sha: [u8; 32],
}
impl TapeHeader {
    fn valid_layout(layout: TapeLayout) -> Result<()> {
        if TapeLayout::from_file_bytes(layout.total_bytes, SessionLimits::default())? != layout {
            return Err(Error::Invalid("tape layout fields"));
        }
        Ok(())
    }
    pub fn decode(raw: &[u8], expected: TapeLayout) -> Result<Self> {
        Self::valid_layout(expected)?;
        if raw.len() != HEADER_BYTES
            || bytes(raw, 0, 8)? != b"RG1QRY02"
            || u32_at(raw, 8)? != 2
            || u32_at(raw, 12)? != HEADER_BYTES as u32
            || u64_at(raw, 16)? != expected.total_bytes
            || u64_at(raw, 24)? != expected.queries
            || u64_at(raw, 32)? != Query::BYTES as u64
            || u64_at(raw, 40)? != expected.payload_bytes
            || u64_at(raw, 112)? != HEADER_BYTES as u64 + expected.payload_bytes
            || u64_at(raw, 120)? != 0
        {
            return Err(Error::Invalid("tape header framing"));
        }
        zero(&raw[128..])?;
        Ok(Self {
            layout: expected,
            static_sha: digest_at(raw, 48)?,
            payload_sha: digest_at(raw, 80)?,
        })
    }
    pub fn encode(self) -> Result<[u8; HEADER_BYTES]> {
        Self::valid_layout(self.layout)?;
        let mut raw = [0u8; HEADER_BYTES];
        put(&mut raw, 0, b"RG1QRY02")?;
        put(&mut raw, 8, &2u32.to_le_bytes())?;
        put(&mut raw, 12, &(HEADER_BYTES as u32).to_le_bytes())?;
        for (at, n) in [
            (16, self.layout.total_bytes),
            (24, self.layout.queries),
            (32, Query::BYTES as u64),
            (40, self.layout.payload_bytes),
            (112, HEADER_BYTES as u64 + self.layout.payload_bytes),
        ] {
            put(&mut raw, at, &n.to_le_bytes())?;
        }
        put(&mut raw, 48, &self.static_sha)?;
        put(&mut raw, 80, &self.payload_sha)?;
        Ok(raw)
    }
    pub fn validate_footer(
        self,
        raw: &[u8],
        header_sha: [u8; 32],
        actual_payload_sha: [u8; 32],
    ) -> Result<()> {
        Self::valid_layout(self.layout)?;
        if raw.len() != HEADER_BYTES
            || bytes(raw, 0, 8)? != b"RG1END02"
            || u32_at(raw, 8)? != 2
            || u32_at(raw, 12)? != HEADER_BYTES as u32
            || u64_at(raw, 16)? != self.layout.queries
            || u64_at(raw, 24)? != self.layout.payload_bytes
            || digest_at(raw, 32)? != self.payload_sha
            || digest_at(raw, 64)? != header_sha
            || digest_at(raw, 96)? != self.static_sha
            || actual_payload_sha != self.payload_sha
        {
            return Err(Error::Invalid("tape footer/hash/count"));
        }
        zero(&raw[128..])
    }
    pub fn encode_footer(self, header_sha: [u8; 32]) -> Result<[u8; HEADER_BYTES]> {
        Self::valid_layout(self.layout)?;
        let mut raw = [0u8; HEADER_BYTES];
        put(&mut raw, 0, b"RG1END02")?;
        put(&mut raw, 8, &2u32.to_le_bytes())?;
        put(&mut raw, 12, &(HEADER_BYTES as u32).to_le_bytes())?;
        put(&mut raw, 16, &self.layout.queries.to_le_bytes())?;
        put(&mut raw, 24, &self.layout.payload_bytes.to_le_bytes())?;
        put(&mut raw, 32, &self.payload_sha)?;
        put(&mut raw, 64, &header_sha)?;
        put(&mut raw, 96, &self.static_sha)?;
        Ok(raw)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct ActivationHeader {
    pub virtual_shapes: u64,
    pub queries: u64,
    pub words: u64,
    pub success_work: u64,
    pub static_sha: [u8; 32],
    pub tape_sha: [u8; 32],
    pub protocol_sha: [u8; 32],
    pub workers: u32,
    pub prefixes: u64,
}
impl ActivationHeader {
    pub fn encode(self) -> Result<[u8; ACTIVATION_HEADER_BYTES]> {
        if self.virtual_shapes > crate::MAX_VIRTUAL as u64
            || self.queries > crate::session::MAX_STREAM_QUERIES
            || self.words != self.virtual_shapes.div_ceil(64)
            || self.success_work > crate::MAX_WORK
            || !(1..=4).contains(&self.workers)
            || self.prefixes > self.queries
        {
            return Err(Error::Invalid("activation header bounds"));
        }
        let mut raw = [0u8; ACTIVATION_HEADER_BYTES];
        put(&mut raw, 0, b"RG1ACT03")?;
        put(&mut raw, 8, &3u32.to_le_bytes())?;
        put(
            &mut raw,
            12,
            &(ACTIVATION_HEADER_BYTES as u32).to_le_bytes(),
        )?;
        for (at, n) in [
            (16, self.virtual_shapes),
            (24, self.queries),
            (32, self.words),
            (40, self.success_work),
            (152, self.prefixes),
        ] {
            put(&mut raw, at, &n.to_le_bytes())?;
        }
        put(&mut raw, 48, &self.static_sha)?;
        put(&mut raw, 80, &self.tape_sha)?;
        put(&mut raw, 112, &self.protocol_sha)?;
        put(&mut raw, 144, &0u32.to_le_bytes())?;
        put(&mut raw, 148, &self.workers.to_le_bytes())?;
        Ok(raw)
    }
}

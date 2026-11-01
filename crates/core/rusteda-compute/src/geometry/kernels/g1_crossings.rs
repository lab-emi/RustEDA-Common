// SPDX-License-Identifier: Apache-2.0
//! Allocation-free narrow G1 traversal over authenticated fixed-endian bytes.
//!
//! Host canonical CSR/union admission and the complete M-range proof precede
//! every device call. This is a second implementation, not the CPU oracle.
//! Per thread: constant-size scalar/record state, no heap, candidate list or
//! per-query bitset. Estimated register/temporary state is below512B; generated
//! local memory and JIT attributes must independently fit256B/thread before use.

pub const RANGE: u32 = 1;
pub const RECORD: u32 = 2;
pub const QUERY_WORK: u32 = 3;
pub const ACTIVATION: u32 = 4;
pub const ARITHMETIC: u32 = 5;
const M: i64 = (1_i64 << 60) - 1;
type KernelResult<T> = Result<T, u32>;

#[derive(Clone, Copy)]
pub struct Outcome {
    pub work: u64,
    pub status: u32,
}

#[derive(Clone, Copy)]
struct Rect {
    lx: i64,
    ly: i64,
    ux: i64,
    uy: i64,
}
impl Rect {
    #[inline(always)]
    fn positive(self) -> bool {
        self.lx < self.ux && self.ly < self.uy
    }
    #[inline(always)]
    fn overlaps(self, b: Self) -> bool {
        self.lx < b.ux && b.lx < self.ux && self.ly < b.uy && b.ly < self.uy
    }
    #[inline(always)]
    fn expanded(self) -> KernelResult<Self> {
        Ok(Self {
            lx: self.lx.checked_sub(1).ok_or(ARITHMETIC)?,
            ly: self.ly.checked_sub(1).ok_or(ARITHMETIC)?,
            ux: self.ux.checked_add(1).ok_or(ARITHMETIC)?,
            uy: self.uy.checked_add(1).ok_or(ARITHMETIC)?,
        })
    }
}

#[derive(Clone, Copy)]
struct Section {
    at: usize,
    count: usize,
}

#[derive(Clone, Copy)]
struct Static {
    offsets: Section,
    entries: Section,
    instances: Section,
    templates: Section,
    rectangles: Section,
    origin_x: i64,
    origin_y: i64,
    bin: i64,
    width: u32,
    height: u32,
    layers: u32,
    virtual_shapes: u32,
}

#[inline(always)]
fn byte(raw: &[u8], at: usize) -> KernelResult<u8> {
    raw.get(at).copied().ok_or(RECORD)
}
#[inline(always)]
fn u32_at(raw: &[u8], at: usize) -> KernelResult<u32> {
    let end = at.checked_add(4).ok_or(RECORD)?;
    if end > raw.len() {
        return Err(RECORD);
    }
    Ok(u32::from(byte(raw, at)?)
        | (u32::from(byte(raw, at + 1)?) << 8)
        | (u32::from(byte(raw, at + 2)?) << 16)
        | (u32::from(byte(raw, at + 3)?) << 24))
}
#[inline(always)]
fn u64_at(raw: &[u8], at: usize) -> KernelResult<u64> {
    let end = at.checked_add(8).ok_or(RECORD)?;
    if end > raw.len() {
        return Err(RECORD);
    }
    Ok(u64::from(u32_at(raw, at)?) | (u64::from(u32_at(raw, at + 4)?) << 32))
}
#[inline(always)]
fn i64_at(raw: &[u8], at: usize) -> KernelResult<i64> {
    // Fixed two's-complement byte decoding, not a narrowing numerical cast.
    Ok(u64_at(raw, at)? as i64)
}
#[inline(always)]
fn rect_at(raw: &[u8], at: usize) -> KernelResult<Rect> {
    Ok(Rect {
        lx: i64_at(raw, at)?,
        ly: i64_at(raw, at.checked_add(8).ok_or(RECORD)?)?,
        ux: i64_at(raw, at.checked_add(16).ok_or(RECORD)?)?,
        uy: i64_at(raw, at.checked_add(24).ok_or(RECORD)?)?,
    })
}
#[inline(always)]
fn narrow(value: i64) -> KernelResult<()> {
    if value < -M || value > M {
        Err(RANGE)
    } else {
        Ok(())
    }
}
#[inline(always)]
fn index(section: Section, row: usize, width: usize) -> KernelResult<usize> {
    if row >= section.count {
        return Err(RECORD);
    }
    row.checked_mul(width)
        .and_then(|n| section.at.checked_add(n))
        .ok_or(RECORD)
}
#[inline(always)]
fn section(raw: &[u8], number: usize, width: usize, previous: &mut usize) -> KernelResult<Section> {
    let at = usize::try_from(u64_at(raw, 64 + 16 * number)?).map_err(|_| RECORD)?;
    let count = usize::try_from(u64_at(raw, 72 + 16 * number)?).map_err(|_| RECORD)?;
    let aligned = previous.checked_add(63).ok_or(RECORD)? & !63;
    let end = count
        .checked_mul(width)
        .and_then(|n| at.checked_add(n))
        .ok_or(RECORD)?;
    if at != aligned || end > raw.len() {
        return Err(RECORD);
    }
    *previous = end;
    Ok(Section { at, count })
}

impl Static {
    #[inline(always)]
    fn decode(raw: &[u8]) -> KernelResult<Self> {
        if raw.len() < 4096
            || raw.len() > 32 << 20
            || byte(raw, 0)? != b'R'
            || byte(raw, 1)? != b'G'
            || byte(raw, 2)? != b'1'
            || byte(raw, 3)? != b'S'
            || byte(raw, 4)? != b'T'
            || byte(raw, 5)? != b'A'
            || byte(raw, 6)? != b'0'
            || byte(raw, 7)? != b'2'
            || u32_at(raw, 8)? != 2
            || u32_at(raw, 12)? != 4096
            || u64_at(raw, 16)? != raw.len() as u64
        {
            return Err(RECORD);
        }
        let mut end = 4096;
        let offsets = section(raw, 0, 4, &mut end)?;
        let entries = section(raw, 1, 4, &mut end)?;
        let instances = section(raw, 2, 96, &mut end)?;
        let templates = section(raw, 3, 4, &mut end)?;
        let rectangles = section(raw, 4, 48, &mut end)?;
        if end != raw.len() {
            return Err(RECORD);
        }
        let out = Self {
            offsets,
            entries,
            instances,
            templates,
            rectangles,
            origin_x: i64_at(raw, 32)?,
            origin_y: i64_at(raw, 40)?,
            bin: i64_at(raw, 48)?,
            width: u32_at(raw, 56)?,
            height: u32_at(raw, 60)?,
            layers: u32_at(raw, 24)?,
            virtual_shapes: u32_at(raw, 28)?,
        };
        narrow(out.origin_x)?;
        narrow(out.origin_y)?;
        narrow(out.bin)?;
        let bins = u64::from(out.width)
            .checked_mul(u64::from(out.height))
            .ok_or(RECORD)?;
        if out.bin <= 0
            || out.width == 0
            || out.height == 0
            || bins > 1_000_000
            || out.layers == 0
            || out.layers > 32
            || out.virtual_shapes > 1_000_000
            || instances.count > 100_000
            || entries.count > 4_000_000
            || rectangles.count > 16_384
            || templates.count == 0
            || templates.count > 1025
            || offsets.count as u64 != bins + 1
        {
            return Err(RECORD);
        }
        out.origin_x
            .checked_add(i64::from(out.width) - 1)
            .ok_or(ARITHMETIC)?;
        out.origin_y
            .checked_add(i64::from(out.height) - 1)
            .ok_or(ARITHMETIC)?;
        Ok(out)
    }
    #[inline(always)]
    fn span(self, r: Rect) -> KernelResult<Span> {
        Ok(Span {
            x0: floor(r.lx, self.bin)?,
            y0: floor(r.ly, self.bin)?,
            x1: floor(r.ux, self.bin)?,
            y1: floor(r.uy, self.bin)?,
        })
    }
    #[inline(always)]
    fn cell(self, x: i64, y: i64) -> KernelResult<Option<usize>> {
        let last_x = self
            .origin_x
            .checked_add(i64::from(self.width) - 1)
            .ok_or(ARITHMETIC)?;
        let last_y = self
            .origin_y
            .checked_add(i64::from(self.height) - 1)
            .ok_or(ARITHMETIC)?;
        // Check bounds before subtracting; no unchecked cross-origin difference.
        if x < self.origin_x || y < self.origin_y || x > last_x || y > last_y {
            return Ok(None);
        }
        let dx = u64::try_from(x.checked_sub(self.origin_x).ok_or(ARITHMETIC)?)
            .map_err(|_| ARITHMETIC)?;
        let dy = u64::try_from(y.checked_sub(self.origin_y).ok_or(ARITHMETIC)?)
            .map_err(|_| ARITHMETIC)?;
        let cell = dy
            .checked_mul(u64::from(self.width))
            .and_then(|n| n.checked_add(dx))
            .ok_or(ARITHMETIC)?;
        Ok(Some(usize::try_from(cell).map_err(|_| RECORD)?))
    }
}

#[derive(Clone, Copy)]
struct Span {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}
impl Span {
    #[inline(always)]
    fn area(self) -> KernelResult<u64> {
        let dx = self
            .x1
            .checked_sub(self.x0)
            .and_then(|n| n.checked_add(1))
            .ok_or(ARITHMETIC)?;
        let dy = self
            .y1
            .checked_sub(self.y0)
            .and_then(|n| n.checked_add(1))
            .ok_or(ARITHMETIC)?;
        if dx <= 0 || dy <= 0 {
            return Err(RANGE);
        }
        (dx as u64).checked_mul(dy as u64).ok_or(ARITHMETIC)
    }
}
#[inline(always)]
fn floor(value: i64, bin: i64) -> KernelResult<i64> {
    if bin <= 0 {
        return Err(RECORD);
    }
    let q = value.checked_div(bin).ok_or(ARITHMETIC)?;
    let r = value.checked_rem(bin).ok_or(ARITHMETIC)?;
    if r < 0 {
        q.checked_sub(1).ok_or(ARITHMETIC)
    } else {
        Ok(q)
    }
}

struct Work {
    used: u64,
    upper: u64,
}
impl Work {
    #[inline(always)]
    fn charge(&mut self) -> KernelResult<()> {
        let next = self.used.checked_add(1).ok_or(QUERY_WORK)?;
        if next > self.upper {
            return Err(QUERY_WORK);
        }
        self.used = next;
        Ok(())
    }
}

#[inline(always)]
fn world(raw: &[u8], instance: usize, local: Rect) -> KernelResult<Rect> {
    let x = i64_at(raw, instance)?;
    let y = i64_at(raw, instance + 8)?;
    let height = i64_at(raw, instance + 16)?;
    let orientation = u32_at(raw, instance + 64)?;
    narrow(x)?;
    narrow(y)?;
    narrow(height)?;
    if height <= 0 {
        return Err(RECORD);
    }
    narrow(local.lx)?;
    narrow(local.ly)?;
    narrow(local.ux)?;
    narrow(local.uy)?;
    let lx = x.checked_add(local.lx).ok_or(ARITHMETIC)?;
    let ux = x.checked_add(local.ux).ok_or(ARITHMETIC)?;
    let (ly, uy) = if orientation == 0 {
        (
            y.checked_add(local.ly).ok_or(ARITHMETIC)?,
            y.checked_add(local.uy).ok_or(ARITHMETIC)?,
        )
    } else if orientation == 1 {
        let top = y.checked_add(height).ok_or(ARITHMETIC)?;
        (
            top.checked_sub(local.uy).ok_or(ARITHMETIC)?,
            top.checked_sub(local.ly).ok_or(ARITHMETIC)?,
        )
    } else {
        return Err(RECORD);
    };
    Ok(Rect { lx, ly, ux, uy })
}

/// Executes one query with no global error cancellation or private shape bitmap.
/// `mark` must implement aligned native atomic-u64 OR on the shared activation.
#[inline(always)]
pub fn query<F: FnMut(usize, u64) -> KernelResult<()>>(
    raw: &[u8],
    query: &[u8; 40],
    mask: u32,
    max_query_cells: u64,
    activation_words: u64,
    mut mark: F,
) -> Outcome {
    let mut work = Work { used: 0, upper: 0 };
    let result = (|| -> KernelResult<()> {
        let view = Static::decode(raw)?;
        let q = rect_at(query, 0)?;
        let layer = u32_at(query, 32)?;
        narrow(q.lx)?;
        narrow(q.ly)?;
        narrow(q.ux)?;
        narrow(q.uy)?;
        if !q.positive() || layer >= view.layers || u32_at(query, 36)? != 0 {
            return Err(RANGE);
        }
        if activation_words != u64::from(view.virtual_shapes).div_ceil(64) {
            return Err(ACTIVATION);
        }
        let expanded = q.expanded()?;
        let range = view.span(expanded)?;
        let area = range.area()?;
        if max_query_cells == 0 || max_query_cells > 1_000_000 || area > max_query_cells {
            return Err(RANGE);
        }
        work.upper = if mask & (1_u32 << layer) == 0 {
            1
        } else {
            1_u64
                .checked_add(area)
                .and_then(|n| n.checked_add(view.entries.count as u64))
                .and_then(|n| n.checked_add(view.instances.count as u64))
                .and_then(|n| n.checked_add(u64::from(view.virtual_shapes)))
                .ok_or(QUERY_WORK)?
        };
        work.charge()?;
        if mask & (1_u32 << layer) == 0 {
            return Ok(());
        }
        let mut x = range.x0;
        loop {
            let mut y = range.y0;
            loop {
                work.charge()?;
                if let Some(cell) = view.cell(x, y)? {
                    let start = u32_at(raw, index(view.offsets, cell, 4)?)? as usize;
                    let end = u32_at(
                        raw,
                        index(view.offsets, cell.checked_add(1).ok_or(RECORD)?, 4)?,
                    )? as usize;
                    if start > end || end > view.entries.count {
                        return Err(RECORD);
                    }
                    for entry in start..end {
                        work.charge()?;
                        let id = u32_at(raw, index(view.entries, entry, 4)?)? as usize;
                        let instance = index(view.instances, id, 96)?;
                        let bounds = rect_at(raw, instance + 32)?;
                        let span = view.span(bounds)?;
                        if x != range.x0.max(span.x0) || y != range.y0.max(span.y0) {
                            continue;
                        }
                        work.charge()?;
                        if !expanded.overlaps(bounds) {
                            continue;
                        }
                        let template = u32_at(raw, instance + 24)? as usize;
                        let first = u32_at(raw, instance + 28)?;
                        let r0 = u32_at(raw, index(view.templates, template, 4)?)? as usize;
                        let r1 = u32_at(
                            raw,
                            index(view.templates, template.checked_add(1).ok_or(RECORD)?, 4)?,
                        )? as usize;
                        if r0 > r1 || r1 > view.rectangles.count {
                            return Err(RECORD);
                        }
                        for rectangle in r0..r1 {
                            work.charge()?;
                            let at = index(view.rectangles, rectangle, 48)?;
                            if u32_at(raw, at + 32)? == layer
                                && q.overlaps(world(raw, instance, rect_at(raw, at)?)?)
                            {
                                let bit = u64::from(first)
                                    .checked_add(u64::from(u32_at(raw, at + 36)?))
                                    .ok_or(ACTIVATION)?;
                                if bit >= u64::from(view.virtual_shapes) {
                                    return Err(ACTIVATION);
                                }
                                mark((bit / 64) as usize, 1_u64 << (bit % 64))?;
                            }
                        }
                    }
                }
                if y == range.y1 {
                    break;
                }
                y = y.checked_add(1).ok_or(ARITHMETIC)?;
            }
            if x == range.x1 {
                break;
            }
            x = x.checked_add(1).ok_or(ARITHMETIC)?;
        }
        Ok(())
    })();
    Outcome {
        work: work.used,
        status: match result {
            Ok(()) => 0,
            Err(code) => code,
        },
    }
}

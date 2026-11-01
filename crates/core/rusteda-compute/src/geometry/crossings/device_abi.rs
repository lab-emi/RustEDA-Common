// SPDX-License-Identifier: Apache-2.0
//! Canonical record codecs/static byte layout moved unchanged from the adapter.
use super::{Error, Instance, Orientation, Query, Record, Rect, Result, TemplateRect, View};
const HEADER_BYTES: usize = 4096;
const WIDTHS: [usize; 5] = [4, 4, 96, 4, 48];
pub fn part(bytes: &[u8], at: usize, n: usize) -> Result<&[u8]> {
    bytes
        .get(at..at.checked_add(n).ok_or(Error::Budget("field end"))?)
        .ok_or(Error::Invalid("field extent"))
}
pub fn put(bytes: &mut [u8], at: usize, value: &[u8]) -> Result<()> {
    let end = at
        .checked_add(value.len())
        .ok_or(Error::Budget("field end"))?;
    bytes
        .get_mut(at..end)
        .ok_or(Error::Invalid("field extent"))?
        .copy_from_slice(value);
    Ok(())
}
pub fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        part(bytes, at, 4)?
            .try_into()
            .map_err(|_| Error::Invalid("u32"))?,
    ))
}
pub fn u64_at(bytes: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        part(bytes, at, 8)?
            .try_into()
            .map_err(|_| Error::Invalid("u64"))?,
    ))
}
pub fn i64_at(bytes: &[u8], at: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(
        part(bytes, at, 8)?
            .try_into()
            .map_err(|_| Error::Invalid("i64"))?,
    ))
}
fn rect_at(bytes: &[u8], at: usize) -> Result<Rect> {
    Ok(Rect {
        lx: i64_at(bytes, at)?,
        ly: i64_at(bytes, at + 8)?,
        ux: i64_at(bytes, at + 16)?,
        uy: i64_at(bytes, at + 24)?,
    })
}
fn put_rect(bytes: &mut [u8], at: usize, r: Rect) -> Result<()> {
    for (i, v) in [r.lx, r.ly, r.ux, r.uy].into_iter().enumerate() {
        put(bytes, at + 8 * i, &v.to_le_bytes())?;
    }
    Ok(())
}
pub fn zeros(bytes: &[u8]) -> Result<()> {
    if bytes.iter().any(|&b| b != 0) {
        return Err(Error::Invalid("noncanonical reserved bytes"));
    }
    Ok(())
}
fn width(bytes: &[u8], n: usize) -> Result<()> {
    if bytes.len() != n {
        return Err(Error::Invalid("record width"));
    }
    Ok(())
}
impl Record for u32 {
    const BYTES: usize = 4;
    fn decode(bytes: &[u8]) -> Result<Self> {
        width(bytes, 4)?;
        u32_at(bytes, 0)
    }
    fn encode(self, bytes: &mut [u8]) -> Result<()> {
        width(bytes, 4)?;
        put(bytes, 0, &self.to_le_bytes())
    }
}
impl Record for Instance {
    const BYTES: usize = 96;
    fn decode(bytes: &[u8]) -> Result<Self> {
        width(bytes, 96)?;
        zeros(&bytes[68..])?;
        let orientation = match u32_at(bytes, 64)? {
            0 => Orientation::N,
            1 => Orientation::Fs,
            _ => return Err(Error::Invalid("orientation")),
        };
        Ok(Self {
            at: [i64_at(bytes, 0)?, i64_at(bytes, 8)?],
            height: i64_at(bytes, 16)?,
            template: u32_at(bytes, 24)?,
            first: u32_at(bytes, 28)?,
            bounds: rect_at(bytes, 32)?,
            orientation,
        })
    }
    fn encode(self, bytes: &mut [u8]) -> Result<()> {
        width(bytes, 96)?;
        bytes.fill(0);
        put(bytes, 0, &self.at[0].to_le_bytes())?;
        put(bytes, 8, &self.at[1].to_le_bytes())?;
        put(bytes, 16, &self.height.to_le_bytes())?;
        put(bytes, 24, &self.template.to_le_bytes())?;
        put(bytes, 28, &self.first.to_le_bytes())?;
        put_rect(bytes, 32, self.bounds)?;
        put(
            bytes,
            64,
            &match self.orientation {
                Orientation::N => 0u32,
                Orientation::Fs => 1u32,
            }
            .to_le_bytes(),
        )
    }
}
impl Record for TemplateRect {
    const BYTES: usize = 48;
    fn decode(bytes: &[u8]) -> Result<Self> {
        width(bytes, 48)?;
        zeros(&bytes[40..])?;
        Ok(Self {
            rect: rect_at(bytes, 0)?,
            layer: u32_at(bytes, 32)?,
            ordinal: u32_at(bytes, 36)?,
        })
    }
    fn encode(self, bytes: &mut [u8]) -> Result<()> {
        width(bytes, 48)?;
        bytes.fill(0);
        put_rect(bytes, 0, self.rect)?;
        put(bytes, 32, &self.layer.to_le_bytes())?;
        put(bytes, 36, &self.ordinal.to_le_bytes())
    }
}
impl Record for Query {
    const BYTES: usize = 40;
    fn decode(bytes: &[u8]) -> Result<Self> {
        width(bytes, 40)?;
        zeros(&bytes[36..])?;
        Ok(Self {
            rect: rect_at(bytes, 0)?,
            layer: u32_at(bytes, 32)?,
        })
    }
    fn encode(self, bytes: &mut [u8]) -> Result<()> {
        width(bytes, 40)?;
        bytes.fill(0);
        put_rect(bytes, 0, self.rect)?;
        put(bytes, 32, &self.layer.to_le_bytes())
    }
}
fn align(n: usize) -> Result<usize> {
    n.checked_add(63)
        .map(|n| n & !63)
        .ok_or(Error::Budget("static alignment"))
}
pub fn layout(counts: [usize; 5]) -> Result<([(usize, usize); 5], usize)> {
    let mut at = HEADER_BYTES;
    let mut ranges = [(0, 0); 5];
    for i in 0..5 {
        let start = align(at)?;
        at = counts[i]
            .checked_mul(WIDTHS[i])
            .and_then(|n| start.checked_add(n))
            .ok_or(Error::Budget("static section bytes"))?;
        ranges[i] = (start, at);
    }
    Ok((ranges, at))
}

/// Exact five-section byte length; semantic validation is a separate step.
pub fn canonical_static_bytes(view: View<'_>) -> Result<u64> {
    let counts = [
        view.offsets.len(),
        view.entries.len(),
        view.instances.len(),
        view.template_offsets.len(),
        view.rectangles.len(),
    ];
    u64::try_from(layout(counts)?.1).map_err(|_| Error::Budget("static length"))
}

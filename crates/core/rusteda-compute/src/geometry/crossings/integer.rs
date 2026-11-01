// SPDX-License-Identifier: Apache-2.0
//! Additional narrow-device admission; canonical CPU i128 arithmetic is unchanged.
//! All loops are bounded by the already admitted static/window counts and charge
//! the same borrowed whole-session ledger. No second work counter or reset exists.
#![cfg_attr(not(feature = "cuda"), allow(dead_code))]

use super::session::{Budget, MAX_STREAM_QUERIES};
use super::{Error, MAX_INSTANCES, MAX_RECTANGLES, Query, Result, View};

/// Conservative common magnitude bound for device i64 intermediates.
pub const DEVICE_COORDINATE_MAX: i64 = (1_i64 << 60) - 1;

/// Raw scalar extrema, not a physical distance or a wirelength observation.
/// Grid origins are cell indices; other admitted values are integer DBU fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScalarExtrema {
    pub minimum: i64,
    pub maximum: i64,
}

impl ScalarExtrema {
    fn new(value: i64) -> Self {
        Self {
            minimum: value,
            maximum: value,
        }
    }

    fn admit(&mut self, value: i64) -> Result<()> {
        if !(-DEVICE_COORDINATE_MAX..=DEVICE_COORDINATE_MAX).contains(&value) {
            return Err(Error::Invalid("unsupported CUDA scalar range"));
        }
        self.minimum = self.minimum.min(value);
        self.maximum = self.maximum.max(value);
        Ok(())
    }
}

/// Reserved before large reads. Actual row-attempt fees are consumed separately.
/// Eight fixed grid checks, eight per instance, four per rectangle, five per
/// query. These are defined logical fees, not exhaustive instruction counts.
pub(super) fn range_control_upper(queries: u64) -> Result<u64> {
    if queries > MAX_STREAM_QUERIES {
        return Err(Error::Budget("CUDA range query count"));
    }
    8_u64
        .checked_add(8 * MAX_INSTANCES as u64)
        .and_then(|n| n.checked_add(4 * MAX_RECTANGLES as u64))
        .and_then(|n| n.checked_add(5 * queries))
        .ok_or(Error::Budget("CUDA range reservation"))
}

/// Additional static range proof after the canonical complete CSR/union admit.
/// This does not replace or weaken that original semantic validation.
pub(super) fn static_range(view: View<'_>, budget: &mut Budget) -> Result<ScalarExtrema> {
    if !view.queries.is_empty()
        || view.instances.len() > MAX_INSTANCES
        || view.rectangles.len() > MAX_RECTANGLES
    {
        return Err(Error::Budget("CUDA static range counts"));
    }
    budget.control(8)?;
    let grid = view.grid;
    let mut extrema = ScalarExtrema::new(grid.bin);
    extrema.admit(grid.bin)?;
    extrema.admit(grid.origin[0])?;
    extrema.admit(grid.origin[1])?;
    if grid.bin <= 0 || !(1..=32).contains(&view.layer_count) {
        return Err(Error::Invalid("CUDA grid bin/layers"));
    }
    grid.cells()?;
    grid.ends()?;
    for index in 0..view.instances.len() {
        // One attempted instance carries its complete logical eight-field fee,
        // even if decoding/range validation then fails. No retry releases it.
        budget.control(8)?;
        let instance = view.instances.get(index)?;
        for value in [
            instance.at[0],
            instance.at[1],
            instance.height,
            instance.bounds.lx,
            instance.bounds.ly,
            instance.bounds.ux,
            instance.bounds.uy,
        ] {
            extrema.admit(value)?;
        }
        if instance.height <= 0 {
            return Err(Error::Invalid("CUDA instance height"));
        }
        // Canonical admit also allows all-zero bounds for an empty template. That
        // instance owns no CSR entry, so the device never reads them; any other
        // bounds must be positive before the N/FS inequalities rely on them.
        let empty = (
            instance.bounds.lx,
            instance.bounds.ly,
            instance.bounds.ux,
            instance.bounds.uy,
        ) == (0, 0, 0, 0);
        if !instance.bounds.positive() && !empty {
            return Err(Error::Invalid("CUDA instance bounds"));
        }
    }
    for index in 0..view.rectangles.len() {
        budget.control(4)?;
        let rect = view.rectangles.get(index)?.rect;
        for value in [rect.lx, rect.ly, rect.ux, rect.uy] {
            extrema.admit(value)?;
        }
        if !rect.positive() {
            return Err(Error::Invalid("CUDA template rectangle"));
        }
    }
    Ok(extrema)
}

/// One row of the complete pre-context pass; the owning preparation enforces
/// window count/order and retains this same exclusive budget between calls.
pub(super) fn query_one(
    query: Query,
    layer_count: u32,
    budget: &mut Budget,
    extrema: &mut ScalarExtrema,
) -> Result<()> {
    budget.control(5)?;
    for value in [query.rect.lx, query.rect.ly, query.rect.ux, query.rect.uy] {
        extrema.admit(value)?;
    }
    if query.layer >= layer_count || !query.rect.positive() {
        return Err(Error::Invalid("CUDA query rectangle/layer"));
    }
    Ok(())
}

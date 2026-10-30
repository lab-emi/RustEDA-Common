// SPDX-License-Identifier: Apache-2.0
//! Allocation-free integer predicate, compiled by Rust and cuda-oxide.

#[derive(Clone, Copy)]
pub struct Shapes<'a> {
    /// lx, ly, ux, uy columns, each `layers.len()` elements.
    pub bounds: &'a [i64],
    pub layers: &'a [u32],
}

#[derive(Clone, Copy)]
pub struct Queries<'a> {
    /// lx, ly, ux, uy columns, each `layers.len()` elements.
    pub bounds: &'a [i64],
    pub layers: &'a [u32],
    /// Packed shape index, or u32::MAX when there is no own shape.
    pub excluded_shape_index: &'a [u32],
}

/// Zero/one acceptance. Host validation proves all SoA lengths and indices.
/// Only comparisons are used, so extreme coordinates cannot overflow.
pub fn hit(shapes: Shapes<'_>, queries: Queries<'_>, query: u32, shape: u32) -> u32 {
    let q = query as usize;
    let s = shape as usize;
    let nq = queries.layers.len();
    let ns = shapes.layers.len();
    if q >= nq || s >= ns {
        return 0;
    }
    if queries.excluded_shape_index[q] == shape || queries.layers[q] != shapes.layers[s] {
        return 0;
    }
    if shapes.bounds[s] < queries.bounds[2 * nq + q]
        && queries.bounds[q] < shapes.bounds[2 * ns + s]
        && shapes.bounds[ns + s] < queries.bounds[3 * nq + q]
        && queries.bounds[nq + q] < shapes.bounds[3 * ns + s]
    {
        1
    } else {
        0
    }
}

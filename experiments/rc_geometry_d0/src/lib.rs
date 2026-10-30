// SPDX-License-Identifier: Apache-2.0
//! D0 source-only bounded integer geometry experiment; not a production backend.
#[cfg(feature = "cuda")]
pub mod cuda;
#[path = "../../../crates/core/rusteda-compute/src/geometry/kernels/rect_pairs.rs"]
pub mod predicate;

pub use predicate::{Queries, Shapes};
use rayon::prelude::*;

#[derive(Clone, Copy)]
pub struct Batch<'a> {
    pub shapes: Shapes<'a>,
    pub queries: Queries<'a>,
    /// Interleaved query_index, shape_index pairs. Order is preserved.
    pub pairs: &'a [u32],
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_shapes: usize,
    pub max_queries: usize,
    pub max_pairs: usize,
    pub max_candidates_per_query: usize,
    pub max_host_bytes: u64,
    pub max_device_bytes: u64,
    pub max_threads: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_shapes: 4_000_000,
            max_queries: 65_536,
            max_pairs: 8_000_000,
            max_candidates_per_query: 65_536,
            max_host_bytes: 1 << 30,
            max_device_bytes: 512 << 20,
            max_threads: 2,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Budget(&'static str),
    Device(String),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    pub host_bytes: u64,
    pub device_bytes: u64,
}

pub fn estimate(shapes: usize, queries: usize, pairs: usize) -> Result<Estimate, Error> {
    let input = (shapes as u64)
        .checked_mul(36)
        .and_then(|n| n.checked_add((queries as u64).checked_mul(40)?))
        .and_then(|n| n.checked_add((pairs as u64).checked_mul(8)?))
        .ok_or(Error::Budget("input byte estimate overflow"))?;
    let flags = (pairs as u64)
        .checked_mul(4)
        .ok_or(Error::Budget("flag byte estimate overflow"))?;
    Ok(Estimate {
        host_bytes: input
            .checked_mul(2)
            .and_then(|n| n.checked_add(flags.checked_mul(2)?))
            .and_then(|n| n.checked_add(256 << 20))
            .ok_or(Error::Budget("host byte estimate overflow"))?,
        device_bytes: input
            .checked_add(flags)
            .ok_or(Error::Budget("device byte estimate overflow"))?,
    })
}

fn positive_boxes(bounds: &[i64], count: usize) -> bool {
    count.checked_mul(4) == Some(bounds.len())
        && (0..count)
            .all(|i| bounds[i] < bounds[2 * count + i] && bounds[count + i] < bounds[3 * count + i])
}

pub fn validate(batch: Batch<'_>, limits: Limits) -> Result<Estimate, Error> {
    let ns = batch.shapes.layers.len();
    let nq = batch.queries.layers.len();
    let np = batch.pairs.len() / 2;
    if limits.max_shapes > 4_000_000
        || limits.max_queries > 65_536
        || limits.max_pairs > 8_000_000
        || limits.max_candidates_per_query > 65_536
        || limits.max_candidates_per_query == 0
        || !(1..=4).contains(&limits.max_threads)
        || limits.max_host_bytes > 1 << 30
        || limits.max_device_bytes > 512 << 20
        || ns > limits.max_shapes
        || nq > limits.max_queries
        || np > limits.max_pairs
    {
        return Err(Error::Budget("hard object or memory bounds"));
    }
    let bytes = estimate(ns, nq, np)?;
    if bytes.host_bytes > limits.max_host_bytes || bytes.device_bytes > limits.max_device_bytes {
        return Err(Error::Budget("admitted host/device bytes"));
    }
    if !batch.pairs.len().is_multiple_of(2)
        || batch.queries.excluded_shape_index.len() != nq
        || !positive_boxes(batch.shapes.bounds, ns)
        || !positive_boxes(batch.queries.bounds, nq)
    {
        return Err(Error::Invalid("SoA dimensions or nonpositive rectangle"));
    }
    if batch
        .queries
        .excluded_shape_index
        .iter()
        .any(|&s| s != u32::MAX && s as usize >= ns)
    {
        return Err(Error::Invalid("excluded shape index"));
    }
    let mut counts = Vec::new();
    counts
        .try_reserve_exact(nq)
        .map_err(|_| Error::Budget("query-count allocation"))?;
    counts.resize(nq, 0usize);
    for pair in batch.pairs.chunks_exact(2) {
        if pair[0] as usize >= nq || pair[1] as usize >= ns {
            return Err(Error::Invalid("candidate pair index"));
        }
        let count = &mut counts[pair[0] as usize];
        *count += 1;
        if *count > limits.max_candidates_per_query {
            return Err(Error::Budget("candidates per query"));
        }
    }
    Ok(bytes)
}

/// Deterministic output order; immutable input is shared by Rayon workers.
pub fn cpu_ref(batch: Batch<'_>, limits: Limits) -> Result<Vec<u32>, Error> {
    validate(batch, limits)?;
    let mut flags = Vec::new();
    flags
        .try_reserve_exact(batch.pairs.len() / 2)
        .map_err(|_| Error::Budget("output allocation"))?;
    flags.resize(batch.pairs.len() / 2, 0);
    let workers = rayon::ThreadPoolBuilder::new()
        .num_threads(limits.max_threads)
        .build()
        .map_err(|_| Error::Budget("bounded CPU worker pool"))?;
    workers.install(|| {
        batch
            .pairs
            .par_chunks_exact(2)
            .zip(flags.par_iter_mut())
            .for_each(|(pair, flag)| {
                *flag = predicate::hit(batch.shapes, batch.queries, pair[0], pair[1]);
            });
    });
    Ok(flags)
}

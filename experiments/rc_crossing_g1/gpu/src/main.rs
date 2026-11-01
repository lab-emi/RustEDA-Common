// SPDX-License-Identifier: Apache-2.0
//! Isolated Rust/PTX G1 image. The ABI probe remains a separate frozen crate.
//! No host entry here opens a device or selects this image automatically.
#![allow(unsafe_code)]
use core::sync::atomic::{AtomicU64, Ordering};
use cuda_device::{kernel, thread};
use cuda_host::cuda_module;

#[path = "../../../../crates/core/rusteda-compute/src/geometry/kernels/g1_crossings.rs"]
mod traversal;

const MAX_TILE: usize = 65_536;
const MAX_QUERIES: u64 = 12_000_000;
const MAX_WORDS: u64 = 1_000_000_u64.div_ceil(64);

#[cuda_module]
mod kernels {
    use super::*;

    /// Ten independently inspected 64-bit PTX parameters (80 bytes):
    /// S pointer/bytes, query pointer/record count, first global ordinal, layer
    /// mask, cell cap, outcome pointer, activation pointer, activation words.
    /// Slice values are never passed as implicit host Rust ABI aggregates.
    #[kernel]
    pub fn rusteda_g1_crossings_v1(
        static_image: &[u8],
        queries: &[[u8; 40]],
        global_first: u64,
        layer_mask: u64,
        max_query_cells: u64,
        outcomes: *mut [u8; 16],
        activation: *mut u64,
        activation_words: u64,
    ) {
        let local = thread::index_1d().get();
        if local >= queries.len() {
            return;
        }
        let outcome = if queries.len() > MAX_TILE
            || global_first > MAX_QUERIES
            || queries.len() as u64 > MAX_QUERIES - global_first
            || layer_mask > u32::MAX as u64
            || max_query_cells == 0
            || max_query_cells > 1_000_000
            || activation_words > MAX_WORDS
        {
            traversal::Outcome {
                work: 0,
                status: traversal::RANGE,
            }
        } else {
            traversal::query(
                static_image,
                &queries[local],
                layer_mask as u32,
                max_query_cells,
                activation_words,
                |word, mask| {
                    if word as u64 >= activation_words {
                        return Err(traversal::ACTIVATION);
                    }
                    // SAFETY: the separately authenticated host validates the
                    // aligned allocation and its entire lifetime before this
                    // launch. The decoded bit is within the declared extent.
                    // Every concurrent writer uses the same native u64 atomic;
                    // the host reads only after stream synchronization.
                    unsafe {
                        AtomicU64::from_ptr(activation.add(word)).fetch_or(mask, Ordering::Relaxed);
                    }
                    Ok(())
                },
            )
        };
        let mut row = [0_u8; 16];
        row[..8].copy_from_slice(&outcome.work.to_le_bytes());
        row[8..12].copy_from_slice(&outcome.status.to_le_bytes());
        row[12..].copy_from_slice(&(local as u32).to_le_bytes());
        // SAFETY: host outcome storage has one 16-byte slot per admitted query,
        // and only this local thread writes this slot. Padded threads returned
        // before any access; no thread can claim another thread's outcome.
        unsafe { outcomes.add(local).write(row) };
    }
}

fn main() {}

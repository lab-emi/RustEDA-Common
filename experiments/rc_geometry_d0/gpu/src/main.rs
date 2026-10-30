//! Source-only D0 GPU entrypoint; generation and execution await the CPU gate.
use cuda_device::{kernel, thread};
use cuda_host::cuda_module;

#[path = "../../../../crates/core/rusteda-compute/src/geometry/kernels/rect_pairs.rs"]
mod predicate;

#[cuda_module]
mod kernels {
    use super::*;

    #[kernel]
    pub fn rc_geometry_flags(
        shape_bounds: &[i64],
        shape_layers: &[u32],
        query_bounds: &[i64],
        query_layers: &[u32],
        excluded_shape_index: &[u32],
        pairs: &[u32],
        out: *mut u32,
    ) {
        let i = thread::index_1d().get();
        if i >= pairs.len() / 2 {
            return;
        }
        let value = predicate::hit(
            predicate::Shapes {
                bounds: shape_bounds,
                layers: shape_layers,
            },
            predicate::Queries {
                bounds: query_bounds,
                layers: query_layers,
                excluded_shape_index,
            },
            pairs[2 * i],
            pairs[2 * i + 1],
        );
        // SAFETY: the validated host allocates one output word per pair; each
        // CUDA thread writes only its own in-range output index.
        unsafe {
            *out.add(i) = value;
        }
    }
}

fn main() {}

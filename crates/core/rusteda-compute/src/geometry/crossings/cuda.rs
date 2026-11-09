// SPDX-License-Identifier: Apache-2.0
//! Explicit prebuilt-PTX component, never an automatic CPU fallback.
//! No old source or CPU execution receipt qualifies this new implementation.
pub use super::session::cuda_backend::{
    CudaActivation, CudaError, CudaFailure, CudaPreparation, CudaReady, CudaResult, CudaSession,
    CudaStats, RangeProof, control_reservation, io_reservation,
};

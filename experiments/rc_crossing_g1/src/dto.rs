// SPDX-License-Identifier: Apache-2.0
//! Adapter-owned serde DTOs over plain shared CPU types.
use crate::session::{BudgetSnapshot, SessionLimits, SessionStats};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLimitsDto {
    pub max_work: u64,
    pub max_threads: usize,
    pub max_static_bytes: u64,
    pub max_tape_bytes: u64,
    pub max_queries: u64,
    pub max_tile_queries: usize,
    pub max_aux_bytes: u64,
    pub max_output_bytes: u64,
    pub max_query_cells: u64,
}
impl From<SessionLimits> for SessionLimitsDto {
    fn from(value: SessionLimits) -> Self {
        Self {
            max_work: value.max_work,
            max_threads: value.max_threads,
            max_static_bytes: value.max_static_bytes,
            max_tape_bytes: value.max_tape_bytes,
            max_queries: value.max_queries,
            max_tile_queries: value.max_tile_queries,
            max_aux_bytes: value.max_aux_bytes,
            max_output_bytes: value.max_output_bytes,
            max_query_cells: value.max_query_cells,
        }
    }
}
impl From<SessionLimitsDto> for SessionLimits {
    fn from(value: SessionLimitsDto) -> Self {
        Self {
            max_work: value.max_work,
            max_threads: value.max_threads,
            max_static_bytes: value.max_static_bytes,
            max_tape_bytes: value.max_tape_bytes,
            max_queries: value.max_queries,
            max_tile_queries: value.max_tile_queries,
            max_aux_bytes: value.max_aux_bytes,
            max_output_bytes: value.max_output_bytes,
            max_query_cells: value.max_query_cells,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct BudgetSnapshotDto {
    pub cap: u64,
    pub completed: u64,
    pub io_completed: u64,
    pub control_completed: u64,
    pub query_completed: u64,
    pub io_held: u64,
    pub control_held: u64,
    pub query_held: u64,
    pub initial_io_reservation: u64,
    pub initial_control_reservation: u64,
    pub fixed_events: u64,
    pub static_started: bool,
    pub publishing: bool,
}
impl From<BudgetSnapshot> for BudgetSnapshotDto {
    fn from(value: BudgetSnapshot) -> Self {
        Self {
            cap: value.cap,
            completed: value.completed,
            io_completed: value.io_completed,
            control_completed: value.control_completed,
            query_completed: value.query_completed,
            io_held: value.io_held,
            control_held: value.control_held,
            query_held: value.query_held,
            initial_io_reservation: value.initial_io_reservation,
            initial_control_reservation: value.initial_control_reservation,
            fixed_events: value.fixed_events,
            static_started: value.static_started,
            publishing: value.publishing,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct SessionStatsDto {
    pub queries_staged: u64,
    pub queries_completed: u64,
    pub stage_windows: u64,
    pub executed_prefixes: u64,
    pub query_work: u64,
    pub static_validation_work: u64,
    pub query_validation_work: u64,
    pub outcome_initialization_work: u64,
    pub ordered_aggregation_work: u64,
    pub bitset_work: u64,
    pub active_shapes: usize,
    pub virtual_shapes: usize,
    pub workers: usize,
    pub aux_bytes: u64,
    pub actual_static_bytes: u64,
    pub legacy_admission_bytes: u64,
}
impl From<SessionStats> for SessionStatsDto {
    fn from(value: SessionStats) -> Self {
        Self {
            queries_staged: value.queries_staged,
            queries_completed: value.queries_completed,
            stage_windows: value.stage_windows,
            executed_prefixes: value.executed_prefixes,
            query_work: value.query_work,
            static_validation_work: value.static_validation_work,
            query_validation_work: value.query_validation_work,
            outcome_initialization_work: value.outcome_initialization_work,
            ordered_aggregation_work: value.ordered_aggregation_work,
            bitset_work: value.bitset_work,
            active_shapes: value.active_shapes,
            virtual_shapes: value.virtual_shapes,
            workers: value.workers,
            aux_bytes: value.aux_bytes,
            actual_static_bytes: value.actual_static_bytes,
            legacy_admission_bytes: value.legacy_admission_bytes,
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Foreground CPU stream with one global reservation and one worker pool.
//! No production dispatch, device runtime or geometric relaxation.
use super::{
    Admission, Error, Limits, MAX_AUX_BYTES, MAX_ENTRIES, MAX_GRID_CELLS, MAX_INSTANCES,
    MAX_OUTPUT_BYTES, MAX_QUERIES, MAX_RECTANGLES, MAX_TEMPLATES, MAX_VIRTUAL, MAX_WORK, Query,
    QueryOutcome, Result, Table, View, Worker, admit, filled, query_bound, query_measured,
};
use rayon::prelude::*;
use std::fmt;

pub const MAX_STATIC_BYTES: u64 = 32 << 20;
pub const MAX_TAPE_BYTES: u64 = 512 << 20;
pub const MAX_STREAM_QUERIES: u64 = 12_000_000;
pub const FIXED_CONTROL_RESERVE: u64 = 64;
const STATIC_WORK_UPPER: u64 = 1
    + 3 * MAX_INSTANCES as u64
    + 2 * MAX_TEMPLATES as u64
    + MAX_RECTANGLES as u64
    + MAX_GRID_CELLS as u64
    + MAX_ENTRIES as u64;

#[derive(Clone, Copy, Debug)]
pub struct SessionLimits {
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
impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_work: MAX_WORK,
            max_threads: 2,
            max_static_bytes: MAX_STATIC_BYTES,
            max_tape_bytes: MAX_TAPE_BYTES,
            max_queries: MAX_STREAM_QUERIES,
            max_tile_queries: MAX_QUERIES,
            max_aux_bytes: MAX_AUX_BYTES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_query_cells: MAX_GRID_CELLS as u64,
        }
    }
}
impl SessionLimits {
    pub fn validate(self) -> Result<()> {
        if self.max_work > MAX_WORK
            || !(1..=4).contains(&self.max_threads)
            || !(4096..=MAX_STATIC_BYTES).contains(&self.max_static_bytes)
            || !(8192..=MAX_TAPE_BYTES).contains(&self.max_tape_bytes)
            || self.max_queries > MAX_STREAM_QUERIES
            || !(1..=MAX_QUERIES).contains(&self.max_tile_queries)
            || !(1..=MAX_AUX_BYTES).contains(&self.max_aux_bytes)
            || !(128..=MAX_OUTPUT_BYTES).contains(&self.max_output_bytes)
            || !(1..=MAX_GRID_CELLS as u64).contains(&self.max_query_cells)
        {
            return Err(Error::Budget("session hard limits"));
        }
        Ok(())
    }
    pub fn core_limits(self) -> Limits {
        Limits {
            max_work: MAX_WORK,
            max_threads: self.max_threads,
            // This is only the legacy validator's pessimistic allowance.
            // The actual five-section file/layout cap is checked separately.
            max_view_bytes: self
                .max_static_bytes
                .saturating_add(6 * 63)
                .min(super::MAX_VIEW_BYTES),
            max_output_bytes: self.max_output_bytes,
            max_aux_bytes: self.max_aux_bytes,
            max_query_cells: self.max_query_cells,
        }
    }
}

/// Whole-session work. Held amounts are reservations, never completed work.
#[derive(Debug)]
pub struct Budget {
    cap: u64,
    completed: u64,
    io_completed: u64,
    control_completed: u64,
    query_completed: u64,
    io_held: u64,
    control_held: u64,
    query_held: u64,
    initial_io_reservation: u64,
    initial_control_reservation: u64,
    fixed_events: u64,
    static_started: bool,
    publishing: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct BudgetSnapshot {
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
impl Budget {
    pub fn control_reservation(queries: u64, workers: usize) -> Result<u64> {
        if queries > MAX_STREAM_QUERIES || !(1..=4).contains(&workers) {
            return Err(Error::Budget("session reservation objects"));
        }
        STATIC_WORK_UPPER
            .checked_add(5 * queries)
            .and_then(|n| n.checked_add(2 * MAX_VIRTUAL.div_ceil(64) as u64 * workers as u64))
            .and_then(|n| n.checked_add(FIXED_CONTROL_RESERVE))
            .ok_or(Error::Budget("control reservation overflow"))
    }
    pub fn new(cap: u64, bootstrap: u64, io: u64, control: u64) -> Result<Self> {
        let total = bootstrap
            .checked_add(io)
            .and_then(|n| n.checked_add(control))
            .ok_or(Error::Budget("initial work overflow"))?;
        if cap > MAX_WORK || total > cap {
            return Err(Error::Budget("before large IO"));
        }
        Ok(Self {
            cap,
            completed: bootstrap,
            io_completed: bootstrap,
            control_completed: 0,
            query_completed: 0,
            io_held: io,
            control_held: control,
            query_held: 0,
            initial_io_reservation: bootstrap + io,
            initial_control_reservation: control,
            fixed_events: 0,
            static_started: false,
            publishing: false,
        })
    }
    fn consume(held: &mut u64, completed: &mut u64, n: u64) -> Result<()> {
        *held = held
            .checked_sub(n)
            .ok_or(Error::Budget("reservation exhausted"))?;
        *completed = completed
            .checked_add(n)
            .ok_or(Error::Budget("work overflow"))?;
        Ok(())
    }
    pub fn io(&mut self, n: u64) -> Result<()> {
        Self::consume(&mut self.io_held, &mut self.io_completed, n)?;
        self.completed = self
            .completed
            .checked_add(n)
            .ok_or(Error::Budget("work overflow"))?;
        self.check()
    }
    fn control(&mut self, n: u64) -> Result<()> {
        Self::consume(&mut self.control_held, &mut self.control_completed, n)?;
        self.completed = self
            .completed
            .checked_add(n)
            .ok_or(Error::Budget("work overflow"))?;
        self.check()
    }
    pub fn fixed_event(&mut self) -> Result<()> {
        self.fixed_events = self
            .fixed_events
            .checked_add(1)
            .filter(|n| *n <= FIXED_CONTROL_RESERVE)
            .ok_or(Error::Budget("fixed control events"))?;
        self.control(1)
    }
    fn release_control(&mut self, n: u64) -> Result<()> {
        self.control_held = self
            .control_held
            .checked_sub(n)
            .ok_or(Error::Budget("control release"))?;
        Ok(())
    }
    pub fn available_query_work(&self) -> Result<u64> {
        self.cap
            .checked_sub(self.completed)
            .and_then(|n| n.checked_sub(self.io_held))
            .and_then(|n| n.checked_sub(self.control_held))
            .and_then(|n| n.checked_sub(self.query_held))
            .ok_or(Error::Budget("global work invariant"))
    }
    fn start_queries(&mut self, n: u64) -> Result<()> {
        if self.publishing || self.query_held != 0 || n > self.available_query_work()? {
            return Err(Error::Budget("unlaunched query prefix"));
        }
        self.query_held = n;
        self.check()
    }
    fn finish_queries(&mut self, n: u64) -> Result<()> {
        if n > self.query_held {
            return Err(Error::Budget("query reservation exceeded"));
        }
        self.query_held = 0;
        self.query_completed = self
            .query_completed
            .checked_add(n)
            .ok_or(Error::Budget("query total"))?;
        self.completed = self
            .completed
            .checked_add(n)
            .ok_or(Error::Budget("work overflow"))?;
        self.check()
    }
    fn check(&self) -> Result<()> {
        let all = self
            .completed
            .checked_add(self.io_held)
            .and_then(|n| n.checked_add(self.control_held))
            .and_then(|n| n.checked_add(self.query_held))
            .ok_or(Error::Budget("global work overflow"))?;
        if all > self.cap {
            return Err(Error::Budget("global work cap"));
        }
        Ok(())
    }
    /// Called only after all input/footer checks and complete activation.
    /// Shrinks conservative unused allowances to the exact publication tail.
    pub fn publication_promise(&mut self, io_tail: u64, fixed_tail: u64) -> Result<u64> {
        if self.publishing
            || self.control_held
                != FIXED_CONTROL_RESERVE
                    .checked_sub(self.fixed_events)
                    .ok_or(Error::Budget("fixed control balance"))?
            || self.query_held != 0
            || io_tail > self.io_held
            || fixed_tail > self.control_held
            || self.fixed_events + fixed_tail > FIXED_CONTROL_RESERVE
        {
            return Err(Error::Budget("publication tail"));
        }
        self.io_held = io_tail;
        self.control_held = fixed_tail;
        self.publishing = true;
        self.completed
            .checked_add(io_tail)
            .and_then(|n| n.checked_add(fixed_tail))
            .filter(|n| *n <= self.cap)
            .ok_or(Error::Budget("success work"))
    }
    /// Read-only snapshot; does not release, clone or replace the ledger.
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            cap: self.cap,
            completed: self.completed,
            io_completed: self.io_completed,
            control_completed: self.control_completed,
            query_completed: self.query_completed,
            io_held: self.io_held,
            control_held: self.control_held,
            query_held: self.query_held,
            initial_io_reservation: self.initial_io_reservation,
            initial_control_reservation: self.initial_control_reservation,
            fixed_events: self.fixed_events,
            static_started: self.static_started,
            publishing: self.publishing,
        }
    }
    pub fn completed(&self) -> u64 {
        self.completed
    }
    pub fn io_completed(&self) -> u64 {
        self.io_completed
    }
    pub fn complete_success(&self, promised: u64) -> Result<()> {
        if !self.publishing
            || self.completed != promised
            || self.io_held != 0
            || self.control_held != 0
            || self.query_held != 0
        {
            return Err(Error::Budget("successful completion ledger"));
        }
        self.check()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Static,
    Window,
    Query,
    Finish,
}
#[derive(Clone, Copy, Debug)]
pub struct Failure {
    pub stage: Stage,
    pub query_ordinal: Option<u64>,
    pub error: Error,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}:{:?}:{}",
            self.stage, self.query_ordinal, self.error
        )
    }
}
impl std::error::Error for Failure {}
fn at(stage: Stage, ordinal: Option<u64>, error: Error) -> Failure {
    Failure {
        stage,
        query_ordinal: ordinal,
        error,
    }
}
pub type SessionResult<T> = std::result::Result<T, Failure>;

#[derive(Debug)]
struct StaticState<'view> {
    view: View<'view>,
    admission: Admission,
    actual_static_bytes: u64,
}
impl StaticState<'_> {
    fn virtual_count(&self) -> usize {
        self.view.virtual_count
    }
    fn upper(&self, query: Query, limits: SessionLimits) -> Result<u64> {
        query_bound(
            self.view,
            query,
            self.admission.layer_mask,
            limits.max_query_cells,
        )
    }
}

/// Borrowed geometry and the exclusive ledger used to validate it.
/// Moving this value into [`Session`] preserves that same ledger borrow until
/// the session is finished or dropped. Transport IO during the session uses
/// [`Session::io`], rather than accepting a replacement ledger per window.
///
/// ```compile_fail
/// use rusteda_compute::geometry::crossings::{View, session::{Budget, PreparedStatic, Session, SessionLimits}};
/// fn cannot_replace(view: View<'_>, limits: SessionLimits, budget: &mut Budget) {
///     let prepared = PreparedStatic::new(view, limits, budget).unwrap();
///     budget.io(0).unwrap(); // Still exclusively borrowed by prepared.
///     let _ = Session::new(prepared, limits, 0);
/// }
/// ```
#[derive(Debug)]
pub struct PreparedStatic<'view, 'budget> {
    state: StaticState<'view>,
    budget: &'budget mut Budget,
}
impl<'view, 'budget> PreparedStatic<'view, 'budget> {
    pub fn new(
        view: View<'view>,
        limits: SessionLimits,
        budget: &'budget mut Budget,
    ) -> SessionResult<Self> {
        limits.validate().map_err(|e| at(Stage::Static, None, e))?;
        if budget.publishing
            || budget.static_started
            || budget.cap != limits.max_work
            || budget.control_held < STATIC_WORK_UPPER
        {
            return Err(at(
                Stage::Static,
                None,
                Error::Budget("static budget identity"),
            ));
        }
        if !view.queries.is_empty() {
            return Err(at(
                Stage::Static,
                None,
                Error::Invalid("static has queries"),
            ));
        }
        let actual_static_bytes = super::device_abi::canonical_static_bytes(view)
            .map_err(|e| at(Stage::Static, None, e))?;
        if actual_static_bytes > limits.max_static_bytes {
            return Err(at(
                Stage::Static,
                None,
                Error::Budget("actual static bytes"),
            ));
        }
        // A failed/dropped preparation cannot repeat a large semantic walk with
        // the same reserved allowance. No retry/reset path exists in a session.
        budget.static_started = true;
        let admission =
            admit(view, limits.core_limits()).map_err(|e| at(Stage::Static, None, e))?;
        budget
            .control(admission.validation_work)
            .map_err(|e| at(Stage::Static, None, e))?;
        budget
            .release_control(
                STATIC_WORK_UPPER
                    .checked_sub(admission.validation_work)
                    .ok_or_else(|| at(Stage::Static, None, Error::Budget("static upper")))?,
            )
            .map_err(|e| at(Stage::Static, None, e))?;
        Ok(Self {
            state: StaticState {
                view,
                admission,
                actual_static_bytes,
            },
            budget,
        })
    }
    pub fn virtual_count(&self) -> usize {
        self.state.virtual_count()
    }
    pub fn static_validation_work(&self) -> u64 {
        self.state.admission.validation_work
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    upper: u64,
    outcome: QueryOutcome,
}
#[derive(Debug, Default)]
pub struct SessionStats {
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
#[derive(Debug)]
pub struct Session<'view, 'budget> {
    prepared: StaticState<'view>,
    budget: &'budget mut Budget,
    limits: SessionLimits,
    total: u64,
    pool: rayon::ThreadPool,
    workers: Vec<Worker>,
    pending: Vec<Pending>,
    failed: bool,
    stats: SessionStats,
}
#[derive(Debug)]
pub struct SessionActivation {
    pub marked: Vec<u64>,
    pub stats: SessionStats,
}
impl<'view, 'budget> Session<'view, 'budget> {
    pub fn new(
        prepared: PreparedStatic<'view, 'budget>,
        limits: SessionLimits,
        total: u64,
    ) -> SessionResult<Self> {
        let PreparedStatic {
            state: prepared,
            budget,
        } = prepared;
        let fail = |e| at(Stage::Static, None, e);
        limits.validate().map_err(fail)?;
        if budget.publishing
            || budget.cap != limits.max_work
            || budget.initial_control_reservation
                != Budget::control_reservation(total, limits.max_threads).map_err(fail)?
        {
            return Err(fail(Error::Budget("session budget identity")));
        }
        if prepared.actual_static_bytes > limits.max_static_bytes {
            return Err(fail(Error::Budget("prepared static allowance")));
        }
        if total > limits.max_queries {
            return Err(fail(Error::Budget("stream query count")));
        }
        let workers_count = limits
            .max_threads
            .min(usize::try_from(total.max(1)).map_err(|_| fail(Error::Budget("worker count")))?);
        let words = prepared.virtual_count().div_ceil(64);
        let planned = (words as u64)
            .checked_mul(8 * (workers_count as u64 + 1))
            .and_then(|n| {
                n.checked_add((limits.max_tile_queries as u64) * size_of::<Pending>() as u64)
            })
            .and_then(|n| n.checked_add(workers_count as u64 * size_of::<Worker>() as u64 + 4096))
            .ok_or_else(|| fail(Error::Budget("aux arithmetic")))?;
        if planned > limits.max_aux_bytes {
            return Err(fail(Error::Budget("session aux admission")));
        }
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(workers_count)
            .map_err(|_| fail(Error::Budget("workers allocation")))?;
        for _ in 0..workers_count {
            workers.push(Worker {
                marked: filled(words, 0u64).map_err(fail)?,
            });
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(limits.max_tile_queries)
            .map_err(|_| fail(Error::Budget("pending allocation")))?;
        let actual = workers
            .iter()
            .map(|w| w.marked.capacity() as u64 * 8)
            .sum::<u64>()
            .checked_add(pending.capacity() as u64 * size_of::<Pending>() as u64)
            .and_then(|n| {
                n.checked_add(workers.capacity() as u64 * size_of::<Worker>() as u64 + 4096)
            })
            .ok_or_else(|| fail(Error::Budget("aux capacities")))?;
        if actual > planned || actual > limits.max_aux_bytes {
            return Err(fail(Error::Budget("actual session aux")));
        }
        let bit_work = words as u64 * workers_count as u64;
        budget.control(bit_work).map_err(fail)?;
        let excess = 2
            * (MAX_VIRTUAL.div_ceil(64) as u64 * limits.max_threads as u64
                - words as u64 * workers_count as u64);
        budget.release_control(excess).map_err(fail)?;
        budget.fixed_event().map_err(fail)?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers_count)
            .stack_size(2 << 20)
            .build()
            .map_err(|_| fail(Error::Budget("CPU worker pool")))?;
        let stats = SessionStats {
            static_validation_work: prepared.admission.validation_work,
            actual_static_bytes: prepared.actual_static_bytes,
            legacy_admission_bytes: prepared.admission.view_bytes,
            workers: workers_count,
            virtual_shapes: prepared.virtual_count(),
            aux_bytes: actual,
            bitset_work: bit_work,
            ..SessionStats::default()
        };
        Ok(Self {
            prepared,
            budget,
            limits,
            total,
            pool,
            workers,
            pending,
            failed: false,
            stats,
        })
    }
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }
    /// Charges transport work against this session's exclusively held ledger.
    pub fn io(&mut self, work: u64) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("session already failed"));
        }
        let result = self.budget.io(work);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    /// Charges one bounded protocol event against the same held ledger.
    pub fn fixed_event(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("session already failed"));
        }
        let result = self.budget.fixed_event();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn push(&mut self, queries: Table<'_, Query>) -> SessionResult<()> {
        if self.failed {
            return Err(at(
                Stage::Window,
                None,
                Error::Invalid("session already failed"),
            ));
        }
        let result = self.push_inner(queries);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn push_inner(&mut self, queries: Table<'_, Query>) -> SessionResult<()> {
        let budget = &mut *self.budget;
        let first_global = self.stats.queries_completed;
        let fail = |e| at(Stage::Window, Some(first_global), e);
        if queries.is_empty()
            || queries.len() > self.limits.max_tile_queries
            || self
                .stats
                .queries_staged
                .checked_add(queries.len() as u64)
                .is_none_or(|n| n > self.total)
        {
            return Err(fail(Error::Budget("staged window count")));
        }
        queries.valid_stride().map_err(fail)?;
        self.pending.clear();
        self.pending.resize(queries.len(), Pending::default());
        budget.control(queries.len() as u64 + 1).map_err(fail)?;
        budget
            .release_control(queries.len() as u64 - 1)
            .map_err(fail)?;
        self.stats.outcome_initialization_work += queries.len() as u64;
        self.stats.stage_windows += 1;
        self.stats.queries_staged += queries.len() as u64;
        let mut valid = queries.len();
        let mut invalid = None;
        for i in 0..queries.len() {
            budget.control(1).map_err(fail)?;
            self.stats.query_validation_work += 1;
            match queries
                .get(i)
                .and_then(|q| self.prepared.upper(q, self.limits))
            {
                Ok(upper) => self.pending[i].upper = upper,
                Err(error) => {
                    valid = i;
                    invalid = Some(at(Stage::Query, Some(first_global + i as u64), error));
                    break;
                }
            }
        }
        let mut first = 0;
        while first < valid {
            let available = budget.available_query_work().map_err(fail)?;
            let mut bound = 0u64;
            let mut end = first;
            while end < valid {
                let Some(next) = bound.checked_add(self.pending[end].upper) else {
                    break;
                };
                if next > available {
                    break;
                }
                bound = next;
                end += 1;
            }
            if end == first {
                return Err(at(
                    Stage::Query,
                    Some(first_global + first as u64),
                    Error::Budget("unlaunched query prefix"),
                ));
            }
            budget.start_queries(bound).map_err(fail)?;
            budget.control(1).map_err(fail)?;
            let slots = &mut self.pending[first..end];
            let chunk = slots.len().div_ceil(self.workers.len());
            let view = self.prepared.view;
            let mask = self.prepared.admission.layer_mask;
            self.pool.install(|| {
                slots
                    .par_chunks_mut(chunk)
                    .zip(self.workers.par_iter_mut())
                    .enumerate()
                    .for_each(|(part, (slots, worker))| {
                        for (j, slot) in slots.iter_mut().enumerate() {
                            let ordinal = first + part * chunk + j;
                            slot.outcome = match queries.get(ordinal) {
                                Ok(q) => {
                                    query_measured(view, q, &mut worker.marked, mask, slot.upper)
                                }
                                Err(error) => QueryOutcome {
                                    work: 0,
                                    error: Some(error),
                                },
                            };
                        }
                    });
            });
            let mut actual = 0u64;
            let mut error = None;
            for (i, slot) in self.pending[first..end].iter().enumerate() {
                actual = actual
                    .checked_add(slot.outcome.work)
                    .ok_or_else(|| fail(Error::Budget("ordered query sum")))?;
                if error.is_none() {
                    error = slot
                        .outcome
                        .error
                        .map(|e| at(Stage::Query, Some(first_global + (first + i) as u64), e));
                }
            }
            budget.finish_queries(actual).map_err(fail)?;
            budget.control((end - first) as u64).map_err(fail)?;
            budget
                .release_control((end - first - 1) as u64)
                .map_err(fail)?;
            self.stats.ordered_aggregation_work += (end - first) as u64;
            self.stats.executed_prefixes += 1;
            self.stats.query_work += actual;
            self.stats.queries_completed += (end - first) as u64;
            if let Some(error) = error {
                return Err(error);
            }
            first = end;
        }
        if let Some(error) = invalid {
            return Err(error);
        }
        Ok(())
    }
    pub fn finish(mut self) -> SessionResult<SessionActivation> {
        let budget = &mut *self.budget;
        let fail = |e| at(Stage::Finish, None, e);
        if self.failed
            || self.stats.queries_completed != self.total
            || self.stats.queries_staged != self.total
        {
            return Err(fail(Error::Invalid("incomplete query stream")));
        }
        let words = self.prepared.virtual_count().div_ceil(64);
        let remaining_bits = words as u64 * self.workers.len() as u64;
        budget.control(remaining_bits).map_err(fail)?;
        budget.fixed_event().map_err(fail)?;
        self.stats.bitset_work += remaining_bits;
        let (first, others) = self
            .workers
            .split_first_mut()
            .ok_or_else(|| fail(Error::Budget("workers missing")))?;
        for other in others {
            for (a, b) in first.marked.iter_mut().zip(&other.marked) {
                *a |= *b;
            }
        }
        self.stats.active_shapes = first
            .marked
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        if let Some(last) = first.marked.last() {
            let tail = self.prepared.virtual_count() % 64;
            if tail != 0 && *last >> tail != 0 {
                return Err(fail(Error::Invalid("activation tail bits")));
            }
        }
        let marked = std::mem::take(&mut first.marked);
        Ok(SessionActivation {
            marked,
            stats: self.stats,
        })
    }
}

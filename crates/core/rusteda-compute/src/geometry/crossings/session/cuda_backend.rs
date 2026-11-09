// SPDX-License-Identifier: Apache-2.0
//! Owned CUDA stream with a complete pre-context range pass and one ledger.
//! Static/cache/source/process authority belongs to the separately pinned DTO
//! adapter; no file or process is opened here. Never constructs the CPU pool.
#![allow(unsafe_code)]

use super::super::{Record, device_abi, integer, outcome};
use super::*;
use crate::GpuError;
use crate::cuda::{
    CudaSlice, DeviceAdmission, PtxDevice, PtxDeviceStats, PushKernelArg, VerifiedPtx, launch_1d,
};
use sha2::{Digest, Sha256};

const RANGE_DOMAIN: &[u8] = b"RustEDA.G1.range.queries.v1\0";
const BLOCK_THREADS: usize = 128;
const OUTCOME_CHECKS_PER_QUERY: u64 = 3;
const SENTINEL: u64 = u64::MAX;

fn blocks(bytes: u64) -> Result<u64> {
    bytes
        .checked_add(63)
        .map(|n| n / 64)
        .ok_or(Error::Budget("CUDA IO rounding"))
}

/// Entire backend control hold, added before static reads. The fixed 100B cap
/// is unchanged. CPU workers are not inferred from this reservation schedule.
pub fn control_reservation(queries: u64) -> Result<u64> {
    Budget::control_reservation(queries, 1)?
        .checked_add(integer::range_control_upper(queries)?)
        .and_then(|n| n.checked_add(OUTCOME_CHECKS_PER_QUERY * queries))
        .ok_or(Error::Budget("CUDA control reservation"))
}

/// Additional backend IO hold. Worst case is one query per prefix, so the
/// three block-rounded transfers cost at most three units per query. This is
/// a reservation, not a claim that every prefix has that worst-case size.
pub fn io_reservation(static_bytes: u64, queries: u64, virtual_shapes: usize) -> Result<u64> {
    if !(4096..=MAX_STATIC_BYTES).contains(&static_bytes)
        || queries > MAX_STREAM_QUERIES
        || virtual_shapes > MAX_VIRTUAL
    {
        return Err(Error::Budget("CUDA IO admission"));
    }
    let words = virtual_shapes.div_ceil(64) as u64;
    let words_bytes = words * 8;
    // Complete S range SHA read+hash; static H2D; logical range-record reads;
    // activation initialization/readback; two fixed hash domains; and bounded
    // per-query range read/encode/hash, staging decode/encode, transfer and hash.
    (3 * blocks(static_bytes)?)
        .checked_add(blocks(
            64 + 96 * MAX_INSTANCES as u64 + 48 * MAX_RECTANGLES as u64,
        )?)
        .and_then(|n| n.checked_add((4096_u64 + 5 * 63).div_ceil(64)))
        .and_then(|n| n.checked_add((8 * words.max(1)).div_ceil(64) + words_bytes.div_ceil(64)))
        .and_then(|n| {
            n.checked_add(
                (40_u64 * MAX_QUERIES as u64).div_ceil(64)
                    + (16_u64 * MAX_QUERIES as u64).div_ceil(64),
            )
        })
        .and_then(|n| n.checked_add(11 * queries))
        .and_then(|n| n.checked_add(8)) // three hash domains plus five range-proof units
        .ok_or(Error::Budget("CUDA IO reservation"))
}

/// A complete range-pass receipt; the digest is over the canonical ordered
/// query payload, not a geometry oracle or a source/protocol qualification.
#[derive(Clone, Copy, Debug)]
pub struct RangeProof {
    pub static_sha256: [u8; 32],
    pub query_sha256: [u8; 32],
    pub queries: u64,
    pub minimum_scalar: i64,
    pub maximum_scalar: i64,
    pub control_work: u64,
}

fn static_binding(view: View<'_>, raw: &[u8], budget: &mut Budget) -> Result<()> {
    use device_abi::{i64_at, part, u32_at, u64_at, zeros};
    if raw.len() < 4096
        || raw.len() as u64 > MAX_STATIC_BYTES
        || part(raw, 0, 8)? != b"RG1STA02"
        || u32_at(raw, 8)? != 2
        || u32_at(raw, 12)? != 4096
        || u64_at(raw, 16)? != raw.len() as u64
        || u32_at(raw, 24)? != view.layer_count
        || u32_at(raw, 28)? as usize != view.virtual_count
        || i64_at(raw, 32)? != view.grid.origin[0]
        || i64_at(raw, 40)? != view.grid.origin[1]
        || i64_at(raw, 48)? != view.grid.bin
        || u32_at(raw, 56)? != view.grid.width
        || u32_at(raw, 60)? != view.grid.height
    {
        return Err(Error::Invalid("CUDA static image/header binding"));
    }
    let counts = [
        view.offsets.len(),
        view.entries.len(),
        view.instances.len(),
        view.template_offsets.len(),
        view.rectangles.len(),
    ];
    let (ranges, end) = device_abi::layout(counts)?;
    if end != raw.len() {
        return Err(Error::Invalid("CUDA static image extent"));
    }
    let mut previous = 4096;
    let mut gap_bytes = 0;
    for (i, &(start, end)) in ranges.iter().enumerate() {
        if u64_at(raw, 64 + 16 * i)? != start as u64
            || u64_at(raw, 72 + 16 * i)? != counts[i] as u64
        {
            return Err(Error::Invalid("CUDA static descriptor"));
        }
        gap_bytes += start - previous;
        previous = end;
    }
    // Header field decoding plus its reserved bytes is one complete logical
    // 4096-byte header read; each section-alignment gap is separately scanned.
    budget.io(blocks((4096 + gap_bytes) as u64)?)?;
    zeros(&raw[144..4096])?;
    previous = 4096;
    for &(start, end) in &ranges {
        zeros(part(raw, previous, start - previous)?)?;
        previous = end;
    }
    fn same<T: Record>(table: Table<'_, T>, bytes: &[u8]) -> Result<()> {
        match table {
            Table::Bytes(rows) if rows.len() == bytes.len() && rows.as_ptr() == bytes.as_ptr() => {
                Ok(())
            }
            _ => Err(Error::Invalid("CUDA requires same borrowed static bytes")),
        }
    }
    same(view.offsets, &raw[ranges[0].0..ranges[0].1])?;
    same(view.entries, &raw[ranges[1].0..ranges[1].1])?;
    same(view.instances, &raw[ranges[2].0..ranges[2].1])?;
    same(view.template_offsets, &raw[ranges[3].0..ranges[3].1])?;
    same(view.rectangles, &raw[ranges[4].0..ranges[4].1])
}

/// The same exclusive budget is held while every query is checked before any
/// device context exists. A failed/dropped preparation cannot be retried using
/// another tape under that ledger (PreparedStatic already consumed its attempt).
pub struct CudaPreparation<'view, 'budget> {
    prepared: StaticState<'view>,
    budget: &'budget mut Budget,
    raw: &'view [u8],
    limits: SessionLimits,
    total: u64,
    checked: u64,
    hash: Sha256,
    extrema: integer::ScalarExtrema,
    static_sha256: [u8; 32],
    range_control: u64,
    failed: bool,
}

pub struct CudaReady<'view, 'budget> {
    prepared: StaticState<'view>,
    budget: &'budget mut Budget,
    raw: &'view [u8],
    limits: SessionLimits,
    total: u64,
    proof: RangeProof,
}

impl fmt::Debug for CudaPreparation<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CudaPreparation")
            .field("queries", &self.total)
            .field("checked", &self.checked)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for CudaReady<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CudaReady")
            .field("proof", &self.proof)
            .finish_non_exhaustive()
    }
}

impl<'view, 'budget> CudaPreparation<'view, 'budget> {
    pub fn io(&mut self, work: u64) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("CUDA range already failed"));
        }
        let result = self.budget.io(work);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn fixed_event(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("CUDA range already failed"));
        }
        let result = self.budget.fixed_event();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn new(
        prepared: PreparedStatic<'view, 'budget>,
        raw: &'view [u8],
        limits: SessionLimits,
        total: u64,
        expected_static_sha256: [u8; 32],
    ) -> SessionResult<Self> {
        let PreparedStatic {
            state: prepared,
            budget,
        } = prepared;
        let fail = |e| at(Stage::Static, None, e);
        limits.validate().map_err(fail)?;
        if limits.max_threads != 1
            || total > limits.max_queries
            || prepared.actual_static_bytes != raw.len() as u64
            || budget.initial_control_reservation != control_reservation(total).map_err(fail)?
            || budget.cap != limits.max_work
            || budget.publishing
            || budget.io_held
                < io_reservation(raw.len() as u64, total, prepared.virtual_count()).map_err(fail)?
        {
            return Err(fail(Error::Budget("CUDA preparation identity")));
        }
        budget
            .io(2 * blocks(raw.len() as u64).map_err(fail)?)
            .map_err(fail)?;
        let static_sha256: [u8; 32] = Sha256::digest(raw).into();
        if static_sha256 != expected_static_sha256 {
            return Err(fail(Error::Invalid("CUDA static SHA binding")));
        }
        static_binding(prepared.view, raw, budget).map_err(fail)?;
        let start = budget.snapshot().control_completed;
        let range_bytes = 64
            + 96 * prepared.view.instances.len() as u64
            + 48 * prepared.view.rectangles.len() as u64;
        budget
            .io(blocks(range_bytes).map_err(fail)?)
            .map_err(fail)?;
        let extrema = integer::static_range(prepared.view, budget).map_err(fail)?;
        let range_control = budget.snapshot().control_completed - start;
        budget.io(1).map_err(fail)?;
        let mut hash = Sha256::new();
        hash.update(RANGE_DOMAIN);
        Ok(Self {
            prepared,
            budget,
            raw,
            limits,
            total,
            checked: 0,
            hash,
            extrema,
            static_sha256,
            range_control,
            failed: false,
        })
    }

    pub fn push_range(&mut self, queries: Table<'_, Query>) -> SessionResult<()> {
        let first = self.checked;
        let fail = |e| at(Stage::Window, Some(first), e);
        if self.failed {
            return Err(fail(Error::Invalid("CUDA range already failed")));
        }
        let result = (|| -> Result<()> {
            if queries.is_empty()
                || queries.len() > self.limits.max_tile_queries
                || self
                    .checked
                    .checked_add(queries.len() as u64)
                    .is_none_or(|n| n > self.total)
            {
                return Err(Error::Budget("CUDA complete range count"));
            }
            queries.valid_stride()?;
            for index in 0..queries.len() {
                self.budget.io(3)?;
                let query = queries.get(index)?;
                integer::query_one(
                    query,
                    self.prepared.view.layer_count,
                    self.budget,
                    &mut self.extrema,
                )?;
                // Includes raw expanded area outside the grid before the
                // absent-layer shortcut, exactly as the original CPU bound.
                self.prepared.upper(query, self.limits)?;
                let mut record = [0_u8; 40];
                query.encode(&mut record)?;
                self.hash.update(record);
                self.checked += 1;
                self.range_control += 5;
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result.map_err(|e| at(Stage::Query, Some(self.checked), e))
    }

    pub fn finish_range(self) -> SessionResult<CudaReady<'view, 'budget>> {
        let fail = |e| at(Stage::Static, None, e);
        if self.failed || self.checked != self.total {
            return Err(fail(Error::Invalid("incomplete CUDA range pass")));
        }
        self.budget
            .release_control(
                integer::range_control_upper(self.total)
                    .map_err(fail)?
                    .checked_sub(self.range_control)
                    .ok_or_else(|| fail(Error::Budget("CUDA range upper")))?,
            )
            .map_err(fail)?;
        let proof = RangeProof {
            static_sha256: self.static_sha256,
            query_sha256: self.hash.finalize().into(),
            queries: self.checked,
            minimum_scalar: self.extrema.minimum,
            maximum_scalar: self.extrema.maximum,
            control_work: self.range_control,
        };
        Ok(CudaReady {
            prepared: self.prepared,
            budget: self.budget,
            raw: self.raw,
            limits: self.limits,
            total: self.total,
            proof,
        })
    }
}

#[derive(Debug)]
pub enum CudaError {
    Semantic(Error),
    Driver(GpuError),
}

#[derive(Debug)]
pub struct CudaFailure {
    pub stage: Stage,
    pub query_ordinal: Option<u64>,
    pub error: CudaError,
}
impl fmt::Display for CudaFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}:{:?}:{:?}",
            self.stage, self.query_ordinal, self.error
        )
    }
}
impl std::error::Error for CudaFailure {}
pub type CudaResult<T> = std::result::Result<T, CudaFailure>;
fn semantic(stage: Stage, ordinal: Option<u64>, error: Error) -> CudaFailure {
    CudaFailure {
        stage,
        query_ordinal: ordinal,
        error: CudaError::Semantic(error),
    }
}
fn driver(stage: Stage, ordinal: Option<u64>, error: impl Into<GpuError>) -> CudaFailure {
    CudaFailure {
        stage,
        query_ordinal: ordinal,
        error: CudaError::Driver(error.into()),
    }
}

/// Backend observations are separate from CPU worker statistics. Device bytes
/// count attempted copies; completed bytes advance only after synchronization.
#[derive(Debug, Default)]
pub struct CudaStats {
    pub core: SessionStats,
    pub device_bytes_attempted: u64,
    pub device_bytes_completed: u64,
    pub device_outcome_checks: u64,
    pub first_raw_error: Option<(u64, u32)>,
    pub payload_bytes: u64,
    pub padded_threads: u32,
}

/// One foreground session. No Clone, Rayon pool, per-query seen set, second
/// budget or CPU fallback exists. Dropping on failure cannot publish results.
pub struct CudaSession<'view, 'budget> {
    prepared: StaticState<'view>,
    budget: &'budget mut Budget,
    limits: SessionLimits,
    total: u64,
    proof: RangeProof,
    device: PtxDevice,
    static_dev: CudaSlice<u8>,
    queries_dev: CudaSlice<u8>,
    outcomes_dev: CudaSlice<u64>,
    activation_dev: CudaSlice<u64>,
    query_staging: Vec<u8>,
    outcome_staging: Vec<u64>,
    uppers: Vec<u64>,
    query_hash: Sha256,
    transcript: outcome::RollingOutcome,
    failed: bool,
    stats: CudaStats,
}
impl fmt::Debug for CudaSession<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CudaSession")
            .field("stats", &self.stats)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct CudaActivation {
    pub marked: Vec<u64>,
    pub stats: CudaStats,
    pub range_proof: RangeProof,
    pub range_digest: [u8; 32],
    pub outcome_digest: outcome::OutcomeDigest,
    pub device: PtxDeviceStats,
}

impl<'view, 'budget> CudaSession<'view, 'budget> {
    /// `CudaReady` proves complete S/T narrow admission before a context can be
    /// created. The adapter must first authenticate the image/source/CPU gate,
    /// host cgroup/cache model and CUDA_CACHE_DISABLE=1 in its recorded spawn.
    pub fn new(ready: CudaReady<'view, 'budget>, ptx: VerifiedPtx) -> CudaResult<Self> {
        let CudaReady {
            prepared,
            budget,
            raw,
            limits,
            total,
            proof,
        } = ready;
        let fail = |e| semantic(Stage::Static, None, e);
        if !cfg!(target_endian = "little") {
            return Err(fail(Error::Invalid("CUDA host byte order")));
        }
        let tile = limits.max_tile_queries.min(total.max(1) as usize);
        let words = prepared.virtual_count().div_ceil(64);
        let payload = (raw.len() as u64)
            .checked_add(40 * tile as u64 + 16 * tile as u64 + 8 * words.max(1) as u64)
            .filter(|n| *n <= 40 << 20)
            .ok_or_else(|| fail(Error::Budget("CUDA payload")))?;
        let padded = u32::try_from(tile.div_ceil(BLOCK_THREADS) * BLOCK_THREADS)
            .map_err(|_| fail(Error::Budget("CUDA padded threads")))?;
        let aux = 64 * tile as u64 + 8 * words as u64 + 4096;
        if aux > limits.max_aux_bytes {
            return Err(fail(Error::Budget("CUDA host aux before allocation")));
        }
        // Host and device allocation are distinct. All host proportional
        // reserves are checked before context/JIT or any array allocation.
        let mut query_staging = Vec::new();
        query_staging
            .try_reserve_exact(40 * tile)
            .map_err(|_| fail(Error::Budget("CUDA query staging")))?;
        let mut outcome_staging = Vec::new();
        outcome_staging
            .try_reserve_exact(2 * tile)
            .map_err(|_| fail(Error::Budget("CUDA outcome staging")))?;
        let mut uppers = Vec::new();
        uppers
            .try_reserve_exact(tile)
            .map_err(|_| fail(Error::Budget("CUDA upper staging")))?;
        let actual_aux = query_staging.capacity() as u64
            + 8 * outcome_staging.capacity() as u64
            + 8 * uppers.capacity() as u64
            + 8 * words as u64
            + 4096;
        if actual_aux > aux || actual_aux > limits.max_aux_bytes {
            return Err(fail(Error::Budget("CUDA actual host aux")));
        }
        budget.control(words as u64).map_err(fail)?;
        budget
            .release_control(2 * (MAX_VIRTUAL.div_ceil(64) - words) as u64)
            .map_err(fail)?;
        budget.fixed_event().map_err(fail)?;
        budget
            .io(blocks(raw.len() as u64).map_err(fail)?
                + blocks(8 * words.max(1) as u64).map_err(fail)?
                + blocks(40 * tile as u64).map_err(fail)?
                + blocks(16 * tile as u64).map_err(fail)?)
            .map_err(fail)?;
        // Domain hashes are charged before construction; range domain was
        // already charged in preparation. This session hashes its own replay.
        budget.io(2).map_err(fail)?;
        let mut query_hash = Sha256::new();
        query_hash.update(RANGE_DOMAIN);
        let transcript = outcome::RollingOutcome::new();
        let admission =
            DeviceAdmission::new(payload, padded).map_err(|e| driver(Stage::Static, None, e))?;
        let device = PtxDevice::open(ptx, admission).map_err(|e| driver(Stage::Static, None, e))?;
        let stream = device.stream();
        let static_dev = stream
            .clone_htod(raw)
            .map_err(|e| driver(Stage::Static, None, e))?;
        let queries_dev = stream
            .alloc_zeros(40 * tile)
            .map_err(|e| driver(Stage::Static, None, e))?;
        let outcomes_dev = stream
            .alloc_zeros(2 * tile)
            .map_err(|e| driver(Stage::Static, None, e))?;
        let activation_dev = stream
            .alloc_zeros(words.max(1))
            .map_err(|e| driver(Stage::Static, None, e))?;
        device.sync().map_err(|e| driver(Stage::Static, None, e))?;
        let initialized =
            raw.len() as u64 + 40 * tile as u64 + 16 * tile as u64 + 8 * words.max(1) as u64;
        // All initialization attempts were charged before context/allocation;
        // they are distinct from the later per-prefix sentinel reset.
        let stats = CudaStats {
            core: SessionStats {
                workers: 0,
                virtual_shapes: prepared.virtual_count(),
                static_validation_work: prepared.admission.validation_work,
                actual_static_bytes: prepared.actual_static_bytes,
                legacy_admission_bytes: prepared.admission.view_bytes,
                aux_bytes: actual_aux,
                bitset_work: words as u64,
                ..SessionStats::default()
            },
            device_bytes_attempted: initialized,
            device_bytes_completed: initialized,
            payload_bytes: payload,
            padded_threads: padded,
            ..CudaStats::default()
        };
        Ok(Self {
            prepared,
            budget,
            limits,
            total,
            proof,
            device,
            static_dev,
            queries_dev,
            outcomes_dev,
            activation_dev,
            query_staging,
            outcome_staging,
            uppers,
            query_hash,
            transcript,
            failed: false,
            stats,
        })
    }

    pub fn stats(&self) -> &CudaStats {
        &self.stats
    }
    pub fn outcome_digest(&self) -> outcome::OutcomeDigest {
        self.transcript.snapshot()
    }
    pub fn unreconciled_device_work_upper(&self) -> u64 {
        self.budget.snapshot().query_held
    }
    pub fn io(&mut self, work: u64) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("CUDA session failed"));
        }
        let result = self.budget.io(work);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn fixed_event(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("CUDA session failed"));
        }
        let result = self.budget.fixed_event();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn push(&mut self, queries: Table<'_, Query>) -> CudaResult<()> {
        if self.failed {
            return Err(semantic(
                Stage::Window,
                None,
                Error::Invalid("CUDA session failed"),
            ));
        }
        let result = self.push_inner(queries);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(&mut self, queries: Table<'_, Query>) -> CudaResult<()> {
        let first_global = self.stats.core.queries_completed;
        let fail = |e| semantic(Stage::Window, Some(first_global), e);
        if queries.is_empty()
            || queries.len() > self.limits.max_tile_queries
            || self
                .stats
                .core
                .queries_staged
                .checked_add(queries.len() as u64)
                .is_none_or(|n| n > self.total)
        {
            return Err(fail(Error::Budget("CUDA staged window")));
        }
        queries.valid_stride().map_err(fail)?;
        self.uppers.clear();
        self.query_staging.clear();
        self.budget
            .control(queries.len() as u64 + 1)
            .map_err(fail)?;
        self.budget
            .release_control(queries.len() as u64 - 1)
            .map_err(fail)?;
        self.stats.core.outcome_initialization_work += queries.len() as u64;
        self.stats.core.stage_windows += 1;
        self.stats.core.queries_staged += queries.len() as u64;
        // Like the CPU, validate the complete staged window before launching
        // any prefix, then execute its valid prefix before a later invalid row.
        let mut invalid = None;
        for i in 0..queries.len() {
            self.budget.control(1).map_err(fail)?;
            self.budget.io(3).map_err(fail)?;
            self.stats.core.query_validation_work += 1;
            let checked = queries
                .get(i)
                .and_then(|q| self.prepared.upper(q, self.limits).map(|u| (q, u)));
            match checked {
                Ok((query, upper)) => {
                    let mut raw = [0_u8; 40];
                    query.encode(&mut raw).map_err(fail)?;
                    self.query_hash.update(raw);
                    self.query_staging.extend_from_slice(&raw);
                    self.uppers.push(upper);
                }
                Err(error) => {
                    invalid = Some(semantic(Stage::Query, Some(first_global + i as u64), error));
                    break;
                }
            }
        }
        let mut first = 0;
        while first < self.uppers.len() {
            let available = self.budget.available_query_work().map_err(fail)?;
            let mut upper = 0_u64;
            let mut end = first;
            while end < self.uppers.len() {
                let Some(next) = upper.checked_add(self.uppers[end]) else {
                    break;
                };
                if next > available {
                    break;
                }
                upper = next;
                end += 1;
            }
            if end == first {
                return Err(semantic(
                    Stage::Query,
                    Some(first_global + first as u64),
                    Error::Budget("unlaunched query prefix"),
                ));
            }
            self.budget.start_queries(upper).map_err(fail)?;
            self.budget.control(1).map_err(fail)?;
            let n = end - first;
            let qbytes = 40 * n;
            let obytes = 16 * n;
            self.budget
                .io(blocks(qbytes as u64).map_err(fail)?
                    + 2 * blocks(obytes as u64).map_err(fail)?)
                .map_err(fail)?;
            self.outcome_staging.clear();
            self.outcome_staging.resize(2 * n, SENTINEL);
            self.stats.device_bytes_attempted += (qbytes + 2 * obytes) as u64;
            {
                let stream = self.device.stream();
                let mut qslice = self.queries_dev.slice_mut(..qbytes);
                stream
                    .memcpy_htod(&self.query_staging[40 * first..40 * end], &mut qslice)
                    .map_err(|e| driver(Stage::Query, Some(first_global + first as u64), e))?;
                let mut oslice = self.outcomes_dev.slice_mut(..2 * n);
                stream
                    .memcpy_htod(&self.outcome_staging, &mut oslice)
                    .map_err(|e| driver(Stage::Query, Some(first_global + first as u64), e))?;
            }
            let static_bytes = self.prepared.actual_static_bytes;
            let count = n as u64;
            let global = first_global + first as u64;
            let mask = u64::from(self.prepared.admission.layer_mask);
            let max_cells = self.limits.max_query_cells;
            let words = self.prepared.virtual_count().div_ceil(64) as u64;
            let mut launch = self.device.stream().launch_builder(self.device.function());
            launch
                .arg(&self.static_dev)
                .arg(&static_bytes)
                .arg(&self.queries_dev)
                .arg(&count)
                .arg(&global)
                .arg(&mask)
                .arg(&max_cells)
                .arg(&mut self.outcomes_dev)
                .arg(&mut self.activation_dev)
                .arg(&words);
            // SAFETY: complete canonical static+M/span admission preceded the
            // context. This prefix is within the preallocated tile; ten ABI
            // arguments have separately verified PTX roles. Only aligned u64
            // atomic OR races. Outcomes have one slot per active thread and
            // sentinel/index checks happen after synchronization. All arrays
            // remain owned here until synchronization/drop completes.
            unsafe { launch.launch(launch_1d(n, BLOCK_THREADS as u32)) }
                .map_err(|e| driver(Stage::Query, Some(global), e))?;
            let source = self.outcomes_dev.slice(..2 * n);
            self.device
                .stream()
                .memcpy_dtoh(&source, &mut self.outcome_staging)
                .map_err(|e| driver(Stage::Query, Some(global), e))?;
            self.device
                .sync()
                .map_err(|e| driver(Stage::Query, Some(global), e))?;
            self.stats.device_bytes_completed += (qbytes + 2 * obytes) as u64;
            self.budget
                .control(OUTCOME_CHECKS_PER_QUERY * n as u64)
                .map_err(fail)?;
            self.stats.device_outcome_checks += OUTCOME_CHECKS_PER_QUERY * n as u64;
            let mut actual = 0_u64;
            let mut error = None;
            // First validate EVERY row before trusting any sum or transcript.
            for i in 0..n {
                let work = self.outcome_staging[2 * i];
                let packed = self.outcome_staging[2 * i + 1];
                let raw_status = packed as u32;
                let index = (packed >> 32) as u32;
                if index != i as u32
                    || raw_status > 5
                    || work > self.uppers[first + i]
                    || (raw_status == 0 && work == 0)
                {
                    return Err(fail(Error::Invalid("untrusted CUDA outcome/index/work")));
                }
                actual = actual
                    .checked_add(work)
                    .ok_or_else(|| fail(Error::Budget("CUDA ordered sum")))?;
                if raw_status != 0 && error.is_none() {
                    self.stats.first_raw_error = Some((global + i as u64, raw_status));
                    let e = if raw_status == 3 {
                        Error::Budget("device query work")
                    } else {
                        Error::Invalid("device query status")
                    };
                    error = Some(semantic(Stage::Query, Some(global + i as u64), e));
                }
            }
            for i in 0..n {
                let raw_status = self.outcome_staging[2 * i + 1] as u32;
                self.budget
                    .io(outcome::OUTCOME_RECORD_IO_WORK)
                    .map_err(fail)?;
                self.transcript
                    .record(global + i as u64, self.outcome_staging[2 * i], raw_status)
                    .map_err(fail)?;
            }
            self.budget.finish_queries(actual).map_err(fail)?;
            self.budget.control(n as u64).map_err(fail)?;
            self.budget.release_control(n as u64 - 1).map_err(fail)?;
            self.stats.core.ordered_aggregation_work += n as u64;
            self.stats.core.executed_prefixes += 1;
            self.stats.core.query_work += actual;
            self.stats.core.queries_completed += n as u64;
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

    /// Synchronize/read/validate/free everything before returning publication
    /// data. The exclusive ledger borrow then returns to the adapter's tail.
    pub fn finish(mut self) -> CudaResult<CudaActivation> {
        let fail = |e| semantic(Stage::Finish, None, e);
        if self.failed
            || self.stats.core.queries_completed != self.total
            || self.stats.core.queries_staged != self.total
        {
            return Err(fail(Error::Invalid("incomplete CUDA stream")));
        }
        let actual_query_sha: [u8; 32] = self.query_hash.finalize().into();
        if actual_query_sha != self.proof.query_sha256 {
            return Err(fail(Error::Invalid("CUDA range/execution tape identity")));
        }
        let words = self.prepared.virtual_count().div_ceil(64);
        self.budget.control(words as u64).map_err(fail)?;
        self.budget.fixed_event().map_err(fail)?;
        self.budget
            .io(blocks(8 * words as u64).map_err(fail)?)
            .map_err(fail)?;
        let mut marked = filled(words, 0_u64).map_err(fail)?;
        if words != 0 {
            self.stats.device_bytes_attempted += 8 * words as u64;
            let source = self.activation_dev.slice(..words);
            self.device
                .stream()
                .memcpy_dtoh(&source, &mut marked)
                .map_err(|e| driver(Stage::Finish, None, e))?;
            self.device
                .sync()
                .map_err(|e| driver(Stage::Finish, None, e))?;
            self.stats.device_bytes_completed += 8 * words as u64;
        }
        self.stats.core.bitset_work += words as u64;
        self.stats.core.active_shapes = marked.iter().map(|w| w.count_ones() as usize).sum();
        let tail = self.prepared.virtual_count() % 64;
        if tail != 0 && marked.last().is_some_and(|w| *w >> tail != 0) {
            return Err(fail(Error::Invalid("CUDA activation tail")));
        }
        let device_stats = self.device.stats();
        self.budget.io(5).map_err(fail)?;
        let mut proof_bytes = [0_u8; 128];
        proof_bytes[..32].copy_from_slice(&self.proof.static_sha256);
        proof_bytes[32..64].copy_from_slice(&self.proof.query_sha256);
        proof_bytes[64..72].copy_from_slice(&self.proof.queries.to_le_bytes());
        proof_bytes[72..80].copy_from_slice(&self.proof.minimum_scalar.to_le_bytes());
        proof_bytes[80..88].copy_from_slice(&self.proof.maximum_scalar.to_le_bytes());
        proof_bytes[88..96].copy_from_slice(&self.proof.control_work.to_le_bytes());
        let mut proof_hash = Sha256::new();
        proof_hash.update(b"RustEDA.G1.range-proof.v1\0");
        proof_hash.update(proof_bytes);
        let range_digest = proof_hash.finalize().into();
        self.device
            .sync()
            .map_err(|e| driver(Stage::Finish, None, e))?;
        drop(self.static_dev);
        drop(self.queries_dev);
        drop(self.outcomes_dev);
        drop(self.activation_dev);
        self.device
            .close()
            .map_err(|e| driver(Stage::Finish, None, e))?;
        Ok(CudaActivation {
            marked,
            stats: self.stats,
            range_proof: self.proof,
            range_digest,
            outcome_digest: self.transcript.finish(),
            device: device_stats,
        })
    }
}

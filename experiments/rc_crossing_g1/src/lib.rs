// SPDX-License-Identifier: Apache-2.0
//! Isolated, integer CSR/template activation component. No production dispatch.
//! Working CPU qualification must precede any new device kernel.
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt;

pub mod format;
pub mod session;
pub mod session_format;

pub const MAX_VIEW_BYTES: u64 = 64 << 20;
pub const MAX_OUTPUT_BYTES: u64 = 128 << 10;
pub const MAX_AUX_BYTES: u64 = 16 << 20;
pub const MAX_INSTANCES: usize = 100_000;
pub const MAX_TEMPLATES: usize = 1024;
pub const MAX_RECTANGLES: usize = 16_384;
pub const MAX_GRID_CELLS: usize = 1_000_000;
pub const MAX_ENTRIES: usize = 4_000_000;
pub const MAX_QUERIES: usize = 65_536;
pub const MAX_VIRTUAL: usize = 1_000_000;
pub const MAX_WORK: u64 = 100_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Budget(&'static str),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "INVALID_VIEW:{s}"),
            Self::Budget(s) => write!(f, "BUDGET:{s}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_work: u64,
    pub max_threads: usize,
    pub max_view_bytes: u64,
    pub max_output_bytes: u64,
    pub max_aux_bytes: u64,
    pub max_query_cells: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_work: MAX_WORK,
            max_threads: 2,
            max_view_bytes: MAX_VIEW_BYTES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_aux_bytes: MAX_AUX_BYTES,
            max_query_cells: MAX_GRID_CELLS as u64,
        }
    }
}
impl Limits {
    pub fn validate(self) -> Result<()> {
        if self.max_work > MAX_WORK
            || !(1..=4).contains(&self.max_threads)
            || !(1..=MAX_VIEW_BYTES).contains(&self.max_view_bytes)
            || !(128..=MAX_OUTPUT_BYTES).contains(&self.max_output_bytes)
            || !(1..=MAX_AUX_BYTES).contains(&self.max_aux_bytes)
            || !(1..=MAX_GRID_CELLS as u64).contains(&self.max_query_cells)
        {
            return Err(Error::Budget("hard limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub lx: i64,
    pub ly: i64,
    pub ux: i64,
    pub uy: i64,
}
impl Rect {
    pub fn positive(self) -> bool {
        self.lx < self.ux && self.ly < self.uy
    }
    pub fn overlaps(self, other: Self) -> bool {
        self.lx < other.ux && other.lx < self.ux && self.ly < other.uy && other.ly < self.uy
    }
    fn expanded(self) -> Result<Self> {
        Ok(Self {
            lx: self
                .lx
                .checked_sub(1)
                .ok_or(Error::Budget("query coordinate"))?,
            ly: self
                .ly
                .checked_sub(1)
                .ok_or(Error::Budget("query coordinate"))?,
            ux: self
                .ux
                .checked_add(1)
                .ok_or(Error::Budget("query coordinate"))?,
            uy: self
                .uy
                .checked_add(1)
                .ok_or(Error::Budget("query coordinate"))?,
        })
    }
    fn union(self, b: Self) -> Self {
        Self {
            lx: self.lx.min(b.lx),
            ly: self.ly.min(b.ly),
            ux: self.ux.max(b.ux),
            uy: self.uy.max(b.uy),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    N,
    Fs,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance {
    pub at: [i64; 2],
    pub height: i64,
    pub template: u32,
    pub first: u32,
    pub bounds: Rect,
    pub orientation: Orientation,
}
impl Instance {
    pub fn world(self, rect: Rect) -> Result<Rect> {
        let x = |v| {
            i64::try_from(i128::from(self.at[0]) + i128::from(v))
                .map_err(|_| Error::Budget("world x"))
        };
        let y = |v| {
            i64::try_from(i128::from(self.at[1]) + i128::from(v))
                .map_err(|_| Error::Budget("world y"))
        };
        let r = match self.orientation {
            Orientation::N => Rect {
                lx: x(rect.lx)?,
                ux: x(rect.ux)?,
                ly: y(rect.ly)?,
                uy: y(rect.uy)?,
            },
            Orientation::Fs => {
                let fs = |v| {
                    i64::try_from(i128::from(self.at[1]) + i128::from(self.height) - i128::from(v))
                        .map_err(|_| Error::Budget("world FS y"))
                };
                Rect {
                    lx: x(rect.lx)?,
                    ux: x(rect.ux)?,
                    ly: fs(rect.uy)?,
                    uy: fs(rect.ly)?,
                }
            }
        };
        Ok(r)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateRect {
    pub rect: Rect,
    pub layer: u32,
    pub ordinal: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query {
    pub rect: Rect,
    pub layer: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub origin: [i64; 2],
    pub bin: i64,
    pub width: u32,
    pub height: u32,
}
impl Grid {
    fn cells(self) -> Result<usize> {
        if self.bin <= 0 || self.width == 0 || self.height == 0 {
            return Err(Error::Invalid("grid dimensions"));
        }
        let count = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(Error::Budget("grid size"))?;
        if count > MAX_GRID_CELLS {
            return Err(Error::Budget("grid cells"));
        }
        self.ends()?;
        Ok(count)
    }
    fn ends(self) -> Result<[i64; 2]> {
        Ok([
            i64::try_from(i128::from(self.origin[0]) + i128::from(self.width) - 1)
                .map_err(|_| Error::Budget("grid x"))?,
            i64::try_from(i128::from(self.origin[1]) + i128::from(self.height) - 1)
                .map_err(|_| Error::Budget("grid y"))?,
        ])
    }
    fn index(self, x: i64, y: i64) -> Option<usize> {
        let dx = i128::from(x) - i128::from(self.origin[0]);
        let dy = i128::from(y) - i128::from(self.origin[1]);
        if dx < 0 || dy < 0 || dx >= i128::from(self.width) || dy >= i128::from(self.height) {
            return None;
        }
        usize::try_from(dy * i128::from(self.width) + dx).ok()
    }
    fn span(self, r: Rect) -> [i64; 4] {
        [
            r.lx.div_euclid(self.bin),
            r.ly.div_euclid(self.bin),
            r.ux.div_euclid(self.bin),
            r.uy.div_euclid(self.bin),
        ]
    }
}

/// Fixed-endian record access. Byte-backed views never form unaligned references.
pub trait Record: Copy {
    const BYTES: usize;
    fn decode(bytes: &[u8]) -> Result<Self>;
    fn encode(self, bytes: &mut [u8]) -> Result<()>;
}
#[derive(Clone, Copy, Debug)]
pub enum Table<'a, T> {
    Native(&'a [T]),
    Bytes(&'a [u8]),
}
impl<'a, T: Record> Table<'a, T> {
    pub fn bytes(bytes: &'a [u8]) -> Result<Self> {
        if T::BYTES == 0 || !bytes.len().is_multiple_of(T::BYTES) {
            return Err(Error::Invalid("record stride"));
        }
        Ok(Self::Bytes(bytes))
    }
    pub fn len(self) -> usize {
        match self {
            Self::Native(v) => v.len(),
            Self::Bytes(v) => v.len() / T::BYTES,
        }
    }
    fn valid_stride(self) -> Result<()> {
        if T::BYTES == 0 || matches!(self, Self::Bytes(v) if !v.len().is_multiple_of(T::BYTES)) {
            return Err(Error::Invalid("record stride"));
        }
        Ok(())
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn get(self, i: usize) -> Result<T> {
        match self {
            Self::Native(v) => v.get(i).copied().ok_or(Error::Invalid("record index")),
            Self::Bytes(v) => {
                let start = i
                    .checked_mul(T::BYTES)
                    .ok_or(Error::Budget("record offset"))?;
                let end = start
                    .checked_add(T::BYTES)
                    .ok_or(Error::Budget("record end"))?;
                T::decode(v.get(start..end).ok_or(Error::Invalid("record extent"))?)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pub grid: Grid,
    pub layer_count: u32,
    pub virtual_count: usize,
    pub offsets: Table<'a, u32>,
    pub entries: Table<'a, u32>,
    pub instances: Table<'a, Instance>,
    pub template_offsets: Table<'a, u32>,
    pub rectangles: Table<'a, TemplateRect>,
    pub queries: Table<'a, Query>,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Admission {
    pub view_bytes: u64,
    pub aux_bytes: u64,
    pub output_bytes: u64,
    pub validation_work: u64,
    pub reserved_work: u64,
    pub workers: usize,
    pub layer_mask: u32,
}
#[derive(Clone, Copy, Debug)]
struct Work {
    used: u64,
    cap: u64,
}
impl Work {
    fn charge(&mut self, n: u64) -> Result<()> {
        self.used = self
            .used
            .checked_add(n)
            .filter(|&v| v <= self.cap)
            .ok_or(Error::Budget("aggregate work"))?;
        Ok(())
    }
}
fn filled<T: Clone>(n: usize, value: T) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| Error::Budget("allocation"))?;
    out.resize(n, value);
    Ok(out)
}
fn area(span: [i64; 4]) -> Result<u64> {
    let x = i128::from(span[2]) - i128::from(span[0]) + 1;
    let y = i128::from(span[3]) - i128::from(span[1]) + 1;
    let product = x
        .checked_mul(y)
        .ok_or(Error::Budget("bin extent multiplication"))?;
    u64::try_from(product).map_err(|_| Error::Budget("bin extent"))
}
fn reserve(work: &mut u64, n: u64, cap: u64) -> Result<()> {
    *work = work
        .checked_add(n)
        .filter(|&v| v <= cap)
        .ok_or(Error::Budget("parallel work reservation"))?;
    Ok(())
}

/// Validate complete CSR coverage in O(B+E+I+T+Q), never Q×I.
pub fn admit(view: View<'_>, limits: Limits) -> Result<Admission> {
    limits.validate()?;
    view.offsets.valid_stride()?;
    view.entries.valid_stride()?;
    view.instances.valid_stride()?;
    view.template_offsets.valid_stride()?;
    view.rectangles.valid_stride()?;
    view.queries.valid_stride()?;
    let cells = view.grid.cells()?;
    let ni = view.instances.len();
    let nr = view.rectangles.len();
    let nq = view.queries.len();
    let nt = view
        .template_offsets
        .len()
        .checked_sub(1)
        .ok_or(Error::Invalid("template ranges"))?;
    if ni > MAX_INSTANCES
        || nr > MAX_RECTANGLES
        || nq > MAX_QUERIES
        || nt > MAX_TEMPLATES
        || view.entries.len() > MAX_ENTRIES
        || view.virtual_count > MAX_VIRTUAL
        || view.layer_count == 0
        || view.layer_count > 32
        || view.offsets.len() != cells + 1
    {
        return Err(Error::Budget("hard object bounds"));
    }
    let mut view_bytes = 4096u64 + 6 * 63;
    for (n, width) in [
        (view.offsets.len(), 4u64),
        (view.entries.len(), 4),
        (ni, 96),
        (view.template_offsets.len(), 4),
        (nr, 48),
        (nq, 40),
    ] {
        view_bytes = view_bytes
            .checked_add(
                (n as u64)
                    .checked_mul(width)
                    .ok_or(Error::Budget("view size"))?,
            )
            .ok_or(Error::Budget("view size"))?;
    }
    let workers = limits.max_threads.min(nq.max(1));
    let words = view.virtual_count.div_ceil(64);
    let aux_bytes = (ni as u64)
        .saturating_mul(4)
        .saturating_add((nt as u64).saturating_mul(size_of::<Option<Rect>>() as u64))
        .saturating_add((nq as u64).saturating_mul(size_of::<QueryOutcome>() as u64))
        .saturating_add(
            (words as u64)
                .saturating_mul(8)
                .saturating_mul(workers as u64 + 1),
        )
        .saturating_add((workers as u64).saturating_mul(size_of::<Worker>() as u64))
        .saturating_add(4096);
    let output_bytes = (words as u64)
        .checked_mul(8)
        .and_then(|v| v.checked_add(128 + 2048 + 512 + 1024 + 5))
        .ok_or(Error::Budget("output size"))?;
    if view_bytes > limits.max_view_bytes
        || aux_bytes > limits.max_aux_bytes
        || output_bytes > limits.max_output_bytes
    {
        return Err(Error::Budget("view/aux/output allowance"));
    }
    let mut work = Work {
        used: 0,
        cap: limits.max_work,
    };
    work.charge(1 + ni as u64 + nt as u64)?; // fixed vector initialization
    let mut counts = filled(ni, 0u32)?;
    let mut bounds = filled(nt, None::<Rect>)?;
    if view.offsets.get(0)? != 0
        || view.offsets.get(cells)? as usize != view.entries.len()
        || view.template_offsets.get(0)? != 0
        || view.template_offsets.get(nt)? as usize != nr
    {
        return Err(Error::Invalid("CSR/range endpoints"));
    }
    let mut layer_mask = 0u32;
    for (t, bound) in bounds.iter_mut().enumerate() {
        work.charge(1)?;
        let start = view.template_offsets.get(t)? as usize;
        let end = view.template_offsets.get(t + 1)? as usize;
        if start > end || end > nr {
            return Err(Error::Invalid("template offsets"));
        }
        for j in start..end {
            work.charge(1)?;
            let r = view.rectangles.get(j)?;
            if !r.rect.positive() || r.layer >= view.layer_count || r.ordinal as usize != j - start
            {
                return Err(Error::Invalid("template rectangle/layer/ordinal"));
            }
            layer_mask |= 1u32 << r.layer;
            *bound = Some((*bound).map_or(r.rect, |b| b.union(r.rect)));
        }
    }
    let ends = view.grid.ends()?;
    let mut prefix = 0usize;
    for i in 0..ni {
        work.charge(1)?;
        let instance = view.instances.get(i)?;
        let t = instance.template as usize;
        if t >= nt || instance.height <= 0 || instance.first as usize != prefix {
            return Err(Error::Invalid("instance/template/prefix"));
        }
        let count = (view.template_offsets.get(t + 1)? - view.template_offsets.get(t)?) as usize;
        prefix = prefix
            .checked_add(count)
            .filter(|&n| n <= MAX_VIRTUAL)
            .ok_or(Error::Budget("virtual prefix"))?;
        if let Some(local) = bounds[t] {
            if instance.world(local)? != instance.bounds {
                return Err(Error::Invalid("instance world bound"));
            }
            let span = view.grid.span(instance.bounds);
            if span[0] < view.grid.origin[0]
                || span[1] < view.grid.origin[1]
                || span[2] > ends[0]
                || span[3] > ends[1]
                || area(span)? > MAX_ENTRIES as u64
            {
                return Err(Error::Budget("instance grid coverage"));
            }
        } else if instance.bounds
            != (Rect {
                lx: 0,
                ly: 0,
                ux: 0,
                uy: 0,
            })
        {
            return Err(Error::Invalid("empty template bound"));
        }
    }
    if prefix != view.virtual_count {
        return Err(Error::Invalid("virtual total"));
    }
    for cell in 0..cells {
        work.charge(1)?;
        let start = view.offsets.get(cell)? as usize;
        let end = view.offsets.get(cell + 1)? as usize;
        if start > end || end > view.entries.len() {
            return Err(Error::Invalid("CSR offsets"));
        }
        let x = view.grid.origin[0] + (cell % view.grid.width as usize) as i64;
        let y = view.grid.origin[1] + (cell / view.grid.width as usize) as i64;
        let mut previous = None;
        for entry in start..end {
            work.charge(1)?;
            let id = view.entries.get(entry)? as usize;
            if id >= ni || previous.is_some_and(|p| p >= id) {
                return Err(Error::Invalid("CSR IDs must be unique/sorted"));
            }
            previous = Some(id);
            let instance = view.instances.get(id)?;
            let span = view.grid.span(instance.bounds);
            if !instance.bounds.positive()
                || x < span[0]
                || x > span[2]
                || y < span[1]
                || y > span[3]
            {
                return Err(Error::Invalid("CSR membership"));
            }
            counts[id] = counts[id]
                .checked_add(1)
                .ok_or(Error::Budget("CSR membership count"))?;
        }
    }
    for (i, &count) in counts.iter().enumerate() {
        work.charge(1)?;
        let b = view.instances.get(i)?.bounds;
        let expected = if b.positive() {
            area(view.grid.span(b))?
        } else {
            0
        };
        if u64::from(count) != expected {
            return Err(Error::Invalid("CSR missing coverage"));
        }
    }
    let mut reserved = work.used;
    // Every new bit/result initialization and ordered OR is reserved up front.
    reserve(
        &mut reserved,
        (words as u64)
            .saturating_mul(workers as u64)
            .saturating_mul(2)
            .saturating_add(nq as u64),
        limits.max_work,
    )?;
    for q in 0..nq {
        work.charge(1)?;
        let query = view.queries.get(q)?;
        let upper = query_bound(view, query, layer_mask, limits.max_query_cells)?;
        reserve(
            &mut reserved,
            upper
                .checked_add(1)
                .ok_or(Error::Budget("query reservation"))?,
            limits.max_work,
        )?;
    }
    // The validation query-loop charges were included by the +1 reservation.
    if reserved < work.used {
        return Err(Error::Budget("validation reservation"));
    }
    Ok(Admission {
        view_bytes,
        aux_bytes,
        output_bytes,
        validation_work: work.used,
        reserved_work: reserved,
        workers,
        layer_mask,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct QueryOutcome {
    work: u64,
    error: Option<Error>,
}
#[derive(Debug)]
struct Worker {
    marked: Vec<u64>,
}
#[derive(Debug, Serialize)]
pub struct Stats {
    pub validation_work: u64,
    pub query_work: u64,
    pub control_work: u64,
    pub total_work: u64,
    pub reserved_work: u64,
    pub workers: usize,
    pub queries: usize,
    pub virtual_shapes: usize,
    pub active_shapes: usize,
    pub view_bytes: u64,
    pub aux_bytes: u64,
}
#[derive(Debug)]
pub struct Activation {
    pub marked: Vec<u64>,
    pub stats: Stats,
}

fn query_bound(view: View<'_>, query: Query, layer_mask: u32, max_query_cells: u64) -> Result<u64> {
    if !query.rect.positive() || query.layer >= view.layer_count {
        return Err(Error::Invalid("query rectangle/layer"));
    }
    let count = area(view.grid.span(query.rect.expanded()?))?;
    if count > max_query_cells {
        return Err(Error::Budget("query extent"));
    }
    if layer_mask & (1u32 << query.layer) == 0 {
        return Ok(1);
    }
    1u64.checked_add(count)
        .and_then(|v| v.checked_add(view.entries.len() as u64))
        .and_then(|v| v.checked_add(view.instances.len() as u64))
        .and_then(|v| v.checked_add(view.virtual_count as u64))
        .ok_or(Error::Budget("query work bound"))
}

fn query_measured(
    view: View<'_>,
    q: Query,
    marked: &mut [u64],
    mask: u32,
    cap: u64,
) -> QueryOutcome {
    let mut work = Work { used: 0, cap };
    let result = (|| -> Result<()> {
        work.charge(1)?;
        if mask & (1u32 << q.layer) == 0 {
            return Ok(());
        }
        let expanded = q.rect.expanded()?;
        let range = view.grid.span(expanded);
        for x in range[0]..=range[2] {
            for y in range[1]..=range[3] {
                work.charge(1)?;
                let Some(cell) = view.grid.index(x, y) else {
                    continue;
                };
                for e in view.offsets.get(cell)? as usize..view.offsets.get(cell + 1)? as usize {
                    work.charge(1)?;
                    let id = view.entries.get(e)? as usize;
                    let instance = view.instances.get(id)?;
                    let span = view.grid.span(instance.bounds);
                    if x != range[0].max(span[0]) || y != range[1].max(span[1]) {
                        continue;
                    }
                    work.charge(1)?;
                    if !expanded.overlaps(instance.bounds) {
                        continue;
                    }
                    let t = instance.template as usize;
                    for r in view.template_offsets.get(t)? as usize
                        ..view.template_offsets.get(t + 1)? as usize
                    {
                        work.charge(1)?;
                        let rect = view.rectangles.get(r)?;
                        if rect.layer == q.layer && q.rect.overlaps(instance.world(rect.rect)?) {
                            let bit = (instance.first as usize)
                                .checked_add(rect.ordinal as usize)
                                .filter(|&n| n < view.virtual_count)
                                .ok_or(Error::Invalid("activation ordinal"))?;
                            let word = marked
                                .get_mut(bit / 64)
                                .ok_or(Error::Invalid("activation extent"))?;
                            *word |= 1u64 << (bit % 64);
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    QueryOutcome {
        work: work.used,
        error: result.err(),
    }
}

fn query(view: View<'_>, q: Query, marked: &mut [u64], mask: u32, cap: u64) -> Result<u64> {
    let outcome = query_measured(view, q, marked, mask, cap);
    match outcome.error {
        Some(error) => Err(error),
        None => Ok(outcome.work),
    }
}

/// Pure Rust deterministic integer reference. No geometry/input clone per worker.
/// Conservative batch work is reserved before any parallel query executes.
pub fn cpu_ref(view: View<'_>, limits: Limits) -> Result<Activation> {
    let admission = admit(view, limits)?;
    let words = view.virtual_count.div_ceil(64);
    let mut workers = Vec::new();
    workers
        .try_reserve_exact(admission.workers)
        .map_err(|_| Error::Budget("worker headers"))?;
    for _ in 0..admission.workers {
        workers.push(Worker {
            marked: filled(words, 0u64)?,
        });
    }
    let mut outcomes = filled(view.queries.len(), QueryOutcome::default())?;
    let allocated = workers
        .iter()
        .map(|w| (w.marked.capacity() as u64) * 8)
        .sum::<u64>()
        .saturating_add(
            (outcomes.capacity() as u64).saturating_mul(size_of::<QueryOutcome>() as u64),
        )
        .saturating_add((workers.capacity() as u64).saturating_mul(size_of::<Worker>() as u64))
        .saturating_add(4096);
    if allocated > admission.aux_bytes || allocated > limits.max_aux_bytes {
        return Err(Error::Budget("actual aux capacities"));
    }
    if !outcomes.is_empty() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(admission.workers)
            .stack_size(2 << 20)
            .build()
            .map_err(|_| Error::Budget("CPU worker pool"))?;
        let chunk = outcomes.len().div_ceil(admission.workers);
        pool.install(|| {
            outcomes
                .par_chunks_mut(chunk)
                .zip(workers.par_iter_mut())
                .enumerate()
                .for_each(|(part, (slots, worker))| {
                    for (j, slot) in slots.iter_mut().enumerate() {
                        let q = part * chunk + j;
                        match view.queries.get(q).and_then(|q| {
                            query(
                                view,
                                q,
                                &mut worker.marked,
                                admission.layer_mask,
                                limits.max_work,
                            )
                        }) {
                            Ok(work) => slot.work = work,
                            Err(error) => slot.error = Some(error),
                        }
                    }
                })
        });
    }
    let mut query_work = 0u64;
    // Query order, not worker completion, chooses the diagnostic.
    for outcome in &outcomes {
        if let Some(error) = outcome.error {
            return Err(error);
        }
        query_work = query_work
            .checked_add(outcome.work)
            .ok_or(Error::Budget("ordered query work"))?;
    }
    let control_work = (words as u64)
        .checked_mul(admission.workers as u64)
        .and_then(|v| v.checked_mul(2))
        .and_then(|v| v.checked_add(outcomes.len() as u64))
        .ok_or(Error::Budget("control work"))?;
    let total_work = admission
        .validation_work
        .checked_add(query_work)
        .and_then(|v| v.checked_add(control_work))
        .filter(|&v| v <= admission.reserved_work && v <= limits.max_work)
        .ok_or(Error::Budget("aggregate work"))?;
    let (first, others) = workers
        .split_first_mut()
        .ok_or(Error::Budget("worker count"))?;
    for other in others {
        for (a, b) in first.marked.iter_mut().zip(&other.marked) {
            *a |= *b;
        }
    }
    let active_shapes = first
        .marked
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum();
    let marked = std::mem::take(&mut first.marked);
    Ok(Activation {
        marked,
        stats: Stats {
            validation_work: admission.validation_work,
            query_work,
            control_work,
            total_work,
            reserved_work: admission.reserved_work,
            workers: admission.workers,
            queries: view.queries.len(),
            virtual_shapes: view.virtual_count,
            active_shapes,
            view_bytes: admission.view_bytes,
            aux_bytes: admission.aux_bytes,
        },
    })
}

#[cfg(test)]
mod session_meter_tests {
    use super::*;

    #[test]
    fn failed_query_retains_charged_prefix_without_changing_v1_error() {
        let rect = Rect {
            lx: 0,
            ly: 0,
            ux: 10,
            uy: 10,
        };
        let offsets = [0, 1];
        let entries = [0];
        let template_offsets = [0, 1];
        let instances = [Instance {
            at: [0, 0],
            height: 10,
            template: 0,
            first: 0,
            bounds: rect,
            orientation: Orientation::N,
        }];
        let rectangles = [TemplateRect {
            rect,
            layer: 0,
            ordinal: 0,
        }];
        let view = View {
            grid: Grid {
                origin: [0, 0],
                bin: 1000,
                width: 1,
                height: 1,
            },
            layer_count: 1,
            virtual_count: 1,
            offsets: Table::Native(&offsets),
            entries: Table::Native(&entries),
            instances: Table::Native(&instances),
            template_offsets: Table::Native(&template_offsets),
            rectangles: Table::Native(&rectangles),
            queries: Table::Native(&[]),
        };
        let q = Query {
            rect: Rect {
                lx: 1,
                ly: 1,
                ux: 2,
                uy: 2,
            },
            layer: 0,
        };
        let mut bits = [0];
        let outcome = query_measured(view, q, &mut bits, 1, 1);
        assert_eq!(outcome.work, 1);
        assert_eq!(outcome.error, Some(Error::Budget("aggregate work")));
        assert_eq!(bits, [0]);
        assert_eq!(
            query(view, q, &mut bits, 1, 1).unwrap_err(),
            outcome.error.unwrap()
        );
    }
}

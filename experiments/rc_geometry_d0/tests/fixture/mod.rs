//! Bounded D0 data packing, not a geometry oracle. Max fixture <128 MiB.
use rusteda_rc_geometry_d0::{Batch, Queries, Shapes};

pub struct Fixture {
    sb: Vec<i64>,
    sl: Vec<u32>,
    qb: Vec<i64>,
    ql: Vec<u32>,
    excluded: Vec<u32>,
    pairs: Vec<u32>,
}

impl Fixture {
    pub fn new(shapes: usize, queries: usize, pairs: usize) -> Self {
        assert!(
            (1..=32_768).contains(&shapes) && (1..=2048).contains(&queries) && pairs <= 8_000_000
        );
        assert!(pairs.div_ceil(queries) <= 65_536);
        let mut sb = vec![0; 4 * shapes];
        let mut sl = vec![0; shapes];
        for i in 0..shapes {
            let x = (i % 256) as i64 * 16 - 2048;
            let y = (i / 256) as i64 * 16 - 2048;
            for (axis, value) in [x, y, x + 8, y + 8].into_iter().enumerate() {
                sb[axis * shapes + i] = value;
            }
            sl[i] = (i % 3) as u32;
        }
        let mut qb = vec![0; 4 * queries];
        let mut ql = vec![0; queries];
        let mut excluded = vec![u32::MAX; queries];
        for q in 0..queries {
            let s = q * 7 % shapes;
            for axis in 0..4 {
                qb[axis * queries + q] = sb[axis * shapes + s];
            }
            ql[q] = sl[s];
            if q % 11 == 0 {
                excluded[q] = s as u32;
            }
        }
        let mut packed = Vec::with_capacity(2 * pairs);
        for i in 0..pairs {
            let q = i % queries;
            let base = q * 7 % shapes;
            let s = match i % 4 {
                0 | 3 => base,
                1 => (base + 1) % shapes,
                _ => (i * 13 + 3) % shapes,
            };
            packed.extend([q as u32, s as u32]);
        }
        Self {
            sb,
            sl,
            qb,
            ql,
            excluded,
            pairs: packed,
        }
    }

    pub fn batch(&self) -> Batch<'_> {
        Batch {
            shapes: Shapes {
                bounds: &self.sb,
                layers: &self.sl,
            },
            queries: Queries {
                bounds: &self.qb,
                layers: &self.ql,
                excluded_shape_index: &self.excluded,
            },
            pairs: &self.pairs,
        }
    }
}

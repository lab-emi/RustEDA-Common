// SPDX-License-Identifier: Apache-2.0
//! Tiny manual/integer oracle fixtures. No RustROAD or GPU gate is implied.
//! Upfront: <=16 instances, <=256 bins, <=32 queries, <=16 template shapes;
//! streamed oracle <=8192 predicates, private arrays <64KiB. Two test workers
//! can each make <=4 Rayon2MiB stacks; whole CPU suite estimate<128MiB.
use rusteda_rc_crossing_g1::{
    Admission, Error, Grid, Instance, Limits, Orientation, Query, Rect, Table, TemplateRect, View,
    admit, cpu_ref, format,
};

#[derive(Clone)]
struct Fixture {
    grid: Grid,
    layers: u32,
    virtuals: usize,
    offsets: Vec<u32>,
    entries: Vec<u32>,
    instances: Vec<Instance>,
    ranges: Vec<u32>,
    rectangles: Vec<TemplateRect>,
    queries: Vec<Query>,
}
impl Fixture {
    fn view(&self) -> View<'_> {
        View {
            grid: self.grid,
            layer_count: self.layers,
            virtual_count: self.virtuals,
            offsets: Table::Native(&self.offsets),
            entries: Table::Native(&self.entries),
            instances: Table::Native(&self.instances),
            template_offsets: Table::Native(&self.ranges),
            rectangles: Table::Native(&self.rectangles),
            queries: Table::Native(&self.queries),
        }
    }
    fn bins(&mut self) {
        let cells = self.grid.width as usize * self.grid.height as usize;
        assert!(cells <= 256 && self.instances.len() <= 16);
        self.offsets = vec![0];
        self.entries.clear();
        for y in 0..self.grid.height {
            for x in 0..self.grid.width {
                let bx = self.grid.origin[0] + i64::from(x);
                let by = self.grid.origin[1] + i64::from(y);
                for (i, instance) in self.instances.iter().enumerate() {
                    let r = instance.bounds;
                    if r.lx < r.ux
                        && r.ly < r.uy
                        && bx >= r.lx.div_euclid(self.grid.bin)
                        && bx <= r.ux.div_euclid(self.grid.bin)
                        && by >= r.ly.div_euclid(self.grid.bin)
                        && by <= r.uy.div_euclid(self.grid.bin)
                    {
                        self.entries.push(i as u32);
                    }
                }
                self.offsets.push(self.entries.len() as u32);
            }
        }
    }
}
fn rect(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
    Rect { lx, ly, ux, uy }
}
fn fixture() -> Fixture {
    let mut f = Fixture {
        grid: Grid {
            origin: [-1, -1],
            bin: 10,
            width: 4,
            height: 4,
        },
        layers: 3,
        virtuals: 4,
        offsets: vec![],
        entries: vec![],
        ranges: vec![0, 2, 2],
        rectangles: vec![
            TemplateRect {
                rect: rect(0, 0, 4, 2),
                layer: 0,
                ordinal: 0,
            },
            TemplateRect {
                rect: rect(0, 2, 4, 4),
                layer: 1,
                ordinal: 1,
            },
        ],
        instances: vec![
            Instance {
                at: [0, 0],
                height: 4,
                template: 0,
                first: 0,
                bounds: rect(0, 0, 4, 4),
                orientation: Orientation::N,
            },
            Instance {
                at: [10, 0],
                height: 4,
                template: 0,
                first: 2,
                bounds: rect(10, 0, 14, 4),
                orientation: Orientation::Fs,
            },
            Instance {
                at: [0, 0],
                height: 4,
                template: 1,
                first: 4,
                bounds: rect(0, 0, 0, 0),
                orientation: Orientation::N,
            },
        ],
        queries: vec![
            Query {
                rect: rect(-1, -1, 2, 1),
                layer: 0,
            },
            Query {
                rect: rect(10, -1, 14, 1),
                layer: 1,
            },
            Query {
                rect: rect(4, 0, 6, 2),
                layer: 0,
            },
            Query {
                rect: rect(0, 0, 20, 5),
                layer: 2,
            },
        ],
    };
    f.bins();
    f
}
/// Independent small oracle: widened min/max interval lengths and explicit
/// transform arithmetic. It ignores CSR and never calls core Rect/world/hit.
fn oracle(f: &Fixture) -> Vec<u64> {
    assert!(
        f.instances.len() <= 16
            && f.queries.len() <= 32
            && f.rectangles.len() <= 16
            && f.virtuals <= 128
    );
    let mut result = vec![0u64; f.virtuals.div_ceil(64)];
    let mut steps = 0usize;
    for q in &f.queries {
        for i in &f.instances {
            for r in &f.rectangles
                [f.ranges[i.template as usize] as usize..f.ranges[i.template as usize + 1] as usize]
            {
                steps += 1;
                assert!(steps <= 8192);
                let x0 = i128::from(i.at[0]) + i128::from(r.rect.lx);
                let x1 = i128::from(i.at[0]) + i128::from(r.rect.ux);
                let (y0, y1) = match i.orientation {
                    Orientation::N => (
                        i128::from(i.at[1]) + i128::from(r.rect.ly),
                        i128::from(i.at[1]) + i128::from(r.rect.uy),
                    ),
                    Orientation::Fs => (
                        i128::from(i.at[1]) + i128::from(i.height) - i128::from(r.rect.uy),
                        i128::from(i.at[1]) + i128::from(i.height) - i128::from(r.rect.ly),
                    ),
                };
                let width = i128::from(q.rect.ux).min(x1) - i128::from(q.rect.lx).max(x0);
                let height = i128::from(q.rect.uy).min(y1) - i128::from(q.rect.ly).max(y0);
                if q.layer == r.layer && width > 0 && height > 0 {
                    let bit = i.first as usize + r.ordinal as usize;
                    result[bit / 64] |= 1u64 << (bit % 64);
                }
            }
        }
    }
    result
}
fn result(f: &Fixture) -> Vec<u64> {
    cpu_ref(f.view(), Limits::default())
        .expect("tiny valid fixture")
        .marked
}

#[test]
fn manual_n_fs_layers_contact_and_absent_layer() {
    let f = fixture();
    assert_eq!(oracle(&f), vec![0b1001]);
    assert_eq!(result(&f), vec![0b1001]);
    for (q, expected) in f.queries.iter().zip([1u64, 8, 0, 0]) {
        let mut one = f.clone();
        one.queries = vec![*q];
        assert_eq!(oracle(&one), vec![expected]);
        assert_eq!(result(&one), vec![expected]);
    }
}
#[test]
fn workers_and_query_permutation_preserve_canonical_bits() {
    let mut f = fixture();
    let expected = oracle(&f);
    for workers in [1, 2, 4] {
        let limits = Limits {
            max_threads: workers,
            ..Limits::default()
        };
        let actual = cpu_ref(f.view(), limits).unwrap();
        assert_eq!(actual.marked, expected);
        assert_eq!(actual.stats.active_shapes, 2);
    }
    f.queries.reverse();
    assert_eq!(result(&f), expected);
}
#[test]
fn repeated_query_and_multibin_instance_do_not_duplicate_activation() {
    let mut f = fixture();
    f.ranges = vec![0, 1];
    f.rectangles = vec![TemplateRect {
        rect: rect(0, 0, 14, 2),
        layer: 0,
        ordinal: 0,
    }];
    f.instances = vec![Instance {
        at: [0, 0],
        height: 2,
        template: 0,
        first: 0,
        bounds: rect(0, 0, 14, 2),
        orientation: Orientation::N,
    }];
    f.virtuals = 1;
    f.queries = vec![
        Query {
            rect: rect(0, 0, 14, 2),
            layer: 0
        };
        4
    ];
    f.bins();
    assert_eq!(f.entries.len(), 2);
    assert_eq!(oracle(&f), vec![1]);
    assert_eq!(result(&f), vec![1]);
}
#[test]
fn negative_bins_use_euclidean_coordinates_and_strict_contacts() {
    let mut f = fixture();
    f.instances.truncate(1);
    f.instances[0].at = [-10, -10];
    f.instances[0].bounds = rect(-10, -10, -6, -6);
    f.virtuals = 2;
    f.queries = vec![Query {
        rect: rect(-6, -10, -4, -8),
        layer: 0,
    }];
    f.bins();
    assert_eq!(oracle(&f), vec![0]);
    assert_eq!(result(&f), vec![0]);
    f.queries[0].rect.lx = -7;
    assert_eq!(oracle(&f), vec![1]);
    assert_eq!(result(&f), vec![1]);
}
#[test]
fn csr_missing_duplicate_wrong_instance_and_noncanonical_prefix_reject() {
    let f = fixture();
    let mut bad = f.clone();
    bad.entries[0] = 1;
    assert!(admit(bad.view(), Limits::default()).is_err());
    let mut bad = f.clone();
    bad.entries.remove(0);
    for v in &mut bad.offsets[6..] {
        *v -= 1;
    }
    assert!(admit(bad.view(), Limits::default()).is_err());
    let mut bad = f.clone();
    bad.entries.insert(0, 0);
    for v in &mut bad.offsets[6..] {
        *v += 1;
    }
    assert!(admit(bad.view(), Limits::default()).is_err());
    let mut bad = f.clone();
    bad.instances[1].first = 1;
    assert!(admit(bad.view(), Limits::default()).is_err());
    let mut bad = f.clone();
    bad.rectangles[1].ordinal = 0;
    assert!(admit(bad.view(), Limits::default()).is_err());
}
#[test]
fn empty_queries_empty_template_and_no_activation_are_explicit() {
    let mut f = fixture();
    f.queries.clear();
    let a = cpu_ref(f.view(), Limits::default()).unwrap();
    assert_eq!(a.marked, vec![0]);
    assert_eq!(a.stats.queries, 0);
    f.instances.truncate(1);
    f.instances[0].template = 1;
    f.instances[0].first = 0;
    f.instances[0].bounds = rect(0, 0, 0, 0);
    f.virtuals = 0;
    f.bins();
    let a = cpu_ref(f.view(), Limits::default()).unwrap();
    assert!(a.marked.is_empty());
    assert_eq!(a.stats.active_shapes, 0);
}
#[test]
fn admission_work_bytes_and_threads_enforce_boundaries() {
    let f = fixture();
    let a: Admission = admit(f.view(), Limits::default()).unwrap();
    let mut limits = Limits {
        max_work: a.reserved_work,
        ..Limits::default()
    };
    assert!(cpu_ref(f.view(), limits).is_ok());
    limits.max_work -= 1;
    assert!(matches!(cpu_ref(f.view(), limits), Err(Error::Budget(_))));
    let limits = Limits {
        max_view_bytes: 4096,
        ..Limits::default()
    };
    assert!(admit(f.view(), limits).is_err());
    let limits = Limits {
        max_aux_bytes: 1,
        ..Limits::default()
    };
    assert!(admit(f.view(), limits).is_err());
    let limits = Limits {
        max_output_bytes: 128,
        ..Limits::default()
    };
    assert!(admit(f.view(), limits).is_err());
    let limits = Limits {
        max_threads: 5,
        ..Limits::default()
    };
    assert!(admit(f.view(), limits).is_err());
}
#[test]
fn query_and_world_extremes_fail_without_wrap_or_unbounded_scan() {
    let mut f = fixture();
    f.queries[0].rect.lx = i64::MIN;
    assert!(matches!(
        admit(f.view(), Limits::default()),
        Err(Error::Budget(_))
    ));
    let mut f = fixture();
    f.instances[1].at[0] = i64::MAX - 1;
    assert!(matches!(
        admit(f.view(), Limits::default()),
        Err(Error::Budget(_))
    ));
    let mut f = fixture();
    f.grid = Grid {
        origin: [0, 0],
        bin: 1,
        width: 16,
        height: 8,
    };
    f.bins();
    f.queries = vec![Query {
        rect: rect(i64::MIN + 1, i64::MIN + 1, i64::MAX - 1, i64::MAX - 1),
        layer: 0,
    }];
    assert!(matches!(
        admit(f.view(), Limits::default()),
        Err(Error::Budget(_))
    ));
}
#[test]
fn binary_view_has_exact_native_semantics_and_rejects_unknown_alias_or_tail() {
    let f = fixture();
    let bytes = format::encode(f.view(), Limits::default()).unwrap();
    let view = format::decode(&bytes, Limits::default()).unwrap();
    assert_eq!(cpu_ref(view, Limits::default()).unwrap().marked, oracle(&f));
    let mut bad = bytes.clone();
    bad[0] = 0;
    assert!(format::decode(&bad, Limits::default()).is_err());
    let mut bad = bytes.clone();
    bad[200] = 1;
    assert!(format::decode(&bad, Limits::default()).is_err());
    let mut bad = bytes.clone();
    bad[80..88].copy_from_slice(&4096u64.to_le_bytes());
    assert!(format::decode(&bad, Limits::default()).is_err());
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(format::decode(&bad, Limits::default()).is_err());
}

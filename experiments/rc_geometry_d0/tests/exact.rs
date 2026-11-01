//! D0 tests prepared but not executed while the parent CPU timing run is active.
//! Largest fixture here: <1024 shapes/queries, <64k pairs and two flag arrays,
//! well below 8 MiB with four shared-input CPU workers. No path enumeration.
use rusteda_rc_geometry_d0::{Batch, Error, Limits, Queries, Shapes, cpu_ref, estimate};

fn soa(rectangles: &[[i64; 4]]) -> Vec<i64> {
    (0..4)
        .flat_map(|axis| rectangles.iter().map(move |r| r[axis]))
        .collect()
}

#[test]
fn strict_geometry_matches_independent_widened_interval_oracle() {
    let shapes = [
        [0, 0, 10, 10],
        [10, 0, 20, 10],
        [-10, -10, 0, 0],
        [i64::MIN, i64::MIN, i64::MIN + 8, i64::MIN + 8],
        [i64::MAX - 8, i64::MAX - 8, i64::MAX, i64::MAX],
    ];
    let queries = [
        [-1, -1, 1, 1],
        [0, 0, 10, 10],
        [10, 0, 11, 1],
        [i64::MIN, i64::MIN, i64::MAX, i64::MAX],
    ];
    let sb = soa(&shapes);
    let qb = soa(&queries);
    let sl = [0, 0, 1, 0, 0];
    let ql = [0, 0, 0, 0];
    let excluded = [u32::MAX, 0, u32::MAX, u32::MAX];
    let pairs: Vec<u32> = (0..queries.len() as u32)
        .flat_map(|q| (0..shapes.len() as u32).flat_map(move |s| [q, s]))
        .collect();
    let batch = Batch {
        shapes: Shapes {
            bounds: &sb,
            layers: &sl,
        },
        queries: Queries {
            bounds: &qb,
            layers: &ql,
            excluded_shape_index: &excluded,
        },
        pairs: &pairs,
    };
    // Independent oracle: intersection of widened positive-area intervals.
    // It does not call the shared predicate or use its endpoint inequality form.
    let expected: Vec<u32> = pairs
        .chunks_exact(2)
        .map(|pair| {
            let (q, s) = (pair[0] as usize, pair[1] as usize);
            let overlap = (0..2).all(|axis| {
                let lo = i128::from(shapes[s][axis]).max(i128::from(queries[q][axis]));
                let hi = i128::from(shapes[s][axis + 2]).min(i128::from(queries[q][axis + 2]));
                hi - lo > 0
            });
            u32::from(overlap && sl[s] == ql[q] && excluded[q] != pair[1])
        })
        .collect();
    for workers in [1, 2, 4] {
        let got = cpu_ref(
            batch,
            Limits {
                max_threads: workers,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(got, expected);
    }
}

#[test]
fn non_multiple_launch_sizes_and_duplicate_pairs_preserve_input_order() {
    let sb = soa(&[[0, 0, 10, 10], [10, 0, 20, 10]]);
    let qb = soa(&[[1, 1, 2, 2]]);
    for count in [0, 1, 127, 128, 129, 257, 513] {
        let pairs: Vec<u32> = (0..count).flat_map(|i| [0, (i % 2) as u32]).collect();
        let batch = Batch {
            shapes: Shapes {
                bounds: &sb,
                layers: &[0, 0],
            },
            queries: Queries {
                bounds: &qb,
                layers: &[0],
                excluded_shape_index: &[u32::MAX],
            },
            pairs: &pairs,
        };
        let got = cpu_ref(batch, Limits::default()).unwrap();
        assert_eq!(
            got,
            (0..count)
                .map(|i| u32::from(i % 2 == 0))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn invalid_inputs_and_work_memory_limits_fail_explicitly() {
    let sb = soa(&[[0, 0, 10, 10]]);
    let qb = soa(&[[1, 1, 2, 2]]);
    let batch = Batch {
        shapes: Shapes {
            bounds: &sb,
            layers: &[0],
        },
        queries: Queries {
            bounds: &qb,
            layers: &[0],
            excluded_shape_index: &[u32::MAX],
        },
        pairs: &[0, 0],
    };
    for pairs in [&[0u32][..], &[1, 0], &[0, 1]] {
        assert!(matches!(
            cpu_ref(Batch { pairs, ..batch }, Limits::default()),
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        cpu_ref(
            Batch {
                pairs: &[0, 0, 0, 0],
                ..batch
            },
            Limits {
                max_candidates_per_query: 1,
                ..Limits::default()
            }
        ),
        Err(Error::Budget(_))
    ));
    assert!(matches!(
        cpu_ref(
            batch,
            Limits {
                max_pairs: 0,
                ..Limits::default()
            }
        ),
        Err(Error::Budget(_))
    ));
    assert!(matches!(
        cpu_ref(
            batch,
            Limits {
                max_host_bytes: 1,
                ..Limits::default()
            }
        ),
        Err(Error::Budget(_))
    ));
    assert!(matches!(
        cpu_ref(
            batch,
            Limits {
                max_device_bytes: 1,
                ..Limits::default()
            }
        ),
        Err(Error::Budget(_))
    ));
    assert!(matches!(
        cpu_ref(
            batch,
            Limits {
                max_threads: 5,
                ..Limits::default()
            }
        ),
        Err(Error::Budget(_))
    ));
    let degenerate = soa(&[[1, 1, 1, 2]]);
    assert!(matches!(
        cpu_ref(
            Batch {
                queries: Queries {
                    bounds: &degenerate,
                    layers: &[0],
                    excluded_shape_index: &[u32::MAX]
                },
                ..batch
            },
            Limits::default()
        ),
        Err(Error::Invalid(_))
    ));
    assert!(estimate(usize::MAX, usize::MAX, usize::MAX).is_err());
}

#[test]
fn maximum_structural_estimate_fits_declared_caps_without_allocating_it() {
    let bytes = estimate(4_000_000, 65_536, 8_000_000).unwrap();
    assert_eq!(bytes.device_bytes, 242_621_440);
    assert_eq!(bytes.host_bytes, 753_678_336);
    assert!(bytes.device_bytes <= Limits::default().max_device_bytes);
    assert!(bytes.host_bytes <= Limits::default().max_host_bytes);
}

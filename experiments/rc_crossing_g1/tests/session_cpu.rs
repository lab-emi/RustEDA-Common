//! Stream/control tests. External40manual cases and actual legacy bits remain
//! the required independent geometry oracles; none of these tests ran yet.
use rusteda_rc_crossing_g1::session::{Budget, PreparedStatic, Session, SessionLimits};
use rusteda_rc_crossing_g1::session_format::{self as frame, TapeHeader, TapeLayout};
use rusteda_rc_crossing_g1::{
    Error, Grid, Instance, Orientation, Query, Rect, Table, TemplateRect, View,
};
use sha2::{Digest, Sha256};

struct Fixture {
    offsets: [u32; 2],
    entries: [u32; 1],
    instances: [Instance; 1],
    template_offsets: [u32; 2],
    rectangles: [TemplateRect; 1],
}
impl Fixture {
    fn new() -> Self {
        let rect = Rect {
            lx: 0,
            ly: 0,
            ux: 10,
            uy: 10,
        };
        Self {
            offsets: [0, 1],
            entries: [0],
            template_offsets: [0, 1],
            instances: [Instance {
                at: [0, 0],
                height: 10,
                template: 0,
                first: 0,
                bounds: rect,
                orientation: Orientation::N,
            }],
            rectangles: [TemplateRect {
                rect,
                layer: 0,
                ordinal: 0,
            }],
        }
    }
    fn view(&self) -> View<'_> {
        View {
            grid: Grid {
                origin: [0, 0],
                bin: 1000,
                width: 1,
                height: 1,
            },
            layer_count: 2,
            virtual_count: 1,
            offsets: Table::Native(&self.offsets),
            entries: Table::Native(&self.entries),
            instances: Table::Native(&self.instances),
            template_offsets: Table::Native(&self.template_offsets),
            rectangles: Table::Native(&self.rectangles),
            queries: Table::Native(&[]),
        }
    }
}
fn hit() -> Query {
    Query {
        rect: Rect {
            lx: 1,
            ly: 1,
            ux: 2,
            uy: 2,
        },
        layer: 0,
    }
}
fn run(queries: &[Query], tile: usize, threads: usize) -> (Vec<u64>, u64, u64) {
    let fixture = Fixture::new();
    let limits = SessionLimits {
        max_work: 10_000_000,
        max_threads: threads,
        max_tile_queries: tile,
        ..SessionLimits::default()
    };
    let control = Budget::control_reservation(queries.len() as u64, threads).expect("control");
    let mut budget = Budget::new(limits.max_work, 0, 0, control).expect("budget");
    let prepared = PreparedStatic::new(fixture.view(), limits, &mut budget).expect("static");
    let mut session = Session::new(prepared, limits, queries.len() as u64).expect("session");
    for window in queries.chunks(tile) {
        session.push(Table::Native(window)).expect("window");
    }
    let result = session.finish().expect("whole");
    assert_eq!(result.stats.queries_staged, queries.len() as u64);
    assert_eq!(result.stats.queries_completed, queries.len() as u64);
    let promise = budget.publication_promise(0, 0).expect("tail");
    budget.complete_success(promise).expect("closed");
    (
        result.marked,
        result.stats.stage_windows,
        result.stats.query_work,
    )
}

#[test]
fn integer_union_is_independent_of_windows_and_workers() {
    let mut queries = vec![hit(); 131];
    queries[0] = Query {
        rect: Rect {
            lx: 10,
            ly: 1,
            ux: 11,
            uy: 2,
        },
        layer: 0,
    }; // edge-only, empty
    queries[64].layer = 1; // supported but no shape on this layer
    for workers in [1, 2, 4] {
        for window in [1, 2, 3, 63, 64, 65, 131] {
            let (words, count, _) = run(&queries, window, workers);
            assert_eq!(words, [1]);
            assert_eq!(count, queries.len().div_ceil(window) as u64);
        }
    }
}
#[test]
fn one_session_crosses_the_65536_record_boundary() {
    let queries = vec![hit(); 65_537];
    let (words, windows, query_work) = run(&queries, 65_536, 4);
    assert_eq!(words, [1]);
    assert_eq!(windows, 2);
    assert_eq!(query_work, 5 * queries.len() as u64);
}
#[test]
fn empty_session_has_no_activation_or_prefix() {
    for workers in [1, 4] {
        let (words, windows, work) = run(&[], 3, workers);
        assert_eq!(words, [0]);
        assert_eq!((windows, work), (0, 0));
    }
}
#[test]
fn absent_layer_queries_do_not_visit_geometry() {
    let mut q = hit();
    q.layer = 1;
    let (words, _, work) = run(&[q; 9], 2, 4);
    assert_eq!(words, [0]);
    assert_eq!(work, 9);
}
#[test]
fn later_invalid_query_has_a_global_ordinal_for_any_segmentation() {
    for tile in [2, 3, 8] {
        let fixture = Fixture::new();
        let mut queries = vec![hit(); 8];
        queries[5] = Query {
            rect: Rect {
                lx: i64::MIN,
                ly: 1,
                ux: i64::MIN + 1,
                uy: 2,
            },
            layer: 0,
        };
        let limits = SessionLimits {
            max_tile_queries: tile,
            ..SessionLimits::default()
        };
        let mut budget = Budget::new(
            limits.max_work,
            0,
            0,
            Budget::control_reservation(8, limits.max_threads).unwrap(),
        )
        .unwrap();
        let prepared = PreparedStatic::new(fixture.view(), limits, &mut budget).unwrap();
        let mut session = Session::new(prepared, limits, 8).unwrap();
        let mut error = None;
        for chunk in queries.chunks(tile) {
            if let Err(found) = session.push(Table::Native(chunk)) {
                error = Some(found);
                break;
            }
        }
        assert_eq!(error.unwrap().query_ordinal, Some(5));
        assert_eq!(session.stats().queries_completed, 5);
        assert!(session.push(Table::Native(&[hit()])).is_err());
        assert!(session.finish().is_err());
    }
}
#[test]
fn only_unlaunched_prefix_is_shrunk_and_budget_failure_is_not_retried() {
    let fixture = Fixture::new();
    let total = 8;
    let cap = Budget::control_reservation(total, 1).unwrap();
    let limits = SessionLimits {
        max_work: cap,
        max_threads: 1,
        max_tile_queries: 8,
        ..SessionLimits::default()
    };
    let mut budget =
        Budget::new(cap, 0, 0, Budget::control_reservation(total, 1).unwrap()).unwrap();
    let prepared = PreparedStatic::new(fixture.view(), limits, &mut budget).unwrap();
    let mut session = Session::new(prepared, limits, total).unwrap();
    let expensive = Query {
        rect: Rect {
            lx: 1,
            ly: 1,
            ux: 999_000,
            uy: 999_000,
        },
        layer: 0,
    };
    let error = session.push(Table::Native(&[expensive; 8])).unwrap_err();
    assert_eq!(error.query_ordinal, Some(5));
    assert_eq!(session.stats().executed_prefixes, 1);
    assert_eq!(session.stats().queries_completed, 5);
    assert_eq!(session.stats().query_work, 5_000_020);
    assert!(session.push(Table::Native(&[hit()])).is_err());
    assert_eq!(session.stats().executed_prefixes, 1);
    drop(session); // Ends the same exclusive ledger borrow, including on failure.
    assert!(budget.completed() <= cap);
    assert!(budget.publication_promise(0, 0).is_err());
}
#[test]
fn source_framing_is_canonical_and_footer_detects_late_corruption() {
    let fixture = Fixture::new();
    let limits = SessionLimits::default();
    let raw = frame::encode_static(fixture.view(), limits).unwrap();
    assert_eq!(&raw[..8], b"RG1STA02");
    assert_eq!(u32::from_le_bytes(raw[8..12].try_into().unwrap()), 2);
    let mut changed = raw.clone();
    changed[144] = 1;
    assert!(frame::decode_static(&changed, limits).is_err());
    let mut alias = raw.clone();
    alias[80..88].copy_from_slice(&4096u64.to_le_bytes());
    assert!(frame::decode_static(&alias, limits).is_err());
    let layout = TapeLayout::from_file_bytes(8192 + 40, limits).unwrap();
    let header = TapeHeader {
        layout,
        static_sha: [1; 32],
        payload_sha: [2; 32],
    };
    let h = header.encode().unwrap();
    let sha: [u8; 32] = Sha256::digest(h).into();
    let mut footer = header.encode_footer(sha).unwrap();
    header.validate_footer(&footer, sha, [2; 32]).unwrap();
    footer[96] ^= 1;
    assert!(header.validate_footer(&footer, sha, [2; 32]).is_err());
    assert!(
        TapeHeader {
            layout: TapeLayout {
                queries: 1,
                payload_bytes: u64::MAX,
                total_bytes: 8192
            },
            ..header
        }
        .encode()
        .is_err()
    );
}
#[test]
fn conservative_reservation_and_actual_work_are_different() {
    let budget = Budget::new(100, 4, 20, 30).unwrap();
    assert_eq!(budget.completed(), 4);
    assert!(Budget::new(53, 4, 20, 30).is_err());
    let mut bad = SessionLimits {
        max_threads: 0,
        ..SessionLimits::default()
    };
    assert!(bad.validate().is_err());
    bad = SessionLimits {
        max_tile_queries: 65_537,
        ..SessionLimits::default()
    };
    assert!(bad.validate().is_err());
    assert!(matches!(
        TapeLayout::from_file_bytes(8193, SessionLimits::default()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn actual_five_section_size_honors_exact_user_cap_not_legacy_padding() {
    let fixture = Fixture::new();
    let bytes = frame::encode_static(fixture.view(), SessionLimits::default()).unwrap();
    assert_eq!(bytes.len(), 4464);
    let limits = SessionLimits {
        max_static_bytes: bytes.len() as u64,
        ..SessionLimits::default()
    };
    assert_eq!(frame::encode_static(fixture.view(), limits).unwrap(), bytes);
    let decoded = frame::decode_static(&bytes, limits).unwrap();
    let mut budget = Budget::new(
        limits.max_work,
        0,
        0,
        Budget::control_reservation(0, limits.max_threads).unwrap(),
    )
    .unwrap();
    let prepared = PreparedStatic::new(decoded.view, limits, &mut budget).unwrap();
    let result = Session::new(prepared, limits, 0).unwrap().finish().unwrap();
    assert_eq!(result.stats.actual_static_bytes, 4464);
    assert_eq!(result.stats.legacy_admission_bytes, 4638);
    let too_small = SessionLimits {
        max_static_bytes: 4463,
        ..limits
    };
    assert!(frame::decode_static(&bytes, too_small).is_err());
    assert!(frame::encode_static(fixture.view(), too_small).is_err());
    let mut budget = Budget::new(
        too_small.max_work,
        0,
        0,
        Budget::control_reservation(0, too_small.max_threads).unwrap(),
    )
    .unwrap();
    assert!(PreparedStatic::new(fixture.view(), too_small, &mut budget).is_err());
}

#[test]
fn prepared_static_and_transport_keep_one_exclusive_ledger() {
    let fixture = Fixture::new();
    let limits = SessionLimits::default();
    let control = Budget::control_reservation(1, limits.max_threads).unwrap();
    let mut budget = Budget::new(limits.max_work, 0, 3, control).unwrap();
    let prepared = PreparedStatic::new(fixture.view(), limits, &mut budget).unwrap();
    let mut session = Session::new(prepared, limits, 1).unwrap();
    session.io(3).unwrap();
    session.push(Table::Native(&[hit()])).unwrap();
    assert_eq!(session.finish().unwrap().marked, [1]);
    assert_eq!(budget.io_completed(), 3);
    // Repeating the static walk would re-use a released upper reservation.
    assert!(PreparedStatic::new(fixture.view(), limits, &mut budget).is_err());
    let promised = budget.publication_promise(0, 0).unwrap();
    budget.complete_success(promised).unwrap();
    assert!(budget.publication_promise(0, 0).is_err());

    let mut failed_budget = Budget::new(limits.max_work, 0, 0, control).unwrap();
    let prepared = PreparedStatic::new(fixture.view(), limits, &mut failed_budget).unwrap();
    let mut session = Session::new(prepared, limits, 1).unwrap();
    assert!(session.io(1).is_err());
    assert!(session.push(Table::Native(&[hit()])).is_err());
    assert!(session.finish().is_err());
}

#[test]
fn dropped_static_preparation_cannot_repeat_its_reserved_walk() {
    let fixture = Fixture::new();
    let limits = SessionLimits::default();
    let control = Budget::control_reservation(0, limits.max_threads).unwrap();
    let mut budget = Budget::new(limits.max_work, 0, 0, control).unwrap();
    {
        let _prepared = PreparedStatic::new(fixture.view(), limits, &mut budget).unwrap();
    }
    assert!(PreparedStatic::new(fixture.view(), limits, &mut budget).is_err());
}

// SPDX-License-Identifier: Apache-2.0
//! Required-device differential source, not an executed qualification.
//! Root must bind current source/CPU gate/PTX/tool manifests and run this binary
//! under the separate3GiB/1GiB/noAS guardian. Missing inputs/device are FAIL.
//! Largest case65537*40B plus two<=3MiB staging arrays and<=16KiB static data;
//! four test workers remain below64MiB new arrays, plus separately admitted
//! runtime/test ELF/PTX caches and the1GiB opaque CUDA driver allowance.
use rusteda_compute::cuda::VerifiedPtx;
use rusteda_compute::geometry::crossings::{cuda, outcome};
use rusteda_rc_crossing_g1::session::{Budget, PreparedStatic, Session, SessionLimits};
use rusteda_rc_crossing_g1::session_format::{decode_static, encode_static};
use rusteda_rc_crossing_g1::{Grid, Instance, Orientation, Query, Rect, Table, TemplateRect, View};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, PathBuf};

fn ptx() -> (Vec<u8>, [u8; 32]) {
    let path = PathBuf::from(
        std::env::var_os("RUSTEDA_G1_REQUIRED_PTX")
            .expect("GPU_REQUIRED: exact authorized PTX path"),
    );
    assert!(path.is_absolute());
    let mut prefix = PathBuf::new();
    for component in path.components() {
        assert!(!matches!(
            component,
            Component::ParentDir | Component::CurDir
        ));
        prefix.push(component);
        assert!(
            !fs::symlink_metadata(&prefix)
                .expect("PTX ancestor")
                .file_type()
                .is_symlink()
        );
    }
    let info = fs::symlink_metadata(&path).expect("GPU_REQUIRED: PTX missing");
    assert!(
        info.is_file()
            && info.nlink() == 1
            && info.mode() & 0o222 == 0
            && info.len() != 0
            && info.len() <= 8 << 20
    );
    let sha = std::env::var("RUSTEDA_G1_REQUIRED_PTX_SHA256")
        .expect("GPU_REQUIRED: exact authorized PTX SHA");
    assert!(
        sha.len() == 64
            && sha
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    );
    let mut expected = [0_u8; 32];
    for (i, pair) in sha.as_bytes().chunks_exact(2).enumerate() {
        let value = |c: u8| if c <= b'9' { c - b'0' } else { c - b'a' + 10 };
        expected[i] = value(pair[0]) * 16 + value(pair[1]);
    }
    let mut file = fs::File::open(path).expect("PTX open");
    let opened = file.metadata().unwrap();
    assert_eq!(
        (opened.dev(), opened.ino(), opened.len()),
        (info.dev(), info.ino(), info.len())
    );
    let mut raw = Vec::new();
    raw.try_reserve_exact(info.len() as usize).unwrap();
    file.by_ref()
        .take(info.len() + 1)
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw.len() as u64, info.len());
    assert_eq!(
        (
            file.metadata().unwrap().mtime(),
            file.metadata().unwrap().ctime()
        ),
        (info.mtime(), info.ctime())
    );
    (raw, expected)
}

fn fixture(orientation: Orientation) -> Vec<u8> {
    fixture_with(orientation, false)
}

/// `empty_template` adds a canonical instance of a template with no rectangles.
fn fixture_with(orientation: Orientation, empty_template: bool) -> Vec<u8> {
    //65 contiguous one-DBU rectangles, one instance: literal full activation is
    // [u64::MAX,1], independently including the63/64 word boundary.
    let rectangles = (0..65)
        .map(|i| TemplateRect {
            rect: Rect {
                lx: 2 * i,
                ly: 1,
                ux: 2 * i + 1,
                uy: 3,
            },
            layer: 0,
            ordinal: i as u32,
        })
        .collect::<Vec<_>>();
    let (ly, uy) = match orientation {
        Orientation::N => (1, 3),
        Orientation::Fs => (7, 9),
    };
    let mut instances = vec![Instance {
        at: [0, 0],
        height: 10,
        template: 0,
        first: 0,
        bounds: Rect {
            lx: 0,
            ly,
            ux: 129,
            uy,
        },
        orientation,
    }];
    let mut templates = vec![0, 65];
    if empty_template {
        instances.push(Instance {
            at: [0, 0],
            height: 10,
            template: 1,
            first: 65,
            bounds: Rect {
                lx: 0,
                ly: 0,
                ux: 0,
                uy: 0,
            },
            orientation,
        });
        templates.push(65);
    }
    let offsets = [0, 1];
    let entries = [0];
    let view = View {
        grid: Grid {
            origin: [0, 0],
            bin: 1000,
            width: 1,
            height: 1,
        },
        layer_count: 3,
        virtual_count: 65,
        offsets: Table::Native(&offsets),
        entries: Table::Native(&entries),
        instances: Table::Native(&instances),
        template_offsets: Table::Native(&templates),
        rectangles: Table::Native(&rectangles),
        queries: Table::Native(&[]),
    };
    encode_static(view, SessionLimits::default()).unwrap()
}

fn differential(orientation: Orientation, count: usize, tile: usize) {
    differential_on(fixture(orientation), count, tile);
}

fn differential_on(static_bytes: Vec<u8>, count: usize, tile: usize) {
    assert!(count <= 65537);
    let mut limits = SessionLimits {
        max_threads: 1,
        max_tile_queries: tile,
        ..SessionLimits::default()
    };
    // Backend control/transfer holds differ. Compare the semantic transcript,
    // never fabricate equal total IO/control or equal prefix counts.
    let gpu_control = cuda::control_reservation(count as u64).unwrap();
    let gpu_io = cuda::io_reservation(static_bytes.len() as u64, count as u64, 65).unwrap();
    limits.max_work = gpu_control + gpu_io + 10_000_000;
    let query = Query {
        rect: Rect {
            lx: 0,
            ly: 0,
            ux: 130,
            uy: 10,
        },
        layer: 0,
    };
    let queries = vec![query; count];
    let cpu_view = decode_static(&static_bytes, limits).unwrap().view;
    let mut cpu_budget = Budget::new(
        limits.max_work,
        0,
        outcome::transcript_io_reservation(count as u64).unwrap(),
        Budget::control_reservation(count as u64, 1).unwrap(),
    )
    .unwrap();
    let prepared = PreparedStatic::new(cpu_view, limits, &mut cpu_budget).unwrap();
    let mut cpu = Session::new_with_transcript(prepared, limits, count as u64).unwrap();
    for part in queries.chunks(tile) {
        cpu.push(Table::Native(part)).unwrap();
    }
    let cpu = cpu.finish().unwrap();
    assert_eq!(cpu.marked, [u64::MAX, 1]);
    let (raw_ptx, expected_ptx) = ptx();
    let mut gpu_budget = Budget::new(
        limits.max_work,
        0,
        gpu_io + 2 * (raw_ptx.len() as u64).div_ceil(64),
        gpu_control,
    )
    .unwrap();
    gpu_budget
        .io(2 * (raw_ptx.len() as u64).div_ceil(64))
        .unwrap();
    let image = VerifiedPtx::verify(raw_ptx, expected_ptx_bytes(), expected_ptx)
        .expect("GPU_REQUIRED: exact artifact");
    let gpu_view = decode_static(&static_bytes, limits).unwrap().view;
    let prepared = PreparedStatic::new(gpu_view, limits, &mut gpu_budget).unwrap();
    let mut proof = cuda::CudaPreparation::new(
        prepared,
        &static_bytes,
        limits,
        count as u64,
        Sha256::digest(&static_bytes).into(),
    )
    .unwrap();
    for part in queries.chunks(tile) {
        proof.push_range(Table::Native(part)).unwrap();
    }
    let mut gpu = cuda::CudaSession::new(proof.finish_range().unwrap(), image)
        .expect("GPU_REQUIRED: native driver/function");
    for part in queries.chunks(tile) {
        gpu.push(Table::Native(part)).unwrap();
    }
    let gpu = gpu.finish().expect("GPU_REQUIRED: sync/readback/free");
    assert_eq!(gpu.marked, [u64::MAX, 1]);
    assert_eq!(gpu.marked, cpu.marked);
    let cpu_digest = cpu.outcome_digest.unwrap();
    assert_eq!(gpu.outcome_digest.sha256, cpu_digest.sha256);
    assert_eq!(gpu.outcome_digest.records, count as u64);
    assert_eq!(gpu.outcome_digest.query_work, cpu_digest.query_work);
    assert_eq!(gpu.outcome_digest.first_error, cpu_digest.first_error);
    assert_eq!(gpu.stats.core.query_work, cpu.stats.query_work);
    assert_eq!(gpu.stats.core.workers, 0);
}
fn expected_ptx_bytes() -> u64 {
    std::env::var("RUSTEDA_G1_REQUIRED_PTX_BYTES")
        .expect("GPU_REQUIRED: bytes")
        .parse()
        .unwrap()
}

#[test]
fn required_gpu_or_matches_reference_n_fs_and_nonblock_tail() {
    differential(Orientation::N, 129, 128);
    differential(Orientation::Fs, 129, 128);
}
#[test]
fn required_gpu_segmentation_65537_matches_reference_ordered_digest() {
    differential(Orientation::N, 65537, 65536);
}

#[test]
fn required_gpu_accepts_canonical_empty_template_like_reference() {
    differential_on(fixture_with(Orientation::N, true), 4, 2);
}

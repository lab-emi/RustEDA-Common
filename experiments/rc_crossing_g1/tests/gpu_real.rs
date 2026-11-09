// SPDX-License-Identifier: Apache-2.0
//! Required-device differential on one actual captured Road workload.
//! The oracle is the captured native Road activation, independent of this crate.
//! Missing inputs or device are FAIL. Peak: static (<=32 MiB) + one 2.6 MiB query
//! window + device buffers; caller runs it under a 3 GiB/1 GiB swap scope.
use rusteda_compute::cuda::VerifiedPtx;
use rusteda_compute::geometry::crossings::{cuda, outcome};
use rusteda_rc_crossing_g1::session::{Budget, PreparedStatic, Session, SessionLimits};
use rusteda_rc_crossing_g1::session_format::{HEADER_BYTES, TapeLayout, decode_static};
use rusteda_rc_crossing_g1::{Query, Table};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

const MAX_WORK: u64 = 100_000_000_000;
const QUERY_BYTES: usize = 40;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("GPU_REQUIRED: {name}"))
}

/// Streams the tape once, one bounded window at a time.
fn windows(path: &PathBuf, layout: TapeLayout, tile: usize, mut push: impl FnMut(&[u8])) {
    let mut tape = File::open(path).unwrap();
    tape.seek(SeekFrom::Start(HEADER_BYTES as u64)).unwrap();
    let mut buffer = vec![0_u8; tile * QUERY_BYTES];
    let mut remaining = layout.queries;
    while remaining != 0 {
        let n = remaining.min(tile as u64);
        let bytes = n as usize * QUERY_BYTES;
        tape.read_exact(&mut buffer[..bytes]).unwrap();
        push(&buffer[..bytes]);
        remaining -= n;
    }
}

fn words_le(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

#[test]
fn real_capture_matches_cpu_and_native_road_at_zero_tolerance() {
    let root = PathBuf::from(required("RUSTEDA_G1_REAL_REVISION"));
    let threads: usize = required("RUSTEDA_G1_REAL_CPU_THREADS").parse().unwrap();
    let static_bytes = fs::read(root.join("static.rbin")).unwrap();
    let tape = root.join("queries.rbin");
    let oracle = fs::read(root.join("legacy-bits.rbin")).unwrap();
    let limits = SessionLimits {
        max_work: MAX_WORK,
        ..SessionLimits::default()
    };
    let layout = TapeLayout::from_file_bytes(tape.metadata().unwrap().len(), limits).unwrap();
    let tile = limits.max_tile_queries;

    let started = Instant::now();
    let cpu_limits = SessionLimits {
        max_threads: threads,
        ..limits
    };
    let view = decode_static(&static_bytes, cpu_limits).unwrap().view;
    let mut budget = Budget::new(
        MAX_WORK,
        0,
        outcome::transcript_io_reservation(layout.queries).unwrap(),
        Budget::control_reservation(layout.queries, threads).unwrap(),
    )
    .unwrap();
    let prepared = PreparedStatic::new(view, cpu_limits, &mut budget).unwrap();
    let mut session = Session::new_with_transcript(prepared, cpu_limits, layout.queries).unwrap();
    windows(&tape, layout, tile, |bytes| {
        session.push(Table::<Query>::bytes(bytes).unwrap()).unwrap();
    });
    let cpu = session.finish().unwrap();
    let cpu_seconds = started.elapsed().as_secs_f64();

    let (ptx, ptx_sha) = {
        let path = required("RUSTEDA_G1_REQUIRED_PTX");
        let raw = fs::read(path).unwrap();
        let sha: [u8; 32] = Sha256::digest(&raw).into();
        let hex: String = sha.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, required("RUSTEDA_G1_REQUIRED_PTX_SHA256"));
        (raw, sha)
    };

    let started = Instant::now();
    let gpu_limits = SessionLimits {
        max_threads: 1,
        ..limits
    };
    let control = cuda::control_reservation(layout.queries).unwrap();
    let view = decode_static(&static_bytes, gpu_limits).unwrap().view;
    let io = cuda::io_reservation(
        static_bytes.len() as u64,
        layout.queries,
        view.virtual_count as usize,
    )
    .unwrap();
    let mut budget = Budget::new(
        MAX_WORK,
        0,
        io + 2 * (ptx.len() as u64).div_ceil(64),
        control,
    )
    .unwrap();
    budget.io(2 * (ptx.len() as u64).div_ceil(64)).unwrap();
    let image = VerifiedPtx::verify(ptx.clone(), ptx.len() as u64, ptx_sha).unwrap();
    let prepared = PreparedStatic::new(view, gpu_limits, &mut budget).unwrap();
    let mut proof = cuda::CudaPreparation::new(
        prepared,
        &static_bytes,
        gpu_limits,
        layout.queries,
        Sha256::digest(&static_bytes).into(),
    )
    .unwrap();
    windows(&tape, layout, tile, |bytes| {
        proof
            .push_range(Table::<Query>::bytes(bytes).unwrap())
            .unwrap();
    });
    let ready = proof.finish_range().unwrap();
    let range_seconds = started.elapsed().as_secs_f64();
    let launch_started = Instant::now();
    let mut session = cuda::CudaSession::new(ready, image).expect("GPU_REQUIRED: device");
    let open_seconds = launch_started.elapsed().as_secs_f64();
    windows(&tape, layout, tile, |bytes| {
        session.push(Table::<Query>::bytes(bytes).unwrap()).unwrap();
    });
    let gpu = session.finish().expect("GPU_REQUIRED: readback");
    let gpu_seconds = started.elapsed().as_secs_f64();

    let cpu_digest = cpu.outcome_digest.unwrap();
    assert_eq!(gpu.marked, cpu.marked, "GPU differs from cpu_ref");
    assert_eq!(gpu.outcome_digest.sha256, cpu_digest.sha256);
    assert_eq!(gpu.outcome_digest.records, layout.queries);
    assert_eq!(gpu.outcome_digest.query_work, cpu_digest.query_work);
    assert_eq!(gpu.outcome_digest.first_error, cpu_digest.first_error);
    assert_eq!(gpu.stats.core.query_work, cpu.stats.query_work);
    assert_eq!(
        words_le(&gpu.marked),
        oracle,
        "GPU differs from native Road"
    );
    eprintln!(
        "{}",
        serde_json::json!({
            "schema": "rusteda.g1-real-device-differential.v1",
            "queries": layout.queries,
            "active_bits": gpu.marked.iter().map(|w| u64::from(w.count_ones())).sum::<u64>(),
            "query_work": cpu_digest.query_work,
            "word_tolerance": 0,
            "cpu_threads": threads,
            "cpu_seconds": cpu_seconds,
            "gpu_whole_seconds": gpu_seconds,
            "gpu_range_proof_seconds": range_seconds,
            "gpu_open_seconds": open_seconds,
            "gpu_padded_threads": gpu.stats.padded_threads,
            "device_bytes": gpu.stats.device_bytes_completed,
        })
    );
}

#![cfg(feature = "cuda")]
//! Real-device checks are explicitly selected after the CPU gate, never silent skips.
mod fixture;
use rusteda_rc_geometry_d0::{
    Batch, Limits, Queries, Shapes, cpu_ref,
    cuda::{Session, VerifiedPtx},
};
use std::path::{Path, PathBuf};

fn manifest() -> PathBuf {
    assert_eq!(std::env::var("RUSTEDA_D0_REQUIRE_GPU").as_deref(), Ok("1"));
    std::env::var_os("RUSTEDA_D0_PTX_MANIFEST")
        .map(PathBuf::from)
        .expect("hash-bound PTX manifest required")
}

#[test]
#[ignore = "requires explicit real GPU and hash-bound PTX manifest"]
fn gpu_required_flags_match_cpu_reference_exactly() {
    let ptx = VerifiedPtx::load(&manifest()).expect("PTX/source/tool hashes must match");
    let session = Session::new(&ptx).expect("GPU-required mode must not pass without hardware");
    eprintln!("D0 real GPU: {}", session.description);
    let soa = |rows: &[[i64; 4]]| {
        (0..4)
            .flat_map(|axis| rows.iter().map(move |r| r[axis]))
            .collect::<Vec<_>>()
    };
    let sb = soa(&[
        [-10, -10, 0, 0],
        [i64::MIN, i64::MIN, i64::MIN + 8, i64::MIN + 8],
        [i64::MAX - 8, i64::MAX - 8, i64::MAX, i64::MAX],
    ]);
    let qb = soa(&[
        [i64::MIN, i64::MIN, i64::MAX, i64::MAX],
        [0, 0, 10, 10],
        [i64::MAX - 1, i64::MAX - 1, i64::MAX, i64::MAX],
    ]);
    let pairs: Vec<u32> = (0..3)
        .flat_map(|q| (0..3).flat_map(move |s| [q, s]))
        .collect();
    let extremes = Batch {
        shapes: Shapes {
            bounds: &sb,
            layers: &[0, 0, 0],
        },
        queries: Queries {
            bounds: &qb,
            layers: &[0, 0, 0],
            excluded_shape_index: &[u32::MAX; 3],
        },
        pairs: &pairs,
    };
    let expected = cpu_ref(extremes, Limits::default()).unwrap();
    assert_eq!(expected, [1, 1, 1, 0, 0, 0, 0, 0, 1]);
    assert_eq!(
        session.run(extremes, Limits::default()).unwrap().flags,
        expected
    );
    for count in [0, 1, 127, 128, 129, 257, 513, 65_537] {
        let data = fixture::Fixture::new(1024, 128, count);
        let expected = cpu_ref(data.batch(), Limits::default()).unwrap();
        for _ in 0..3 {
            let got = session.run(data.batch(), Limits::default()).unwrap();
            assert_eq!(
                got.flags, expected,
                "integer tolerance is zero, {count} pairs"
            );
        }
        if count >= 127 {
            assert!(expected.contains(&1) && expected.contains(&0));
        }
    }
}

#[test]
#[ignore = "requires an actual built PTX manifest; opens no CUDA context"]
fn stale_source_or_corrupted_ptx_is_rejected_before_device_setup() {
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest()).unwrap()).unwrap();
    let folder =
        std::env::temp_dir().join(format!("rusteda-d0-hash-negative-{}", std::process::id()));
    std::fs::create_dir(&folder).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(folder.clone());
    let mut changed = original.clone();
    changed["sources"][0]["sha256"] = serde_json::Value::String("0".repeat(64));
    let path = folder.join("bad-source.json");
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(VerifiedPtx::load(&path).is_err());
    changed = original.clone();
    changed["cpu_gate"]["sha256"] = serde_json::Value::String("0".repeat(64));
    let path = folder.join("bad-cpu-gate.json");
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(VerifiedPtx::load(&path).is_err());
    changed = original;
    let ptx = changed["ptx"]["path"].as_str().unwrap();
    let mut bytes = std::fs::read(Path::new(ptx)).unwrap();
    bytes[0] ^= 1;
    let corrupted = folder.join("corrupt.ptx");
    std::fs::write(&corrupted, bytes).unwrap();
    changed["ptx"]["path"] = serde_json::Value::String(corrupted.to_str().unwrap().into());
    let path = folder.join("bad-ptx.json");
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(VerifiedPtx::load(&path).is_err());
}

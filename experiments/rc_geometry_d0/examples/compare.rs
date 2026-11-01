//! Whole-call D0 timing, with all packing, validation, allocation and transfers.
#[cfg(feature = "cuda")]
#[path = "../tests/fixture/mod.rs"]
mod fixture;

#[cfg(feature = "cuda")]
fn main() {
    use rusteda_rc_geometry_d0::{
        Limits, cpu_ref,
        cuda::{Session, VerifiedPtx},
    };
    use std::time::Instant;
    let manifest =
        std::env::var_os("RUSTEDA_D0_PTX_MANIFEST").expect("hash-bound PTX manifest required");
    let cold = Instant::now();
    let ptx = VerifiedPtx::load(std::path::Path::new(&manifest)).unwrap();
    let manifest_seconds = cold.elapsed().as_secs_f64();
    let session = Session::new(&ptx).expect("GPU-required benchmark cannot substitute CPU");
    let cold_setup_seconds = cold.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({"schema":"rusteda.rc-geometry-d0-timing.v1", "device":session.description,
        "manifest_seconds":manifest_seconds,"context_jit_seconds":session.setup_seconds,
        "cold_setup_seconds":cold_setup_seconds,"scope":"integer pair filter only; no extraction or whole-flow speedup claim"})
    );
    for count in [1024, 65_536, 1_048_576, 8_000_000] {
        let packing = Instant::now();
        let data = fixture::Fixture::new(32_768, 2048, count);
        let packing_seconds = packing.elapsed().as_secs_f64();
        // Interleave repeated CPU and GPU observations; no isolated-kernel ratio.
        for repetition in 0..3 {
            let cpu = Instant::now();
            let expected = cpu_ref(data.batch(), Limits::default()).unwrap();
            let cpu_seconds = cpu.elapsed().as_secs_f64();
            let got = session.run(data.batch(), Limits::default()).unwrap();
            assert_eq!(got.flags, expected, "integer tolerance is zero");
            println!(
                "{}",
                serde_json::json!({"pairs":count,"repetition":repetition,
                "packing_seconds":packing_seconds,"cpu_call_seconds":cpu_seconds,
                "gpu_call_seconds":got.wall_seconds,"gpu_validation_seconds":got.validation_seconds,
                "h2d_allocation_seconds":got.upload_seconds,"kernel_sync_seconds":got.kernel_and_sync_seconds,
                "d2h_sync_seconds":got.download_seconds,"cpu_with_packing_seconds":packing_seconds+cpu_seconds,
                "gpu_warm_with_packing_seconds":packing_seconds+got.wall_seconds,
                "gpu_cold_with_packing_seconds":cold_setup_seconds+packing_seconds+got.wall_seconds,
                "estimated_host_bytes":got.bytes.host_bytes,"estimated_device_bytes":got.bytes.device_bytes,
                "device_buffer_payload_peak_bytes":got.device_buffer_peak_bytes,
                "global_free_device_before_bytes":got.free_device_before_bytes,
                "global_free_device_after_upload_bytes":got.free_device_after_upload_bytes,
                "driver_context_memory_status":"not_separately_measured",
                "exact_flags":true})
            );
        }
    }
}

#[cfg(not(feature = "cuda"))]
fn main() {
    panic!("build the D0 comparison with --features cuda; no CPU substitution");
}

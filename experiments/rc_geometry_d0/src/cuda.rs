// SPDX-License-Identifier: Apache-2.0
//! D0 hash-bound prebuilt PTX loader over the existing cudarc version.
#![allow(unsafe_code)]

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use cudarc::driver::{CudaContext, CudaFunction, CudaStream, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Batch, Error, Estimate, Limits, validate};

fn device(error: impl std::fmt::Display) -> Error {
    Error::Device(error.to_string())
}

fn file(record: &Value, cap: u64) -> Result<PathBuf, Error> {
    let item = record.as_object().ok_or(Error::Invalid("FileId object"))?;
    if item.len() != 3 {
        return Err(Error::Invalid("FileId fields"));
    }
    let path = PathBuf::from(
        item.get("path")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid("FileId path"))?,
    );
    let expected = item
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid("FileId digest"))?;
    let bytes = item
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or(Error::Invalid("FileId bytes"))?;
    if !path.is_absolute()
        || bytes > cap
        || expected.len() != 64
        || !expected
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::Invalid("FileId bounds"));
    }
    let mut stream = File::open(&path).map_err(device)?;
    let before = stream.metadata().map_err(device)?;
    if !before.is_file() || before.len() != bytes {
        return Err(Error::Invalid("FileId byte mismatch"));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = stream.read(&mut buffer).map_err(device)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let after = stream.metadata().map_err(device)?;
    if before.len() != after.len()
        || before.modified().map_err(device)? != after.modified().map_err(device)?
        || format!("{:x}", digest.finalize()) != expected
    {
        return Err(Error::Invalid("stale source/tool/PTX hash"));
    }
    Ok(path)
}

/// PTX whose complete source/tool manifest has been verified before context creation.
pub struct VerifiedPtx {
    text: String,
}

impl VerifiedPtx {
    pub fn load(manifest: &Path) -> Result<Self, Error> {
        let metadata = manifest.metadata().map_err(device)?;
        if !metadata.is_file() || metadata.len() > 64 << 10 {
            return Err(Error::Budget("PTX manifest bytes"));
        }
        let value: Value =
            serde_json::from_slice(&std::fs::read(manifest).map_err(device)?).map_err(device)?;
        if value["schema"] != "rusteda.rc-geometry-d0-ptx.v1"
            || value["status"] != "built_unqualified"
            || value["target"] != "sm_80"
            || value["cpu_gate_status"] != "passed"
        {
            return Err(Error::Invalid("PTX manifest scope or CPU gate"));
        }
        let gate = file(&value["cpu_gate"], 1 << 20)?;
        let gate: Value =
            serde_json::from_slice(&std::fs::read(gate).map_err(device)?).map_err(device)?;
        if gate["status"] != "passed" || gate["tests"] != 4 {
            return Err(Error::Invalid("pinned CPU gate is not passed"));
        }
        for (key, cap) in [("sources", 1 << 20), ("tools", 1 << 30)] {
            let records = value[key]
                .as_array()
                .ok_or(Error::Invalid("PTX input manifest"))?;
            if records.is_empty() || records.len() > 32 {
                return Err(Error::Budget("PTX input count"));
            }
            for record in records {
                file(record, cap)?;
            }
        }
        let path = file(&value["ptx"], 8 << 20)?;
        let text = std::fs::read_to_string(path).map_err(device)?;
        let expected = value["ptx"]["sha256"]
            .as_str()
            .ok_or(Error::Invalid("PTX digest"))?;
        if format!("{:x}", Sha256::digest(text.as_bytes())) != expected {
            return Err(Error::Invalid("PTX changed during read"));
        }
        Ok(Self { text })
    }
}

pub struct Session {
    context: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    function: CudaFunction,
    pub setup_seconds: f64,
    pub description: String,
}

pub struct Run {
    pub flags: Vec<u32>,
    pub bytes: Estimate,
    pub validation_seconds: f64,
    pub upload_seconds: f64,
    pub kernel_and_sync_seconds: f64,
    pub download_seconds: f64,
    pub wall_seconds: f64,
    /// Exact simultaneously-live device buffer payload, excluding driver/JIT state.
    pub device_buffer_peak_bytes: u64,
    /// Global free-memory observations with all buffers live. Other clients and
    /// pooling can affect their difference; None means no launch was needed.
    pub free_device_before_bytes: Option<u64>,
    pub free_device_after_upload_bytes: Option<u64>,
}

impl Session {
    /// Does not call NVRTC; missing driver/device is an explicit unavailable result.
    pub fn new(ptx: &VerifiedPtx) -> Result<Self, Error> {
        let started = Instant::now();
        let context = std::panic::catch_unwind(|| CudaContext::new(0))
            .map_err(|_| Error::Unavailable("CUDA driver loading panicked".into()))?
            .map_err(|e| Error::Unavailable(e.to_string()))?;
        let (major, minor) = context.compute_capability().map_err(device)?;
        if major < 8 {
            return Err(Error::Unavailable(
                "D0 requires compute capability 8.0+".into(),
            ));
        }
        let description = format!("{} sm_{major}{minor}", context.name().map_err(device)?);
        let stream = context.new_stream().map_err(device)?;
        let module = context
            .load_module(Ptx::from_src(ptx.text.clone()))
            .map_err(device)?;
        let function = module.load_function("rc_geometry_flags").map_err(device)?;
        Ok(Self {
            context,
            stream,
            function,
            setup_seconds: started.elapsed().as_secs_f64(),
            description,
        })
    }

    pub fn run(&self, batch: Batch<'_>, limits: Limits) -> Result<Run, Error> {
        let started = Instant::now();
        let bytes = validate(batch, limits)?;
        let validation_seconds = started.elapsed().as_secs_f64();
        let count = batch.pairs.len() / 2;
        if count == 0 {
            return Ok(Run {
                flags: Vec::new(),
                bytes,
                validation_seconds,
                upload_seconds: 0.0,
                kernel_and_sync_seconds: 0.0,
                download_seconds: 0.0,
                wall_seconds: started.elapsed().as_secs_f64(),
                device_buffer_peak_bytes: 0,
                free_device_before_bytes: None,
                free_device_after_upload_bytes: None,
            });
        }
        self.context.bind_to_thread().map_err(device)?;
        let (free, _) = cudarc::driver::result::mem_get_info().map_err(device)?;
        if bytes.device_bytes > free as u64 / 2 {
            return Err(Error::Budget("half-free device memory admission"));
        }
        let upload = Instant::now();
        let sb = self
            .stream
            .clone_htod(batch.shapes.bounds)
            .map_err(device)?;
        let sl = self
            .stream
            .clone_htod(batch.shapes.layers)
            .map_err(device)?;
        let qb = self
            .stream
            .clone_htod(batch.queries.bounds)
            .map_err(device)?;
        let ql = self
            .stream
            .clone_htod(batch.queries.layers)
            .map_err(device)?;
        let excluded = self
            .stream
            .clone_htod(batch.queries.excluded_shape_index)
            .map_err(device)?;
        let pairs = self.stream.clone_htod(batch.pairs).map_err(device)?;
        let mut sentinel = Vec::new();
        sentinel
            .try_reserve_exact(count)
            .map_err(|_| Error::Budget("sentinel output allocation"))?;
        sentinel.resize(count, u32::MAX);
        let mut flags = self.stream.clone_htod(&sentinel).map_err(device)?;
        self.stream.synchronize().map_err(device)?;
        let upload_seconds = upload.elapsed().as_secs_f64();
        let (free_after_upload, _) = cudarc::driver::result::mem_get_info().map_err(device)?;
        let (sb_len, sl_len, qb_len, ql_len, ex_len, pair_len) = (
            batch.shapes.bounds.len() as u64,
            batch.shapes.layers.len() as u64,
            batch.queries.bounds.len() as u64,
            batch.queries.layers.len() as u64,
            batch.queries.excluded_shape_index.len() as u64,
            batch.pairs.len() as u64,
        );
        let kernel = Instant::now();
        let mut launch = self.stream.launch_builder(&self.function);
        // Oxide Rust slices lower to (device pointer, u64 length), in this order.
        launch
            .arg(&sb)
            .arg(&sb_len)
            .arg(&sl)
            .arg(&sl_len)
            .arg(&qb)
            .arg(&qb_len)
            .arg(&ql)
            .arg(&ql_len)
            .arg(&excluded)
            .arg(&ex_len)
            .arg(&pairs)
            .arg(&pair_len)
            .arg(&mut flags);
        let config = LaunchConfig {
            grid_dim: ((count as u32).div_ceil(128), 1, 1),
            block_dim: (128, 1, 1),
            shared_mem_bytes: 0,
        };
        // SAFETY: validate() proves SoA lengths, all pair indices and the hard
        // count <=8M. Slice lengths match uploads; each thread writes only its
        // own allocated output word. This ABI is checked against generated PTX.
        unsafe { launch.launch(config) }.map_err(device)?;
        self.stream.synchronize().map_err(device)?;
        let kernel_and_sync_seconds = kernel.elapsed().as_secs_f64();
        let download = Instant::now();
        let flags = self.stream.clone_dtoh(&flags).map_err(device)?;
        self.stream.synchronize().map_err(device)?;
        let download_seconds = download.elapsed().as_secs_f64();
        if flags.iter().any(|&flag| flag > 1) {
            return Err(Error::Device(
                "kernel left sentinel or invalid flags".into(),
            ));
        }
        Ok(Run {
            flags,
            bytes,
            validation_seconds,
            upload_seconds,
            kernel_and_sync_seconds,
            download_seconds,
            wall_seconds: started.elapsed().as_secs_f64(),
            device_buffer_peak_bytes: bytes.device_bytes,
            free_device_before_bytes: Some(free as u64),
            free_device_after_upload_bytes: Some(free_after_upload as u64),
        })
    }
}

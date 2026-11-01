// SPDX-License-Identifier: Apache-2.0
//! Bounded ordered query transcript. This is a comparison aid, not an oracle.

use super::{Error, Result};
use sha2::{Digest, Sha256};

/// The trailing NUL belongs to the hash domain. It is not a string terminator
/// supplied by a caller or an allocation of per-query transcript records.
pub const OUTCOME_DOMAIN: &[u8] = b"RustEDA.G1.outcome.v1\0";
pub const OUTCOME_RECORD_BYTES: usize = 24;
pub const OUTCOME_RECORD_IO_WORK: u64 = 2;

#[derive(Clone, Debug)]
pub struct OutcomeDigest {
    pub sha256: [u8; 32],
    pub records: u64,
    pub query_work: u64,
    pub first_error: Option<(u64, u32)>,
}

#[derive(Clone, Debug)]
pub(super) struct RollingOutcome {
    hash: Sha256,
    records: u64,
    query_work: u64,
    first_error: Option<(u64, u32)>,
}

impl RollingOutcome {
    pub fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(OUTCOME_DOMAIN);
        Self {
            hash,
            records: 0,
            query_work: 0,
            first_error: None,
        }
    }

    /// Caller consumes the two encode/SHA IO units before this call. A record
    /// has one global ordinal even when launches were split into small prefixes.
    pub fn record(&mut self, ordinal: u64, work: u64, status: u32) -> Result<()> {
        if ordinal != self.records || status > 5 {
            return Err(Error::Invalid("ordered outcome transcript"));
        }
        let total = self
            .query_work
            .checked_add(work)
            .ok_or(Error::Budget("outcome work total"))?;
        let mut row = [0_u8; OUTCOME_RECORD_BYTES];
        row[..8].copy_from_slice(&ordinal.to_le_bytes());
        row[8..16].copy_from_slice(&work.to_le_bytes());
        row[16..20].copy_from_slice(&status.to_le_bytes());
        // 20..24 is fixed zero padding, never the device local-index field.
        self.hash.update(row);
        self.query_work = total;
        self.records += 1;
        if status != 0 && self.first_error.is_none() {
            self.first_error = Some((ordinal, status));
        }
        Ok(())
    }

    pub fn finish(self) -> OutcomeDigest {
        OutcomeDigest {
            sha256: self.hash.finalize().into(),
            records: self.records,
            query_work: self.query_work,
            first_error: self.first_error,
        }
    }

    pub fn snapshot(&self) -> OutcomeDigest {
        self.clone().finish()
    }
}

/// Two logical units per 24-byte tuple, plus the bounded domain SHA unit.
pub fn transcript_io_reservation(queries: u64) -> Result<u64> {
    if queries > super::session::MAX_STREAM_QUERIES {
        return Err(Error::Budget("outcome transcript query count"));
    }
    queries
        .checked_mul(OUTCOME_RECORD_IO_WORK)
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Budget("outcome transcript reservation"))
}

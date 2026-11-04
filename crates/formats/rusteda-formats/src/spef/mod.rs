// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Bounded IEEE 1481 detailed RC reader. Grounded inter-net coupling is explicit;
//! unsupported semantic constructs fail instead of silently changing timing.

mod parse;
mod source;

use serde::{Deserialize, Serialize};

pub use parse::{Counts, count, read};
pub use source::{DecodedSource, SourceEncoding, decode_source, decode_source_owned};

/// Hard parser budgets, retained with the imported source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_nets: usize,
    pub max_nodes: usize,
    pub max_elements: usize,
    pub max_name_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 32 << 20,
            max_nets: 100_000,
            max_nodes: 500_000,
            max_elements: 1_000_000,
            max_name_bytes: 16 << 20,
        }
    }
}

impl Limits {
    /// Absolute supported parser ceilings, also used for public typed inputs.
    pub fn absolute_max() -> Self {
        let d = Self::default();
        Self {
            max_bytes: d.max_bytes * 8,
            max_nets: d.max_nets * 8,
            max_nodes: d.max_nodes * 8,
            max_elements: d.max_elements * 8,
            max_name_bytes: d.max_name_bytes * 8,
        }
    }
    /// Reject zero or excessive budgets even for an on-disk table.
    pub fn validate(&self) -> Result<(), Error> {
        let d = Self::default();
        for (value, default) in [
            (self.max_bytes, d.max_bytes),
            (self.max_nets, d.max_nets),
            (self.max_nodes, d.max_nodes),
            (self.max_elements, d.max_elements),
            (self.max_name_bytes, d.max_name_bytes),
        ] {
            if value == 0 || value > default * 8 {
                return Err(Error::new(
                    0,
                    "T0072",
                    "invalid SPEF budget (allowed 1..8x default)",
                ));
            }
        }
        Ok(())
    }
}

/// A structured parse/budget error with a one-based source line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code} at SPEF line {line}: {message}")]
pub struct Error {
    pub line: usize,
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub(crate) fn new(line: usize, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            line,
            code,
            message: message.into(),
        }
    }
}

/// A local RC node (per-net indices never escape their net), as [`Nodes`] holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node<'a> {
    pub name: &'a str,
    pub cap_pf: f64,
}

/// The RC nodes of one net by local index: their names in one buffer and their capacitances
/// (pF), so that a net holds three allocations however many nodes it has instead of one name
/// per node: a parsed extraction of a million nodes is a few large blocks, which its consumer
/// can release at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Nodes {
    names: String,
    /// The end of the name of node `i` in `names`; it starts at the end of node `i - 1`.
    ends: Vec<u32>,
    caps: Vec<f64>,
}

impl Nodes {
    /// No nodes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// No nodes, with room for `nodes` nodes whose names total `name_bytes` bytes.
    #[must_use]
    pub fn with_capacity(nodes: usize, name_bytes: usize) -> Self {
        Self {
            names: String::with_capacity(name_bytes),
            ends: Vec::with_capacity(nodes),
            caps: Vec::with_capacity(nodes),
        }
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.caps.len()
    }

    /// True without nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// Appends a node named `name` with capacitance `cap_pf`; its index is the previous
    /// [`Self::len`].
    ///
    /// # Panics
    /// When the names of the net would exceed `u32::MAX` bytes (the reader caps them far
    /// below).
    pub fn push(&mut self, name: &str, cap_pf: f64) {
        self.names.push_str(name);
        let end = u32::try_from(self.names.len()).expect("RC node names of one net exceed 4 GiB");
        self.ends.push(end);
        self.caps.push(cap_pf);
    }

    /// The name of node `i`.
    ///
    /// # Panics
    /// When `i` is not below [`Self::len`], as indexing a slice does.
    #[must_use]
    pub fn name(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.names[start..self.ends[i] as usize]
    }

    /// The capacitance (pF) of node `i`.
    ///
    /// # Panics
    /// When `i` is not below [`Self::len`].
    #[must_use]
    pub fn cap_pf(&self, i: usize) -> f64 {
        self.caps[i]
    }

    /// The capacitances (pF) of the nodes, by index.
    #[must_use]
    pub fn caps(&self) -> &[f64] {
        &self.caps
    }

    /// The capacitances (pF) of the nodes, by index, to change them.
    pub fn caps_mut(&mut self) -> &mut [f64] {
        &mut self.caps
    }

    /// Node `i`, if there is one.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<Node<'_>> {
        (i < self.len()).then(|| Node {
            name: self.name(i),
            cap_pf: self.caps[i],
        })
    }

    /// The nodes in index order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Node<'_>> + '_ {
        (0..self.len()).map(|i| Node {
            name: self.name(i),
            cap_pf: self.caps[i],
        })
    }

    /// The total bytes of the node names.
    #[must_use]
    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }
}

/// A physical connection, with the source's declared direction.
#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    pub node: u32,
    pub port: bool,
    pub direction: char,
}

/// Resistance in kOhm between local node indices.
#[derive(Debug, Clone, PartialEq)]
pub struct Resistor {
    pub a: u32,
    pub b: u32,
    pub kohm: f64,
}

/// One detailed net, with inter-net coupling grounded at factor one.
#[derive(Debug, Clone, PartialEq)]
pub struct Net {
    pub name: String,
    pub total_cap_pf: f64,
    pub nodes: Nodes,
    pub connections: Vec<Connection>,
    pub resistors: Vec<Resistor>,
}

/// Immutable parsed parasitics. The whole object can be shared between corners.
#[derive(Debug, Clone, PartialEq)]
pub struct Spef {
    pub design: String,
    pub divider: char,
    pub delimiter: char,
    pub bus_delimiter: (char, char),
    pub nets: Vec<Net>,
    pub coupling_records: usize,
    pub metadata_records: usize,
    pub node_count: usize,
    pub element_count: usize,
}

impl Spef {
    /// Validate public in-memory input before indexing or allocating engine state.
    /// Parser counters are not trusted merely because the struct is public.
    pub fn validate(&self, limits: Limits) -> Result<(), Error> {
        limits.validate()?;
        let fail = |message| Error::new(0, "T0071", message);
        if self.design.is_empty() || self.design.len() > 4096 {
            return Err(fail("invalid RC design name"));
        }
        if self.nets.is_empty() || self.nets.len() > limits.max_nets {
            return Err(Error::new(0, "T0072", "invalid RC net count"));
        }
        let mut count = 0usize;
        let mut resistors = 0usize;
        let mut bytes = self.design.len();
        let mut names = std::collections::BTreeSet::new();
        for net in &self.nets {
            count = count
                .checked_add(net.nodes.len())
                .ok_or_else(|| fail("RC count overflow"))?;
            resistors = resistors
                .checked_add(net.resistors.len())
                .ok_or_else(|| fail("RC count overflow"))?;
            if count > limits.max_nodes || resistors > limits.max_elements {
                return Err(Error::new(0, "T0072", "in-memory RC budget exceeded"));
            }
            if net.name.is_empty()
                || net.name.len() > 4096
                || !names.insert(net.name.as_str())
                || net.nodes.is_empty()
                || !net.total_cap_pf.is_finite()
                || net.total_cap_pf < 0.0
            {
                return Err(fail("invalid or duplicate RC net"));
            }
            bytes = bytes.saturating_add(net.name.len());
            let mut node_names = std::collections::BTreeSet::new();
            let mut sum = 0.0;
            for node in net.nodes.iter() {
                if node.name.is_empty()
                    || node.name.len() > 4096
                    || !node_names.insert(node.name)
                    || !node.cap_pf.is_finite()
                    || node.cap_pf < 0.0
                {
                    return Err(fail("invalid or duplicate RC node"));
                }
                bytes = bytes.saturating_add(node.name.len());
                sum += node.cap_pf;
            }
            if !sum.is_finite()
                || (sum - net.total_cap_pf).abs() > 1e-8_f64.max(net.total_cap_pf * 1e-4)
            {
                return Err(fail("RC total capacitance mismatch"));
            }
            let mut connections = std::collections::BTreeSet::new();
            if net.connections.iter().any(|c| {
                c.node as usize >= net.nodes.len()
                    || !matches!(c.direction, 'I' | 'O' | 'B')
                    || !connections.insert(c.node)
            }) {
                return Err(fail("invalid or duplicate RC connection"));
            }
            if net.resistors.iter().any(|r| {
                r.a as usize >= net.nodes.len()
                    || r.b as usize >= net.nodes.len()
                    || !r.kohm.is_finite()
                    || r.kohm < 0.0
            }) {
                return Err(fail("invalid RC resistor"));
            }
            if bytes > limits.max_name_bytes {
                return Err(Error::new(0, "T0072", "RC name budget exceeded"));
            }
        }
        if count != self.node_count
            || self.element_count < resistors
            || self.element_count > limits.max_elements
        {
            return Err(fail("inconsistent RC allocation counters"));
        }
        Ok(())
    }

    /// Conservative additional peak state, including parser and RC tree temporaries.
    pub fn estimated_state_bytes(&self) -> u64 {
        let nodes = self
            .nets
            .iter()
            .fold(0u64, |n, net| n.saturating_add(net.nodes.len() as u64));
        let resistors = self
            .nets
            .iter()
            .fold(0u64, |n, net| n.saturating_add(net.resistors.len() as u64));
        nodes
            .saturating_mul(448)
            .saturating_add(
                (self.element_count as u64)
                    .max(resistors)
                    .saturating_mul(64),
            )
            .saturating_add((self.nets.len() as u64).saturating_mul(256))
    }
}

/// Metadata of the PARASIT v1 source container (section 2; source is section 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub corner: String,
    pub limits: Limits,
}

/// One corner of a PARASIT v2 container (RCLI-REQ-0075): its limits and the identity of the
/// file it was imported from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CornerSource {
    pub corner: String,
    pub limits: Limits,
    /// The source path as given at import.
    pub source_path: String,
    /// The hash of the source file, as the workspace writes source hashes.
    pub source_hash: String,
}

/// Metadata of the PARASIT v2 container (section 1, RCLI-REQ-0075): the corners in ascending
/// name order; the decoded source of entry `i` is section `i + 2`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CornerSetMetadata {
    pub corners: Vec<CornerSource>,
}

#[cfg(test)]
mod tests;

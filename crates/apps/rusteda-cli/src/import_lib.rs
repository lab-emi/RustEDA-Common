// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Liberty library import (`rusteda import --lib`; RCLI-REQ-0012, RCLI-REQ-0013), with an
//! optional compile cache shared by several workspaces (`--lib-cache`, RCLI-REQ-0025).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusteda_core::diag as core_diag;
use rusteda_formats::liberty::{self, Library};
use rusteda_workspace::hash::hash_file;
use rusteda_workspace::{
    CachedLibrary, CompiledDigest, DiagCode, Diagnostic, Error, Hash64, JsonMap, Severity,
    SharedLibrary, SharedLibraryCache, Txn, Workspace, rlib,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The schema of the record that `rusteda import` keeps with a library in a shared cache.
const RECORD_SCHEMA: &str = "rusteda.lib-cache-record.v2";

/// What `rusteda import` needs of a library besides its compiled form, kept with it in a
/// shared cache so that a later import of the same bytes from the same source by the same
/// executable does not parse it again (RCLI-REQ-0025): the library name, its cell names in
/// order, the reader's diagnostics (none of them an error), which name the source, and the
/// length and xxh3-64 of the compiled `.rlib` published with it, which an adopted `.rlib` must
/// match byte for byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheRecord {
    schema: String,
    /// The xxh3-64 of the executable that compiled the library, in 16 hex digits.
    executable: String,
    name: String,
    /// The xxh3-64 of the library bytes, in 16 hex digits.
    hash: String,
    bytes: u64,
    source: String,
    /// The xxh3-64 of the compiled `.rlib`, in 16 hex digits.
    rlib: String,
    rlib_bytes: u64,
    cells: Vec<String>,
    diagnostics: Vec<Diagnostic>,
}

/// The xxh3-64 of the running executable, or `None` when it cannot be read; without it the
/// shared cache is not used.
fn executable_hash() -> Option<Hash64> {
    static HASH: OnceLock<Option<Hash64>> = OnceLock::new();
    *HASH.get_or_init(|| hash_file(&std::env::current_exe().ok()?).ok())
}

/// The shared cache of `--lib-cache`, with the hash of this executable.
pub(crate) struct LibCache {
    shared: SharedLibraryCache,
    executable: Hash64,
}

impl LibCache {
    /// The shared cache in `dir`, created when missing (`IO_ERROR` when it cannot be); `None`
    /// when this executable cannot be read, so that its libraries cannot be told apart from
    /// another build's, and every library is then compiled as without the option.
    pub(crate) fn open(dir: &Path) -> Result<Option<LibCache>, Error> {
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        if !fs::metadata(dir).map_err(|e| Error::io(dir, e))?.is_dir() {
            return Err(Error::io(
                dir,
                std::io::Error::other("--lib-cache must name a directory"),
            ));
        }
        Ok(executable_hash().map(|executable| LibCache {
            shared: SharedLibraryCache::new(dir),
            executable,
        }))
    }

    /// The library of `bytes` (hash `hash`) imported from `source` that this executable
    /// published, with its record and the digest of the `.rlib` it published.
    fn lookup(
        &self,
        bytes: &[u8],
        hash: Hash64,
        source: &str,
    ) -> Option<(SharedLibrary, CacheRecord, CompiledDigest)> {
        let found = self.shared.find(hash).ok()?;
        found.into_iter().find_map(|shared| {
            let record: CacheRecord = serde_json::from_slice(&shared.record).ok()?;
            let compiled = CompiledDigest {
                hash: Hash64::parse_hex(&record.rlib)?,
                bytes: record.rlib_bytes,
            };
            let accepted = record.schema == RECORD_SCHEMA
                && record.executable == self.executable.hex()
                && record.name == shared.name
                && record.hash == hash.hex()
                && record.bytes == bytes.len() as u64
                && record.source == source
                && record
                    .diagnostics
                    .iter()
                    .all(|d| d.severity != Severity::Error);
            accepted.then_some((shared, record, compiled))
        })
    }

    /// Publishes the library just stored in the workspace as `cached`, imported as
    /// `imported`. Publishing only saves later imports work, so a failure is ignored: the
    /// import has its library either way.
    fn publish(&self, cached: &CachedLibrary, imported: &Imported) {
        let _ = self.shared.publish(cached, |compiled| {
            let record = CacheRecord {
                schema: RECORD_SCHEMA.to_string(),
                executable: self.executable.hex(),
                name: imported.name.clone(),
                hash: imported.hash.hex(),
                bytes: imported.bytes.len() as u64,
                source: imported.source_str.clone(),
                rlib: compiled.hash.hex(),
                rlib_bytes: compiled.bytes,
                cells: imported.cells.clone(),
                diagnostics: imported.diagnostics.clone(),
            };
            // Serializing these fields cannot fail; an empty record would never be accepted.
            serde_json::to_vec(&record).unwrap_or_default()
        });
    }
}

/// Converts a `rusteda_core::diag::Diag` from the Liberty reader into an envelope [`Diagnostic`].
fn convert_diag(d: core_diag::Diag, fallback_file: &str) -> Diagnostic {
    let sev = match d.severity {
        core_diag::Severity::Note => Severity::Note,
        core_diag::Severity::Warning => Severity::Warning,
        core_diag::Severity::Error => Severity::Error,
    };
    let code = DiagCode::parse(&d.code).unwrap_or(DiagCode::of("L0001"));
    let loc = d.location.map(|l| {
        let file = if l.file.is_empty() {
            fallback_file.to_string()
        } else {
            l.file
        };
        (file, l.line.max(1), l.column.max(1))
    });
    Diagnostic::from_parts(sev, code, loc, d.message, d.help)
}

/// A library of this invocation: read and checked, from the reader or from a shared cache.
struct Imported {
    source_str: String,
    corner: String,
    bytes: Vec<u8>,
    hash: Hash64,
    name: String,
    cells: Vec<String>,
    /// The reader's diagnostics, as replayed into the envelope.
    diagnostics: Vec<Diagnostic>,
    origin: Origin,
}

/// Where the compiled form of an [`Imported`] library comes from.
enum Origin {
    /// This invocation parsed the library.
    Parsed(Box<Library>),
    /// A shared cache holds it (RCLI-REQ-0025), compiled as the digest says.
    Shared(SharedLibrary, CompiledDigest),
}

/// Reads `text` (from `source_str`) with the Liberty reader. On success, the library and the
/// reader's diagnostics as envelope diagnostics; on a reader error, the diagnostics to report
/// and the error, the first error diagnostic.
fn parse_library(
    text: &str,
    source_str: &str,
) -> Result<(Library, Vec<Diagnostic>), (Vec<Diagnostic>, Error)> {
    let (lib, reader_diags) = match liberty::read_text_diags(text, source_str) {
        Ok(pair) => pair,
        Err(liberty::Error::Syntax {
            file,
            line,
            col,
            msg,
        }) => {
            let d = Diagnostic::new(Severity::Error, DiagCode::of("L0001"), msg.clone()).at(
                file.clone(),
                line.max(1),
                col.max(1),
            );
            let e = Error::input_rejected(format!("{file}:{line}:{col}: {msg}"));
            return Err((vec![d], e));
        }
        Err(liberty::Error::Model { file, msg }) => {
            let d = Diagnostic::new(Severity::Error, DiagCode::of("L0002"), msg.clone()).at(
                file.clone(),
                1,
                1,
            );
            return Err((vec![d], Error::input_rejected(format!("{file}:1:1: {msg}"))));
        }
        Err(liberty::Error::Io { path: p, msg }) => {
            let d = Diagnostic::new(Severity::Error, DiagCode::of("L0001"), msg.clone()).at(
                p.clone(),
                1,
                1,
            );
            return Err((vec![d], Error::input_rejected(format!("{p}:1:1: {msg}"))));
        }
    };

    let mut first_err: Option<String> = None;
    let mut converted_all = Vec::with_capacity(reader_diags.len());
    for rd in reader_diags {
        let converted = convert_diag(rd, source_str);
        if converted.severity == Severity::Error && first_err.is_none() {
            first_err = Some(match &converted.location {
                Some(loc) => format!(
                    "{}:{}:{}: {}",
                    loc.file, loc.line, loc.col, converted.message
                ),
                None => format!("{source_str}:1:1: {}", converted.message),
            });
        }
        converted_all.push(converted);
    }
    match first_err {
        Some(err_msg) => Err((converted_all, Error::input_rejected(err_msg))),
        None => Ok((lib, converted_all)),
    }
}

/// Validates and caches `--lib` inputs and adds them to `txn` (RCLI-REQ-0012, RCLI-REQ-0013).
/// With `lib_cache`, a library that the shared cache holds for the same bytes, source and
/// executable is copied into the workspace cache instead of parsed and compiled, and a library
/// parsed here is published to it (RCLI-REQ-0025); the revision, the workspace cache files,
/// the diagnostics and the metrics are the same either way.
pub(crate) fn import_libs(
    ws: &Workspace,
    txn: &mut Txn,
    libs: &[(PathBuf, String)],
    lib_cache: Option<&LibCache>,
    metrics: &mut JsonMap,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Error> {
    if libs.is_empty() {
        return Ok(());
    }

    // Track (library_name, corner) pairs given in this invocation so duplicates are rejected.
    let mut seen_in_invocation: BTreeSet<(String, String)> = BTreeSet::new();
    // Track the libraries per corner to check cell uniqueness within each corner.
    let mut parsed_new: Vec<Imported> = Vec::with_capacity(libs.len());

    for (path, corner) in libs {
        let source_str = path.display().to_string();
        let meta = fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
        if !meta.file_type().is_file() {
            let msg = format!("{source_str}: not a regular file");
            diags.push(
                Diagnostic::new(Severity::Error, DiagCode::of("L0001"), msg.clone())
                    .at(source_str, 1, 1),
            );
            return Err(Error::input_rejected(msg));
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            let msg = format!("{source_str}:1:1: invalid UTF-8 in Liberty file: {e}");
            diags.push(
                Diagnostic::new(Severity::Error, DiagCode::of("L0001"), msg.clone()).at(
                    source_str.clone(),
                    1,
                    1,
                ),
            );
            Error::input_rejected(msg)
        })?;
        let hash = Hash64::of(&bytes);

        let imported = match lib_cache.and_then(|c| c.lookup(&bytes, hash, &source_str)) {
            Some((shared, record, compiled)) => {
                diags.extend(record.diagnostics.iter().cloned());
                Imported {
                    source_str,
                    corner: corner.clone(),
                    hash,
                    name: record.name,
                    cells: record.cells,
                    diagnostics: record.diagnostics,
                    origin: Origin::Shared(shared, compiled),
                    bytes,
                }
            }
            None => {
                let (lib, converted) = match parse_library(text, &source_str) {
                    Ok(parsed) => parsed,
                    Err((reported, err)) => {
                        diags.extend(reported);
                        return Err(err);
                    }
                };
                diags.extend(converted.iter().cloned());
                Imported {
                    source_str,
                    corner: corner.clone(),
                    hash,
                    name: lib.name.clone(),
                    cells: lib.cells.iter().map(|c| c.name.clone()).collect(),
                    diagnostics: converted,
                    origin: Origin::Parsed(Box::new(lib)),
                    bytes,
                }
            }
        };
        let source_str = &imported.source_str;

        if !seen_in_invocation.insert((imported.name.clone(), corner.clone())) {
            let msg = format!(
                "{source_str}:1:1: library '{}' is given twice for corner '{corner}'",
                imported.name
            );
            diags.push(
                Diagnostic::new(
                    Severity::Error,
                    DiagCode::of("L0003"),
                    format!(
                        "library '{}' is given twice for corner '{corner}'",
                        imported.name
                    ),
                )
                .at(source_str.clone(), 1, 1),
            );
            return Err(Error::input_rejected(msg));
        }

        // Check duplicate cell definitions within this library itself (RCLI-REQ-0013).
        let mut local_cells = BTreeSet::new();
        for cell in &imported.cells {
            if !local_cells.insert(cell.as_str()) {
                let msg = format!(
                    "{source_str}:1:1: cell '{cell}' is defined twice in corner '{corner}' (library '{}')",
                    imported.name
                );
                diags.push(
                    Diagnostic::new(
                        Severity::Error,
                        DiagCode::of("L0055"),
                        format!(
                            "cell '{cell}' is defined twice in corner '{corner}' (library '{}')",
                            imported.name
                        ),
                    )
                    .at(source_str.clone(), 1, 1),
                );
                return Err(Error::input_rejected(msg));
            }
        }

        parsed_new.push(imported);
    }

    // Check cell uniqueness across all libraries in each affected corner (including carried base
    // libraries that are not replaced by `(name, corner)`).
    let mut corner_cells: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for existing in txn.libraries() {
        if seen_in_invocation.contains(&(existing.name.clone(), existing.corner.clone())) {
            continue;
        }
        if let Some(cached) = ws.cache().lookup(&existing.name, existing.hash) {
            if let Ok(text) = fs::read_to_string(&cached.lib_path) {
                if let Ok(lib) = liberty::read_text(&text, &cached.lib) {
                    let map = corner_cells.entry(existing.corner.clone()).or_default();
                    for cell in lib.cells {
                        map.insert(cell.name, existing.name.clone());
                    }
                }
            }
        }
    }

    for imported in &parsed_new {
        let (source_str, corner) = (&imported.source_str, &imported.corner);
        let map = corner_cells.entry(corner.clone()).or_default();
        for cell in &imported.cells {
            if let Some(prev_lib) = map.insert(cell.clone(), imported.name.clone()) {
                let msg = format!(
                    "{source_str}:1:1: cell '{cell}' in library '{}' is already defined in library '{prev_lib}' for corner '{corner}'",
                    imported.name
                );
                diags.push(
                    Diagnostic::new(
                        Severity::Error,
                        DiagCode::of("L0055"),
                        format!(
                            "cell '{cell}' in library '{}' is already defined in library '{prev_lib}' for corner '{corner}'",
                            imported.name
                        ),
                    )
                    .at(source_str.clone(), 1, 1),
                );
                return Err(Error::input_rejected(msg));
            }
        }
    }

    let mut total_cells = 0usize;
    for imported in parsed_new {
        total_cells += imported.cells.len();
        let cached = store(ws, lib_cache, &imported)?;
        txn.add_library(cached.entry(imported.corner, imported.source_str))?;
    }

    metrics.insert("libraries".to_string(), json!(libs.len()));
    metrics.insert("library_cells".to_string(), json!(total_cells));
    Ok(())
}

/// Stores `imported` in the workspace cache: copies it from the shared cache when it comes from
/// there and the shared `.rlib` is exactly the recorded one, otherwise compiles it (parsing it
/// again when it came from the shared cache, whose record must then agree with the reader), and
/// publishes a library it compiled to the shared cache (RCLI-REQ-0025).
fn store(
    ws: &Workspace,
    lib_cache: Option<&LibCache>,
    imported: &Imported,
) -> Result<CachedLibrary, Error> {
    let reparsed;
    let lib = match &imported.origin {
        Origin::Parsed(lib) => lib,
        Origin::Shared(shared, compiled) => {
            if let Some(cached) = ws.cache().put_shared(
                shared,
                &imported.bytes,
                *compiled,
                rlib::KIND,
                rlib::VERSIONS,
            )? {
                return Ok(cached);
            }
            // The shared .rlib is not the recorded one (replaced, damaged or of another
            // publisher since the record was written).
            let text = std::str::from_utf8(&imported.bytes)
                .map_err(|e| Error::internal(format!("cached library text: {e}")))?;
            let agrees = match parse_library(text, &imported.source_str) {
                Ok((lib, converted)) => {
                    let same = lib.name == imported.name
                        && converted == imported.diagnostics
                        && lib.cells.len() == imported.cells.len()
                        && lib
                            .cells
                            .iter()
                            .zip(&imported.cells)
                            .all(|(c, n)| &c.name == n);
                    same.then_some(lib)
                }
                Err(_) => None,
            };
            reparsed = agrees.ok_or_else(|| {
                Error::internal(format!(
                    "the shared library cache record of {} disagrees with the library",
                    imported.source_str
                ))
            })?;
            &reparsed
        }
    };
    let cached = rlib::put(ws, lib, &imported.bytes)?;
    if let Some(cache) = lib_cache {
        debug_assert_eq!(cached.hash, imported.hash);
        cache.publish(&cached, imported);
    }
    Ok(cached)
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The library cache `cache/` of a workspace (WS-REQ-0080 to WS-REQ-0084).
//!
//! Every imported library is kept verbatim as `cache/<name>-<h16>.lib` and compiled as
//! `cache/<name>-<h16>.rlib`, where `h16` is the xxh3-64 of the library bytes in 16 hex digits.
//! Both files are written atomically (temporary file, fsync, rename), so concurrent imports of
//! the same library are harmless, and a library whose two files exist is not compiled again
//! while its `.rlib` is current: a valid rbin file (structure and content hash) of the kind and
//! a format version that the build reads. The caller names that kind and those versions, so a
//! compiled form left by an older release after a format version bump, a damaged one or one of
//! another kind is compiled again from the bytes (contract §5: "format_version bump =>
//! recompile from cache/*.lib"); one of a newer release is `SCHEMA_TOO_NEW` and stays.
//! [`LibraryCache::recompile`] rebuilds the `.rlib` from the verbatim copy, and
//! [`LibraryCache::open_rlib_or_recompile`] and
//! [`Revision::open_library_or_recompile`](crate::Revision::open_library_or_recompile) open a
//! compiled library, compiling it again first when it is not current.
//!
//! A cache belongs to a workspace: it is reached through
//! [`Workspace::cache`](crate::Workspace::cache) and
//! [`Revision::open_library`](crate::Revision::open_library), never for an arbitrary directory,
//! because its compiled libraries are mapped (WS-REQ-0037). The cache reads regular files only
//! and never follows a symbolic link (WS-REQ-0025).
//!
//! A [`SharedLibraryCache`] is a directory that several workspaces share, so that a library
//! compiled for one of them is not compiled again for the next (WS-REQ-0085 to WS-REQ-0087).
//! It holds a copy of the compiled `.rlib` of each library, named as in a workspace cache, and
//! a record that the publishing tool writes, which binds the copy by its hash and length
//! ([`CompiledDigest`]). A workspace adopts a library by writing its own copies of the two files
//! ([`LibraryCache::put_shared`]): the `.lib` from the bytes the caller read, the `.rlib` from
//! the shared bytes once they are exactly the recorded ones. No file of a workspace cache is
//! ever a link of a shared file or of another workspace's file, so a workspace keeps the
//! guarantee of WS-REQ-0037 whatever happens to the shared directory.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::error::{Corruption, Error, Result};
use crate::fsutil;
use crate::hash::Hash64;
use crate::rbin::Rbin;
use crate::schema::LibraryEntry;
use crate::workspace::layout;

/// A failure of [`LibraryCache::put_library`], [`LibraryCache::recompile`] or an open that
/// compiles again.
#[derive(Debug, thiserror::Error)]
pub enum PutError<E> {
    /// The compiler rejected the library.
    #[error("compiling the library failed: {0}")]
    Compile(E),
    /// The cache could not be read or written; a cached file is damaged (`CORRUPT_TABLE`) or
    /// of a newer release (`SCHEMA_TOO_NEW`); or the compiler returned something that is not a
    /// valid rbin file of the kind and versions asked for (`INTERNAL`).
    #[error(transparent)]
    Workspace(#[from] Error),
}

/// A library in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedLibrary {
    /// The library name.
    pub name: String,
    /// The hash of the library bytes.
    pub hash: Hash64,
    /// The verbatim copy, relative to the workspace: `cache/<name>-<h16>.lib`.
    pub lib: String,
    /// The compiled library, relative to the workspace: `cache/<name>-<h16>.rlib`.
    pub rlib: String,
    /// The verbatim copy.
    pub lib_path: PathBuf,
    /// The compiled library.
    pub rlib_path: PathBuf,
    /// Whether this call ran the compiler.
    pub compiled_now: bool,
}

impl CachedLibrary {
    /// The `meta.json` entry of this library in `corner`, imported from `source`.
    pub fn entry(&self, corner: impl Into<String>, source: impl Into<String>) -> LibraryEntry {
        LibraryEntry {
            name: self.name.clone(),
            corner: corner.into(),
            hash: self.hash,
            source: source.into(),
            compiled: self.rlib.clone(),
        }
    }
}

/// The library cache of a workspace, from [`Workspace::cache`](crate::Workspace::cache).
#[derive(Debug, Clone)]
pub struct LibraryCache {
    root: PathBuf,
}

impl LibraryCache {
    /// The cache of the workspace at `workspace_root`. Crate-private (WS-REQ-0037): a cache
    /// exists only for an open workspace (`Workspace::cache`), since its compiled libraries
    /// are mapped, and a map is sound only for files that the workspace never writes again.
    pub(crate) fn new(workspace_root: impl Into<PathBuf>) -> LibraryCache {
        LibraryCache {
            root: workspace_root.into(),
        }
    }

    /// The cache directory.
    pub fn dir(&self) -> PathBuf {
        self.root.join(layout::CACHE)
    }

    fn located(&self, name: &str, hash: Hash64) -> CachedLibrary {
        let stem = format!("{name}-{}", hash.hex());
        let lib = format!("{}/{stem}.lib", layout::CACHE);
        let rlib = rlib_path(name, hash);
        CachedLibrary {
            name: name.to_string(),
            hash,
            lib_path: self.root.join(&lib),
            rlib_path: self.root.join(&rlib),
            lib,
            rlib,
            compiled_now: false,
        }
    }

    /// Stores library `name` with its bytes and compiled form; `kind` and `versions` are the
    /// rbin kind and the format versions of compiled libraries that this build reads, such as
    /// `"LIBERTY", 1..=1`. The compiler runs only when the `.rlib` is not current: missing, not
    /// a valid rbin file (structure and content hash), of another kind, or of a format version
    /// below `versions`, as after a format version bump (WS-REQ-0080, WS-REQ-0083). Its output
    /// must be a valid rbin file of `kind` and `versions` (otherwise `INTERNAL`). Nothing is
    /// written when both files exist and the `.rlib` is current. An `.rlib` of a format
    /// version above `versions`, written by a newer release, is `SCHEMA_TOO_NEW` and stays
    /// (WS-REQ-0039). Library names are 1 to 128 characters of `[A-Za-z0-9._+-]`, not starting
    /// with `.`.
    pub fn put_library<E>(
        &self,
        name: &str,
        lib_bytes: &[u8],
        kind: &str,
        versions: RangeInclusive<u32>,
        compile: impl FnOnce(&[u8]) -> std::result::Result<Vec<u8>, E>,
    ) -> std::result::Result<CachedLibrary, PutError<E>> {
        check_library_name(name)?;
        let mut library = self.located(name, Hash64::of(lib_bytes));
        let has_lib = is_regular_file(&library.lib_path);
        let current = is_current_rlib(&library.rlib_path, kind, &versions)?;
        if has_lib && current {
            return Ok(library);
        }
        fsutil::create_dir_all(&self.dir())?;
        if !has_lib {
            fsutil::write_atomic(&library.lib_path, lib_bytes)?;
        }
        if !current {
            let compiled = compile(lib_bytes).map_err(PutError::Compile)?;
            let compiled = check_compiled(name, compiled, kind, &versions)?;
            fsutil::write_atomic(&library.rlib_path, compiled.as_bytes())?;
            library.compiled_now = true;
        }
        Ok(library)
    }

    /// Stores the library `shared` of a [`SharedLibraryCache`] in this cache instead of
    /// compiling it (WS-REQ-0086), as [`LibraryCache::put_library`] would store `lib_bytes`:
    /// nothing is written when both files exist here and the `.rlib` is current. Otherwise the
    /// shared `.rlib` is read and must be exactly the file that `compiled` describes (its length
    /// and xxh3-64, as the publishing tool recorded them, [`SharedLibraryCache::publish`]) and a
    /// valid rbin file of `kind` and `versions` (structure and content hash); only then are the
    /// missing or stale files written into this cache, each atomically as by `put_library`: the
    /// `.lib` from `lib_bytes` and the `.rlib` from the verified bytes. These are copies of this
    /// workspace's own, never links, so no later change of the shared directory reaches them.
    /// `None` when `lib_bytes` are not the library's, or the shared `.rlib` is missing, of
    /// another type, cannot be read or fails a check; nothing is then written, and the caller
    /// compiles the library itself. A library adopted this way is byte for byte the library
    /// that the publishing tool compiled, so a tool must accept only records of libraries it
    /// compiled itself (see [`SharedLibraryCache`]).
    pub fn put_shared(
        &self,
        shared: &SharedLibrary,
        lib_bytes: &[u8],
        compiled: CompiledDigest,
        kind: &str,
        versions: RangeInclusive<u32>,
    ) -> Result<Option<CachedLibrary>> {
        check_library_name(&shared.name)?;
        if Hash64::of(lib_bytes) != shared.hash {
            return Ok(None);
        }
        let library = self.located(&shared.name, shared.hash);
        let has_lib = is_regular_file(&library.lib_path);
        let current = is_current_rlib(&library.rlib_path, kind, &versions)?;
        if has_lib && current {
            return Ok(Some(library));
        }
        // The shared bytes are read once and checked; the very bytes checked are written.
        let rbin = if current {
            None
        } else {
            match read_shared_rlib(&shared.rlib_path, compiled, kind, &versions) {
                Some(rbin) => Some(rbin),
                None => return Ok(None),
            }
        };
        fsutil::create_dir_all(&self.dir())?;
        if !has_lib {
            fsutil::write_atomic(&library.lib_path, lib_bytes)?;
        }
        if let Some(rbin) = rbin {
            fsutil::write_atomic(&library.rlib_path, rbin.as_bytes())?;
        }
        Ok(Some(library))
    }

    /// Compiles the cached verbatim library again and replaces its `.rlib` atomically, e.g.
    /// after a format version change (WS-REQ-0083). The verbatim copy must still hash to
    /// `hash` (otherwise `CORRUPT_TABLE`), and the compiled bytes must be a valid rbin file of
    /// `kind` and `versions` (otherwise `INTERNAL`); on failure the old `.rlib` stays.
    pub fn recompile<E>(
        &self,
        name: &str,
        hash: Hash64,
        kind: &str,
        versions: RangeInclusive<u32>,
        compile: impl FnOnce(&[u8]) -> std::result::Result<Vec<u8>, E>,
    ) -> std::result::Result<CachedLibrary, PutError<E>> {
        check_library_name(name)?;
        let mut library = self.located(name, hash);
        // WS-REQ-0025: a regular file only; a FIFO in its place would block the read.
        let bytes = fsutil::read_regular(&library.lib_path)?;
        let computed = Hash64::of(&bytes);
        if computed != hash {
            let what = Corruption::TableHash {
                table: name.to_string(),
                stored: hash,
                computed,
            };
            return Err(Error::corrupt(&library.lib_path, what).into());
        }
        let compiled = compile(&bytes).map_err(PutError::Compile)?;
        let compiled = check_compiled(name, compiled, kind, &versions)?;
        fsutil::write_atomic(&library.rlib_path, compiled.as_bytes())?;
        library.compiled_now = true;
        Ok(library)
    }

    /// The cached library `name` with `hash`, if both of its files exist as regular files
    /// (WS-REQ-0081; a symbolic link is not followed). Whether its `.rlib` is current for a
    /// build is checked when it is stored ([`LibraryCache::put_library`]) and opened.
    pub fn lookup(&self, name: &str, hash: Hash64) -> Option<CachedLibrary> {
        check_library_name(name).ok()?;
        let library = self.located(name, hash);
        (is_regular_file(&library.lib_path) && is_regular_file(&library.rlib_path))
            .then_some(library)
    }

    /// Every cached library with `hash`, whatever its name, sorted by name (WS-REQ-0081).
    pub fn find(&self, hash: Hash64) -> Result<Vec<CachedLibrary>> {
        let dir = self.dir();
        let suffix = format!("-{}.lib", hash.hex());
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(dir, e)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let Ok(file) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(name) = file.strip_suffix(&suffix) {
                if let Some(library) = self.lookup(name, hash) {
                    found.push(library);
                }
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    /// Maps the compiled library `cache/<name>-<h16>.rlib` of this workspace read-only and
    /// validates its header (`kind`, `versions`; WS-REQ-0032, WS-REQ-0082). The workspace
    /// replaces the file only by a rename, never writes it in place, so it can be mapped
    /// (WS-REQ-0037). It must be a regular file, checked without following a symbolic link
    /// (WS-REQ-0025); otherwise `CORRUPT_TABLE`, as is a format version below `versions`
    /// (see [`LibraryCache::open_rlib_or_recompile`]); a missing file is `IO_ERROR`.
    pub fn open_rlib(
        &self,
        name: &str,
        hash: Hash64,
        kind: &str,
        versions: RangeInclusive<u32>,
    ) -> Result<Rbin> {
        check_library_name(name)?;
        Rbin::map_immutable(&self.located(name, hash).rlib_path, kind, versions)
    }

    /// [`LibraryCache::open_rlib`], compiling the library again from its verbatim copy first
    /// when its `.rlib` cannot be opened as `kind` and `versions` but the copy can replace it
    /// (WS-REQ-0084): the `.rlib` is missing, damaged, of another kind, or of a format version
    /// below `versions` (contract §5: "format_version bump => recompile from cache/*.lib"). The
    /// copy must hash to `hash` ([`LibraryCache::recompile`]). An `.rlib` of a newer release is
    /// `SCHEMA_TOO_NEW` and stays; the compiler runs at most once.
    pub fn open_rlib_or_recompile<E>(
        &self,
        name: &str,
        hash: Hash64,
        kind: &str,
        versions: RangeInclusive<u32>,
        compile: impl FnOnce(&[u8]) -> std::result::Result<Vec<u8>, E>,
    ) -> std::result::Result<Rbin, PutError<E>> {
        match self.open_rlib(name, hash, kind, versions.clone()) {
            Err(e) if is_stale(&e) => {
                self.recompile(name, hash, kind, versions.clone(), compile)?;
                Ok(self.open_rlib(name, hash, kind, versions)?)
            }
            opened => Ok(opened?),
        }
    }
}

/// The shared `.rlib` at `path` when it is exactly the file `compiled` describes and a valid
/// rbin file of `kind` and `versions`; `None` otherwise. The length is checked before the file
/// is read, so a large file in its place is not read.
fn read_shared_rlib(
    path: &Path,
    compiled: CompiledDigest,
    kind: &str,
    versions: &RangeInclusive<u32>,
) -> Option<Rbin> {
    let file = fsutil::open_regular(path).ok()?;
    if file.metadata().ok()?.len() != compiled.bytes {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(compiled.bytes).ok()?);
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, compiled.bytes + 1),
        &mut bytes,
    )
    .ok()?;
    if bytes.len() as u64 != compiled.bytes || Hash64::of(&bytes) != compiled.hash {
        return None;
    }
    let rbin = Rbin::from_bytes(bytes, kind, versions.clone()).ok()?;
    rbin.verify_hash().ok()?;
    Some(rbin)
}

/// The largest record of a [`SharedLibraryCache`] that is read.
pub const MAX_SHARED_RECORD_BYTES: u64 = 16 << 20;

/// A directory of compiled libraries shared by the caches of several workspaces (WS-REQ-0085),
/// such as all the workspaces of one flow run (`rusteda import --lib-cache DIR`).
///
/// Library `name` with the xxh3-64 `h16` of its bytes is kept as `<name>-<h16>.rlib`, a copy of
/// its compiled form, and `<name>-<h16>.json`, a record of the tool that published it. The
/// directory never compiles anything: a tool publishes a library it compiled into a workspace
/// ([`SharedLibraryCache::publish`]), which puts the length and xxh3-64 of the `.rlib` it copies
/// ([`CompiledDigest`]) into the record, and a later import finds it by the hash of its bytes
/// ([`SharedLibraryCache::find`]) and adopts it into another workspace
/// ([`LibraryCache::put_shared`]), which writes the workspace's own copies once the shared
/// `.rlib` is exactly the recorded one. What else the record holds is the tool's business; it
/// must let the tool accept only libraries that it compiled itself, from the same bytes (for
/// `rusteda import`, the hash of its own executable, the bytes and their source,
/// RCLI-REQ-0025). Every file is written under a temporary name and renamed, so concurrent
/// imports are harmless: an `.rlib` replaced by another publisher, or changed in any other way,
/// no longer matches the record it is found with and is not adopted. The binding guards
/// against mixed, stale and damaged entries; like any cache, the directory must be writable
/// only by the tools that publish to it.
#[derive(Debug, Clone)]
pub struct SharedLibraryCache {
    dir: PathBuf,
}

/// The length and xxh3-64 of the compiled `.rlib` of a library published to a
/// [`SharedLibraryCache`], which the publishing tool records ([`SharedLibraryCache::publish`])
/// and gives back when it adopts the library ([`LibraryCache::put_shared`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompiledDigest {
    /// The xxh3-64 of the `.rlib` file.
    pub hash: Hash64,
    /// Its length in bytes.
    pub bytes: u64,
}

/// A library of a [`SharedLibraryCache`], found by the hash of its bytes (WS-REQ-0085).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedLibrary {
    /// The library name.
    pub name: String,
    /// The hash of the library bytes.
    pub hash: Hash64,
    /// The record stored with the library.
    pub record: Vec<u8>,
    /// The compiled library, `<dir>/<name>-<h16>.rlib`.
    pub rlib_path: PathBuf,
}

impl SharedLibraryCache {
    /// The shared cache in `dir`, which is created when a library is published.
    pub fn new(dir: impl Into<PathBuf>) -> SharedLibraryCache {
        SharedLibraryCache { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn paths(&self, name: &str, hash: Hash64) -> (PathBuf, PathBuf) {
        let stem = format!("{name}-{}", hash.hex());
        (
            self.dir.join(format!("{stem}.rlib")),
            self.dir.join(format!("{stem}.json")),
        )
    }

    /// Every library stored with `hash`, whatever its name, sorted by name, with its record:
    /// those whose `.rlib` and record exist as regular files (a symbolic link is not followed,
    /// WS-REQ-0025) and whose record is at most [`MAX_SHARED_RECORD_BYTES`]. Nothing else is
    /// checked here; [`LibraryCache::put_shared`] verifies the `.rlib` it adopts. A missing
    /// directory has no libraries.
    pub fn find(&self, hash: Hash64) -> Result<Vec<SharedLibrary>> {
        let suffix = format!("-{}.json", hash.hex());
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(&self.dir, e)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&self.dir, e))?;
            let Ok(file) = entry.file_name().into_string() else {
                continue;
            };
            let Some(name) = file.strip_suffix(&suffix) else {
                continue;
            };
            if !is_library_name(name) {
                continue;
            }
            let (rlib_path, record_path) = self.paths(name, hash);
            let record_size = fs::symlink_metadata(&record_path)
                .ok()
                .filter(|m| m.file_type().is_file())
                .map(|m| m.len());
            if !is_regular_file(&rlib_path)
                || !record_size.is_some_and(|n| n <= MAX_SHARED_RECORD_BYTES)
            {
                continue;
            }
            let Ok(record) = fsutil::read_regular(&record_path) else {
                continue;
            };
            found.push(SharedLibrary {
                name: name.to_string(),
                hash,
                record,
                rlib_path,
            });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    /// Publishes the library `cached` of a workspace cache (WS-REQ-0087): its `.rlib` is read
    /// once and copied into the directory, and `record`, given the [`CompiledDigest`] of the
    /// bytes copied, makes the record that is written after it. Each file is written under a
    /// temporary name, fsynced and renamed (the directory is created when missing), so an
    /// entry of the same name and hash is replaced, and the workspace's file stays a file of
    /// its own. The caller has just stored or verified `cached` in its workspace.
    pub fn publish(
        &self,
        cached: &CachedLibrary,
        record: impl FnOnce(CompiledDigest) -> Vec<u8>,
    ) -> Result<()> {
        check_library_name(&cached.name)?;
        let compiled = fsutil::read_regular(&cached.rlib_path)?;
        let digest = CompiledDigest {
            hash: Hash64::of(&compiled),
            bytes: compiled.len() as u64,
        };
        fsutil::create_dir_all(&self.dir)?;
        let (rlib_path, record_path) = self.paths(&cached.name, cached.hash);
        fsutil::write_atomic(&rlib_path, &compiled)?;
        fsutil::write_atomic(&record_path, &record(digest))
    }
}

/// The path of the compiled library `name` with `hash`, relative to the workspace:
/// `cache/<name>-<h16>.rlib` (contract §2), as a library entry of `meta.json` records it.
pub(crate) fn rlib_path(name: &str, hash: Hash64) -> String {
    format!("{}/{name}-{}.rlib", layout::CACHE, hash.hex())
}

/// Whether `path` itself, not following a symbolic link, is a regular file.
fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Whether the cached `.rlib` at `path` is current for a build that reads `kind` and
/// `versions` (WS-REQ-0080, WS-REQ-0083): a regular file and a valid rbin file of that kind and
/// a format version in `versions`, with a matching content hash. `false` when it is missing or
/// fails a check, so it is compiled again; one of a newer format version is `SCHEMA_TOO_NEW`.
fn is_current_rlib(path: &Path, kind: &str, versions: &RangeInclusive<u32>) -> Result<bool> {
    let opened = Rbin::read(path, kind, versions.clone()).and_then(|rbin| rbin.verify_hash());
    match opened {
        Ok(()) => Ok(true),
        Err(e) if is_stale(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether an error of opening a cached `.rlib` means that compiling the library again from
/// its verbatim copy replaces it: the file is missing or fails validation, a format version
/// below the build's included. A newer format version (`SCHEMA_TOO_NEW`) or any other I/O
/// error does not.
fn is_stale(error: &Error) -> bool {
    match error {
        Error::Corrupt { .. } => true,
        Error::Io { source, .. } => source.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// The compiled form of a library is a valid rbin file (structure and content hash) of `kind`
/// and a format version in `versions`; anything else is a bug of the compiler, `INTERNAL`
/// (WS-REQ-0080). Returns it, validated.
fn check_compiled(
    name: &str,
    compiled: Vec<u8>,
    kind: &str,
    versions: &RangeInclusive<u32>,
) -> Result<Rbin> {
    Rbin::from_bytes(compiled, kind, versions.clone())
        .and_then(|rbin| rbin.verify_hash().map(|()| rbin))
        .map_err(|e| {
            Error::internal(format!(
                "the compiled form of library {name} is not a valid rbin file of kind {kind} and \
                 format version {}..={}: {e}",
                versions.start(),
                versions.end()
            ))
        })
}

/// Whether `name` is a library name of the cache: 1 to 128 characters of `[A-Za-z0-9._+-]`,
/// not starting with `.`.
pub(crate) fn is_library_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn check_library_name(name: &str) -> Result<()> {
    if !is_library_name(name) {
        return Err(Error::input_rejected(format!(
            "library name '{name}' must be 1 to 128 characters of [A-Za-z0-9._+-], not starting \
             with '.'"
        )));
    }
    Ok(())
}

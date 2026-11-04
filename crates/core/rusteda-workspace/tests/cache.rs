// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The library cache (WS-REQ-0080 to WS-REQ-0082), the shared library cache (WS-REQ-0085 to
//! WS-REQ-0087) and the library entries of a revision (WS-REQ-0073).

mod common;

use std::cell::Cell;
use std::fs;

use common::{entries, inv, new_workspace, temp_dir};
use rusteda_workspace::{
    CompiledDigest, Error, ErrorCode, Hash64, JsonMap, LibraryEntry, PutError, Rbin, RbinWriter,
    Rev, SharedLibraryCache,
};

/// A stand-in compiler: an rbin of kind LIBERTY whose section 1 holds the library bytes
/// reversed (the codec itself is deferred).
fn reversed(bytes: &[u8]) -> Vec<u8> {
    compiled(&bytes.iter().rev().copied().collect::<Vec<u8>>())
}

/// An rbin of kind LIBERTY whose section 1 holds `content`.
fn compiled(content: &[u8]) -> Vec<u8> {
    let mut w = RbinWriter::new("LIBERTY", 1).unwrap();
    w.section_bytes(1, 1, content.to_vec()).unwrap();
    w.to_bytes()
}

/// The content of section 1 of a compiled library.
fn content(rlib: &[u8]) -> Vec<u8> {
    let r = Rbin::from_bytes(rlib.to_vec(), "LIBERTY", 1..=1).unwrap();
    r.section::<u8>(1).unwrap().to_vec()
}

#[test]
fn put_library_stores_once() {
    let ws = new_workspace("cache");
    let cache = ws.cache();
    let calls = Cell::new(0);
    let compile = |bytes: &[u8]| -> Result<Vec<u8>, String> {
        calls.set(calls.get() + 1);
        Ok(reversed(bytes))
    };
    let lib_bytes = b"library (cells_a) { }";
    let h16 = Hash64::of(lib_bytes).hex();

    let a = cache
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, compile)
        .unwrap();
    assert!(a.compiled_now);
    assert_eq!(calls.get(), 1);
    assert_eq!(a.hash, Hash64::of(lib_bytes));
    assert_eq!(a.lib, format!("cache/cells_a-{h16}.lib"));
    assert_eq!(a.rlib, format!("cache/cells_a-{h16}.rlib"));
    assert_eq!(a.lib_path, ws.root().join(&a.lib));
    assert_eq!(fs::read(&a.lib_path).unwrap(), lib_bytes);
    assert_eq!(fs::read(&a.rlib_path).unwrap(), reversed(lib_bytes));
    assert_eq!(
        content(&fs::read(&a.rlib_path).unwrap()),
        lib_bytes.iter().rev().copied().collect::<Vec<u8>>()
    );

    // Both files exist: nothing is compiled or written.
    let again = cache
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, compile)
        .unwrap();
    assert!(!again.compiled_now);
    assert_eq!(calls.get(), 1);
    assert_eq!(
        entries(&cache.dir()),
        [format!("cells_a-{h16}.lib"), format!("cells_a-{h16}.rlib")]
    );

    // Other bytes, other files.
    let b = cache
        .put_library(
            "cells_a",
            b"library (cells_a) { cell (x) { } }",
            "LIBERTY",
            1..=1,
            compile,
        )
        .unwrap();
    assert_ne!(b.hash, a.hash);
    assert_eq!(calls.get(), 2);
    assert_eq!(entries(&cache.dir()).len(), 4);

    // A missing .rlib is compiled again from the bytes.
    fs::remove_file(&a.rlib_path).unwrap();
    assert!(cache.lookup("cells_a", a.hash).is_none());
    let rebuilt = cache
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, compile)
        .unwrap();
    assert!(rebuilt.compiled_now);
    assert_eq!(calls.get(), 3);

    // Lookup by name and hash, or by hash alone.
    assert_eq!(cache.lookup("cells_a", a.hash).unwrap(), again);
    assert!(cache.lookup("cells_b", a.hash).is_none());
    assert!(cache.lookup("../cells_a", a.hash).is_none());
    cache
        .put_library("alias", lib_bytes, "LIBERTY", 1..=1, compile)
        .unwrap();
    let names: Vec<String> = cache
        .find(a.hash)
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(names, ["alias", "cells_a"]);
    assert!(cache.find(Hash64(1)).unwrap().is_empty());

    let entry = a.entry("default", "libs/cells_a.lib");
    assert_eq!(
        (entry.name.as_str(), entry.corner.as_str()),
        ("cells_a", "default")
    );
    assert_eq!(
        (entry.hash, entry.compiled.as_str()),
        (a.hash, a.rlib.as_str())
    );
    assert_eq!(entry.source, "libs/cells_a.lib");
}

#[test]
fn compile_errors_and_recompile() {
    let ws = new_workspace("cache-errors");
    let cache = ws.cache();
    let failed = cache.put_library("x", b"bad", "LIBERTY", 1..=1, |_| {
        Err::<Vec<u8>, _>("syntax error at 1:1")
    });
    match failed {
        Err(PutError::Compile(message)) => assert_eq!(message, "syntax error at 1:1"),
        other => panic!("expected a compile error: {other:?}"),
    }
    let names = entries(&cache.dir());
    assert_eq!(
        names.len(),
        1,
        "the verbatim copy stays, no .rlib: {names:?}"
    );
    assert!(names[0].ends_with(".lib"));

    let ok = cache
        .put_library("x", b"bad", "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        })
        .unwrap();
    assert!(ok.compiled_now);
    let again = cache
        .recompile("x", ok.hash, "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(&[b.len() as u8]))
        })
        .unwrap();
    assert!(again.compiled_now);
    assert_eq!(content(&fs::read(&ok.rlib_path).unwrap()), [3]);

    fs::write(&ok.lib_path, b"changed").unwrap();
    match cache.recompile("x", ok.hash, "LIBERTY", 1..=1, |b| {
        Ok::<_, String>(compiled(b))
    }) {
        Err(PutError::Workspace(e)) => assert_eq!(e.code(), ErrorCode::CorruptTable),
        other => panic!("expected a hash mismatch: {other:?}"),
    }
    for bad in ["", ".x", "a/b", "a b", "../x", "x\0"] {
        match cache.put_library(bad, b"lib", "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        }) {
            Err(PutError::Workspace(e)) => {
                assert_eq!(e.code(), ErrorCode::InputRejected, "{bad:?}")
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}

#[test]
fn library_in_a_revision() {
    let ws = new_workspace("cache-revision");
    let cached = ws
        .cache()
        .put_library("cells_a", b"lib", "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        })
        .unwrap();
    let mut txn = ws
        .begin(
            ws.resolve_base("rev0").unwrap(),
            &inv("rusteda import --lib a.lib"),
        )
        .unwrap();
    txn.add_library(cached.entry("default", "a.lib")).unwrap();
    let upstream = txn.upstream(&["libraries"]).unwrap();
    txn.add_bytes("liberty", "liberty.json", b"{}", upstream)
        .unwrap();
    let c = txn.commit(JsonMap::new()).unwrap();
    assert_eq!(c.meta.libraries[0].compiled, cached.rlib);
    let revision = ws.revision(c.rev).unwrap();
    let library = &revision.meta().libraries[0];
    assert_eq!(
        ws.cache()
            .lookup(&library.name, library.hash)
            .unwrap()
            .rlib_path,
        cached.rlib_path
    );
    assert_eq!(ws.manifest().unwrap().corners, ["default"]);
}

// WS-REQ-0080 (review CR2-3): the compiled form of a library is a valid rbin file (structure
// and content hash), checked before it is written; a compiler that returns anything else is a
// bug (INTERNAL), and nothing is stored. A cached .rlib that fails the check, left by an older
// release or damaged, is compiled again by the next put_library instead of being kept.
#[test]
fn compiled_libraries_are_checked() {
    let ws = new_workspace("cache-checked");
    let cache = ws.cache();
    let lib = b"library(cells) {}";
    let good = compiled(b"model");
    let mut flipped = good.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    for (what, bytes) in [
        ("not an rbin", b"not an rbin".to_vec()),
        ("content hash", flipped),
        ("truncated", good[..good.len() - 8].to_vec()),
    ] {
        let err = cache
            .put_library("cells", lib, "LIBERTY", 1..=1, |_| {
                Ok::<_, String>(bytes.clone())
            })
            .unwrap_err();
        match err {
            PutError::Workspace(e) => assert_eq!(e.code(), ErrorCode::Internal, "{what}: {e}"),
            other => panic!("{what}: {other:?}"),
        }
        let names = entries(&cache.dir());
        assert!(
            names.iter().all(|n| !n.ends_with(".rlib")),
            "{what}: {names:?}"
        );
    }
    let stored = cache
        .put_library("cells", lib, "LIBERTY", 1..=1, |_| {
            Ok::<_, String>(good.clone())
        })
        .unwrap();
    assert!(stored.compiled_now);

    // recompile refuses a bad result too, and keeps the old .rlib.
    let err = cache
        .recompile("cells", stored.hash, "LIBERTY", 1..=1, |_| {
            Ok::<_, String>(b"junk".to_vec())
        })
        .unwrap_err();
    assert!(matches!(err, PutError::Workspace(ref e) if e.code() == ErrorCode::Internal));
    assert_eq!(fs::read(&stored.rlib_path).unwrap(), good);

    // A damaged .rlib in the cache is compiled again, not kept forever.
    let mut damaged = good.clone();
    damaged[last] ^= 0x01;
    fs::remove_file(&stored.rlib_path).unwrap();
    fs::write(&stored.rlib_path, &damaged).unwrap();
    let healed = cache
        .put_library("cells", lib, "LIBERTY", 1..=1, |_| {
            Ok::<_, String>(good.clone())
        })
        .unwrap();
    assert!(healed.compiled_now, "the damaged .rlib is compiled again");
    assert_eq!(fs::read(&stored.rlib_path).unwrap(), good);
    let again = cache
        .put_library(
            "cells",
            lib,
            "LIBERTY",
            1..=1,
            |_| -> Result<Vec<u8>, String> { panic!("a valid .rlib is not compiled again") },
        )
        .unwrap();
    assert!(!again.compiled_now);
}

// WS-REQ-0025, WS-REQ-0082 (review CR2-5): the cache reads regular files only and never
// follows a symbolic link: a .rlib replaced by a link is not found, not mapped, and is
// replaced by a regular file on the next put_library. open_rlib maps a valid one.
#[cfg(unix)]
#[test]
fn cached_libraries_are_regular_files() {
    let ws = new_workspace("cache-regular");
    let cache = ws.cache();
    let lib = b"library(cells) {}";
    let stored = cache
        .put_library("cells", lib, "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        })
        .unwrap();
    let rlib = cache
        .open_rlib("cells", stored.hash, "LIBERTY", 1..=1)
        .unwrap();
    rlib.verify_hash().unwrap();
    assert_eq!(rlib.section::<u8>(1).unwrap(), lib);
    let err = cache
        .open_rlib("cells", stored.hash, "STA", 1..=1)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CorruptTable);
    let err = cache
        .open_rlib("../cells", stored.hash, "LIBERTY", 1..=1)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InputRejected);

    let outside = ws.root().parent().unwrap().join("outside.rlib");
    fs::write(&outside, compiled(b"elsewhere")).unwrap();
    fs::remove_file(&stored.rlib_path).unwrap();
    std::os::unix::fs::symlink(&outside, &stored.rlib_path).unwrap();
    assert!(cache.lookup("cells", stored.hash).is_none());
    assert!(cache.find(stored.hash).unwrap().is_empty());
    let err = cache
        .open_rlib("cells", stored.hash, "LIBERTY", 1..=1)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CorruptTable, "{err}");
    let healed = cache
        .put_library("cells", lib, "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        })
        .unwrap();
    assert!(healed.compiled_now);
    let meta = fs::symlink_metadata(&stored.rlib_path).unwrap();
    assert!(meta.file_type().is_file(), "the link is replaced");
    assert_eq!(
        fs::read(&outside).unwrap(),
        compiled(b"elsewhere"),
        "not written through"
    );
}

/// A library entry from its parts.
fn entry(name: &str, hash: u64, compiled: &str) -> LibraryEntry {
    LibraryEntry {
        name: name.into(),
        corner: "default".into(),
        hash: Hash64(hash),
        source: format!("{name}.lib"),
        compiled: compiled.into(),
    }
}

// WS-REQ-0073, WS-REQ-0082, WS-REQ-0083, WS-REQ-0084 (review CR2-5): a library entry names its
// compiled form at its cache path, cache/<name>-<h16>.rlib, on write and on read, and a
// revision's library is mapped through Revision::open_library, which never leaves the cache.
#[test]
fn library_entries_point_into_the_cache() {
    let ws = new_workspace("cache-entries");
    let cached = ws
        .cache()
        .put_library("cells", b"library(cells) {}", "LIBERTY", 1..=1, |b| {
            Ok::<_, String>(compiled(b))
        })
        .unwrap();
    let mut txn = ws
        .begin(
            ws.resolve_base("rev0").unwrap(),
            &inv("rusteda import --lib cells.lib"),
        )
        .unwrap();
    let canonical = format!("cache/cells-{}.rlib", Hash64(5).hex());
    for (library, code) in [
        (entry("my lib", 5, &canonical), ErrorCode::InputRejected),
        (entry(".hidden", 5, &canonical), ErrorCode::InputRejected),
        (entry("cells", 5, "../outside.rlib"), ErrorCode::Internal),
        (entry("cells", 5, "/etc/hostname"), ErrorCode::Internal),
        (entry("cells", 6, &canonical), ErrorCode::Internal),
        (
            entry("cells", 5, "cache/other-0000000000000005.rlib"),
            ErrorCode::Internal,
        ),
    ] {
        let err = txn.add_library(library.clone()).unwrap_err();
        assert_eq!(err.code(), code, "{library:?}: {err}");
        assert!(txn.set_libraries(vec![library]).is_err());
    }
    txn.add_library(cached.entry("default", "cells.lib"))
        .unwrap();
    let c = txn.commit(JsonMap::new()).unwrap();
    let rev = ws.revision(c.rev).unwrap();
    let library = &rev.meta().libraries[0];
    let rlib = rev.open_library(library, "LIBERTY", 1..=1).unwrap();
    assert_eq!(rlib.path(), cached.rlib_path);
    rlib.verify_hash().unwrap();
    // A library that is not one of the revision's is a bug of the caller.
    let foreign = entry("cells", 7, &format!("cache/cells-{}.rlib", Hash64(7).hex()));
    let err = rev.open_library(&foreign, "LIBERTY", 1..=1).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Internal);

    // On read, an entry that leaves the cache makes the revision CORRUPT_TABLE.
    let meta = ws.revision_dir(Rev(1)).join("meta.json");
    let text = fs::read_to_string(&meta).unwrap();
    for (from, to) in [
        (cached.rlib.as_str(), "../outside.rlib"),
        ("\"name\": \"cells\"", "\"name\": \"my lib\""),
    ] {
        assert!(text.contains(from), "{from}");
        fs::write(&meta, text.replace(from, to)).unwrap();
        let err = ws.revision(Rev(1)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::CorruptTable, "{to}: {err}");
        assert!(matches!(err, Error::Corrupt { .. }));
    }
    let twice = text.replacen(
        "\"libraries\": [",
        &format!(
            "\"libraries\": [{},",
            serde_json::to_string(&rev.meta().libraries[0]).unwrap()
        ),
        1,
    );
    fs::write(&meta, twice).unwrap();
    let err = ws.revision(Rev(1)).unwrap_err();
    assert!(err.to_string().contains("appears twice"), "{err}");
    fs::write(&meta, &text).unwrap();
    ws.revision(Rev(1)).unwrap();
}

#[cfg(unix)]
fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
}

/// The digest of a compiled file, as a publisher records it.
fn digest_of(bytes: &[u8]) -> CompiledDigest {
    CompiledDigest {
        hash: Hash64::of(bytes),
        bytes: bytes.len() as u64,
    }
}

/// Replaces `path` by a file holding `bytes`, by a rename, as every writer of the cache does.
fn replace_by_rename(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("replacement");
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, path).unwrap();
}

// WS-REQ-0085 to WS-REQ-0087: a library compiled for one workspace is published to the shared
// directory (a copy of its .rlib and a record holding the copy's digest) and adopted by another
// workspace as files of its own, byte for byte; nothing is written when the workspace has the
// library already, and no later write to the shared directory reaches an adopted library.
#[cfg(unix)]
#[test]
fn shared_cache_publishes_and_adopts_copies() {
    let a = new_workspace("shared-a");
    let b = new_workspace("shared-b");
    let shared = SharedLibraryCache::new(temp_dir("shared-dir").join("libs"));
    let lib_bytes = b"library (cells_a) { }";
    let hash = Hash64::of(lib_bytes);
    let h16 = hash.hex();
    assert!(
        shared.find(hash).unwrap().is_empty(),
        "a missing directory has nothing"
    );

    let put = a
        .cache()
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, |b: &[u8]| {
            Ok::<_, String>(reversed(b))
        })
        .unwrap();
    let digest = digest_of(&fs::read(&put.rlib_path).unwrap());
    let mut seen = None;
    shared
        .publish(&put, |d| {
            seen = Some(d);
            b"record one".to_vec()
        })
        .unwrap();
    assert_eq!(
        seen,
        Some(digest),
        "the record is made from the copied bytes"
    );
    assert_eq!(
        entries(shared.dir()),
        [format!("cells_a-{h16}.json"), format!("cells_a-{h16}.rlib")]
    );
    let shared_rlib = shared.dir().join(format!("cells_a-{h16}.rlib"));
    assert_eq!(
        fs::read(&shared_rlib).unwrap(),
        fs::read(&put.rlib_path).unwrap()
    );
    assert_ne!(
        inode(&shared_rlib),
        inode(&put.rlib_path),
        "copied, not linked"
    );

    let found = shared.find(hash).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].name.as_str(), found[0].record.as_slice()),
        ("cells_a", &b"record one"[..])
    );
    assert_eq!(found[0].rlib_path, shared_rlib);
    assert!(shared.find(Hash64::of(b"other")).unwrap().is_empty());

    let adopted = b
        .cache()
        .put_shared(&found[0], lib_bytes, digest, "LIBERTY", 1..=1)
        .unwrap()
        .expect("a verified shared library is adopted");
    assert!(!adopted.compiled_now);
    assert_eq!(
        (adopted.lib.clone(), adopted.rlib.clone()),
        (put.lib.clone(), put.rlib.clone())
    );
    assert_eq!(fs::read(&adopted.lib_path).unwrap(), lib_bytes);
    assert_eq!(
        fs::read(&adopted.rlib_path).unwrap(),
        fs::read(&put.rlib_path).unwrap()
    );
    for other in [&shared_rlib, &put.rlib_path] {
        assert_ne!(inode(&adopted.rlib_path), inode(other), "a file of its own");
    }
    assert_eq!(
        entries(&b.cache().dir()),
        [format!("cells_a-{h16}.lib"), format!("cells_a-{h16}.rlib")],
        "no temporary file is left"
    );
    // The adopted library is a current library of the workspace cache.
    assert!(
        b.cache()
            .open_rlib("cells_a", hash, "LIBERTY", 1..=1)
            .is_ok()
    );
    let again = b
        .cache()
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, |_: &[u8]| {
            Err::<Vec<u8>, _>("no compile expected")
        })
        .unwrap();
    assert!(!again.compiled_now);

    // A write in place into the shared directory reaches neither workspace.
    let before = fs::read(&adopted.rlib_path).unwrap();
    fs::write(&shared_rlib, compiled(b"something else")).unwrap();
    assert_eq!(fs::read(&adopted.rlib_path).unwrap(), before);
    assert_eq!(fs::read(&put.rlib_path).unwrap(), before);

    // Publishing again replaces the .rlib and the record.
    shared.publish(&put, |_| b"record two".to_vec()).unwrap();
    assert_eq!(shared.find(hash).unwrap()[0].record, b"record two");
    assert_eq!(fs::read(&shared_rlib).unwrap(), before);
}

// WS-REQ-0086: a shared library is adopted only when its .rlib is exactly the recorded one and
// valid, and nothing is written otherwise: other bytes, another valid .rlib under the name (the
// compile of other bytes, or of another publisher), another length, a damaged or foreign .rlib
// recorded as such, a symbolic link.
#[cfg(unix)]
#[test]
fn shared_cache_rejects_files_that_do_not_verify() {
    let a = new_workspace("shared-bad-a");
    let shared = SharedLibraryCache::new(temp_dir("shared-bad-dir").join("libs"));
    let lib_bytes = b"library (cells_a) { }";
    let hash = Hash64::of(lib_bytes);
    let h16 = hash.hex();
    let put = a
        .cache()
        .put_library("cells_a", lib_bytes, "LIBERTY", 1..=1, |b: &[u8]| {
            Ok::<_, String>(reversed(b))
        })
        .unwrap();
    shared.publish(&put, |_| b"{}".to_vec()).unwrap();
    let good = fs::read(&put.rlib_path).unwrap();
    let digest = digest_of(&good);
    let entry = shared.find(hash).unwrap().remove(0);
    let rlib = entry.rlib_path.clone();
    let rejected = |label: &str, compiled: CompiledDigest| {
        let w = new_workspace(label);
        let adopted = w
            .cache()
            .put_shared(&entry, lib_bytes, compiled, "LIBERTY", 1..=1)
            .unwrap();
        assert!(adopted.is_none(), "{label}");
        assert!(
            !w.cache().dir().exists() || entries(&w.cache().dir()).is_empty(),
            "{label}: {:?}",
            entries(&w.cache().dir())
        );
    };

    // Other bytes than the entry's.
    let w = new_workspace("shared-bad-bytes");
    assert!(
        w.cache()
            .put_shared(&entry, b"library (x) { }", digest, "LIBERTY", 1..=1)
            .unwrap()
            .is_none()
    );
    assert!(!w.cache().dir().exists() || entries(&w.cache().dir()).is_empty());

    // A valid .rlib of the right kind that is not the recorded one: the compile of other bytes
    // of the same length, and one of another length.
    replace_by_rename(&rlib, &reversed(b"library (cells_z) { }"));
    rejected("shared-bad-swapped", digest);
    replace_by_rename(&rlib, &reversed(b"library (cells_longer) { }"));
    rejected("shared-bad-length", digest);
    // The recorded digest of another file.
    replace_by_rename(&rlib, &good);
    rejected(
        "shared-bad-digest",
        CompiledDigest {
            hash: Hash64::of(b"other"),
            bytes: digest.bytes,
        },
    );

    // A .rlib of another kind or a damaged one, even when the record describes it.
    for bad in [
        RbinWriter::new("NETLIST", 1).unwrap().to_bytes(),
        b"not an rbin".to_vec(),
    ] {
        replace_by_rename(&rlib, &bad);
        rejected("shared-bad-rlib", digest_of(&bad));
    }
    // A symbolic link in place of the .rlib.
    fs::remove_file(&rlib).unwrap();
    std::os::unix::fs::symlink(&put.rlib_path, &rlib).unwrap();
    rejected("shared-bad-link", digest);
    assert!(
        shared.find(hash).unwrap().is_empty(),
        "find skips a symbolic link"
    );
    fs::remove_file(&rlib).unwrap();
    assert!(shared.find(hash).unwrap().is_empty(), "no .rlib, no entry");

    // Published again, the entry is adopted again.
    shared.publish(&put, |_| b"{}".to_vec()).unwrap();
    let w = new_workspace("shared-good-again");
    let entry = shared.find(hash).unwrap().remove(0);
    assert!(
        w.cache()
            .put_shared(&entry, lib_bytes, digest, "LIBERTY", 1..=1)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fs::read(w.cache().dir().join(format!("cells_a-{h16}.rlib"))).unwrap(),
        good
    );
}

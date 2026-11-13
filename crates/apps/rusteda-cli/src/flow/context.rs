// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! What every frontend command shares with the reference helper
//! (`native_frontend_inputs.py`): the pinned context of a run, SHA-256 digests with the run's
//! flow digest cache, file records, fresh workspaces and the JSON files they receive.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap as HashMap;
use sha2::{Digest, Sha256};

use crate::estimate::{Fail, Json, obj, parse_json, path_str};

/// A failure with the reference's code (`INPUT` when the reference raises a plain error).
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) code: String,
    pub(crate) message: String,
}

pub(crate) type Res<T> = Result<T, Failure>;

impl Failure {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Failure {
        Failure {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::new("IO", e.to_string())
    }
}

impl From<Fail> for Failure {
    fn from(e: Fail) -> Self {
        let code = if e.0.starts_with("BUDGET") {
            "BUDGET"
        } else {
            "INPUT"
        };
        Failure::new(code, e.0)
    }
}

impl From<serde_json::Error> for Failure {
    fn from(e: serde_json::Error) -> Self {
        Failure::new("INPUT", format!("invalid JSON: {e}"))
    }
}

/// `require(ok, code, text)` of the ECO planner.
pub(crate) fn require(ok: bool, code: &str, text: impl Into<String>) -> Res<()> {
    if ok {
        Ok(())
    } else {
        Err(Failure::new(code, text))
    }
}

/// `require(ok, text)` of the helper: the code is the text's own (`REVISION_CONFLICT`), else
/// `INPUT`.
pub(crate) fn need(ok: bool, text: impl Into<String>) -> Res<()> {
    if ok {
        return Ok(());
    }
    let text = text.into();
    let code = if text.starts_with("REVISION_CONFLICT") {
        "REVISION_CONFLICT"
    } else {
        "INPUT"
    };
    Err(Failure::new(code, text))
}

/// A missing key, as the reference's `KeyError` reads.
pub(crate) fn key_error(k: &str) -> Failure {
    Failure::new("INPUT", format!("'{k}'"))
}

/// `value[k]` of a JSON object.
pub(crate) fn at<'a>(v: &'a Json, k: &str) -> Res<&'a Json> {
    v.get(k).ok_or_else(|| key_error(k))
}

/// `value[k]` that must be a string.
pub(crate) fn text<'a>(v: &'a Json, k: &str) -> Res<&'a str> {
    at(v, k)?
        .as_str()
        .ok_or_else(|| Failure::new("INPUT", format!("string required: {k}")))
}

/// `str(path)`.
pub(crate) fn show(p: &Path) -> String {
    path_str(p)
}

/// `Path(p).resolve(strict=True)`.
pub(crate) fn resolve(p: &Path) -> Res<PathBuf> {
    fs::canonicalize(p).map_err(|e| Failure::new("IO", format!("{e}: '{}'", show(p))))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The identity of a file version: device, inode, size, mtime and ctime in nanoseconds. Every
/// write, rename and replacement changes it.
pub(crate) type Identity = [i64; 5];

pub(crate) fn identity(m: &fs::Metadata) -> Identity {
    [
        m.dev() as i64,
        m.ino() as i64,
        m.size() as i64,
        m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        m.ctime() * 1_000_000_000 + m.ctime_nsec(),
    ]
}

const FLOW_SCHEMA: &str = "rusteda.frontend-digests.v1";
/// File systems whose file status is exact (local ones); only their files are memoized.
const FS_LOCAL: [&str; 9] = [
    "ext2", "ext3", "ext4", "xfs", "btrfs", "tmpfs", "f2fs", "zfs", "overlay",
];

/// The run's flow digest cache `<run root>/frontend/sha256-cache.json`: the digests of pinned
/// inputs outside the run tree, by identity.
#[derive(Debug)]
struct Flow {
    root: PathBuf,
    path: PathBuf,
    /// In the order of the file, then of this process's additions.
    entries: Vec<(String, Identity, String)>,
    index: HashMap<String, usize>,
    dirty: bool,
}

/// The bytes of a file version and their SHA-256.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) bytes: Vec<u8>,
    pub(crate) sha256: String,
}

/// The digests of this process, as the reference's `MEMO` and `FLOW`.
#[derive(Debug, Default)]
pub(crate) struct Digests {
    memo: Mutex<HashMap<String, (Identity, String)>>,
    flow: Mutex<Option<Flow>>,
    kinds: Mutex<HashMap<u64, bool>>,
    loaded: Mutex<HashMap<String, (Identity, Arc<Loaded>)>>,
}

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// SHA-256 of the file's current content, always read in full, and its identity once read.
pub(crate) fn sha_fresh(path: &Path) -> io::Result<(String, Identity)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok((hex(&h.finalize()), identity(&f.metadata()?)))
}

/// Whether `s` is a SHA-256 in lowercase hexadecimal.
pub(crate) fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The valid entries of a flow digest cache file, none when it is missing or malformed.
fn flow_entries(path: &Path, root: &Path) -> Vec<(String, Identity, String)> {
    let read = || -> Option<Vec<(String, Identity, String)>> {
        let meta = fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > 4 << 20 {
            return None;
        }
        let data = parse_json(&fs::read_to_string(path).ok()?).ok()?;
        if data.get("schema")?.as_str()? != FLOW_SCHEMA || data.get("root")?.as_str()? != show(root)
        {
            return None;
        }
        let mut out = Vec::new();
        for (k, v) in data.get("entries")?.as_object()? {
            let Some(items) = v.as_object() else { continue };
            if items.len() != 2 {
                continue;
            }
            let (Some(id), Some(sha)) = (
                v.get("identity").and_then(Json::as_list),
                v.get("sha256").and_then(Json::as_str),
            ) else {
                continue;
            };
            let ints: Vec<i64> = id.iter().filter_map(Json::as_int).collect();
            if id.len() != 5 || ints.len() != 5 || !is_sha256(sha) {
                continue;
            }
            out.push((
                k.clone(),
                [ints[0], ints[1], ints[2], ints[3], ints[4]],
                sha.to_string(),
            ));
        }
        Some(out)
    };
    read().unwrap_or_default()
}

impl Digests {
    /// Whether the file of status `m` is on a local file system, by its device in
    /// `/proc/self/mountinfo`; false when that cannot be told.
    pub(crate) fn local(&self, m: &fs::Metadata) -> bool {
        let dev = m.dev();
        if let Some(&known) = lock(&self.kinds).get(&dev) {
            return known;
        }
        let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
        let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
        let want = format!("{major}:{minor}");
        let mut kind = None;
        if let Ok(text) = fs::read_to_string("/proc/self/mountinfo") {
            for line in text.lines() {
                let (left, right) = match line.split_once(" - ") {
                    Some(parts) => parts,
                    None => (line, ""),
                };
                let Some(found) = left.split_ascii_whitespace().nth(2) else {
                    // The reference stops with "cannot be told" on a short line.
                    kind = None;
                    break;
                };
                if found == want {
                    if let Some(k) = right.split_ascii_whitespace().next() {
                        kind = Some(k.to_string());
                        break;
                    }
                }
            }
        }
        let local = kind.is_some_and(|k| FS_LOCAL.contains(&k.as_str()));
        lock(&self.kinds).insert(dev, local);
        local
    }

    /// Opens the flow digest cache of the run at `root` when its `frontend` directory exists.
    pub(crate) fn open_flow(&self, root: &Path) {
        let path = root.join("frontend").join("sha256-cache.json");
        if !root.join("frontend").is_dir() {
            return;
        }
        let entries = flow_entries(&path, root);
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.0.clone(), i))
            .collect();
        *lock(&self.flow) = Some(Flow {
            root: root.to_path_buf(),
            path,
            entries,
            index,
            dirty: false,
        });
    }

    /// The SHA-256 of the resolved path `p`, hashed once per file version in this process; a
    /// pinned input outside the run tree whose identity matches the flow digest cache is not
    /// hashed again, and one hashed here joins the cache when its status changed at least a
    /// second before.
    pub(crate) fn digest(&self, p: &Path) -> Res<String> {
        let name = show(p);
        let meta = fs::metadata(p).map_err(|e| Failure::new("IO", format!("{e}: '{name}'")))?;
        let key = identity(&meta);
        if !self.local(&meta) {
            return Ok(sha_fresh(p)?.0);
        }
        if let Some(hit) = lock(&self.memo).get(&name) {
            if hit.0 == key {
                return Ok(hit.1.clone());
            }
        }
        let mut outside = false;
        if let Some(flow) = lock(&self.flow).as_ref() {
            outside = !p.starts_with(&flow.root);
            if outside {
                if let Some(&i) = flow.index.get(&name) {
                    if flow.entries[i].1 == key {
                        let sha = flow.entries[i].2.clone();
                        lock(&self.memo).insert(name, (key, sha.clone()));
                        return Ok(sha);
                    }
                }
            }
        }
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        let (sha, after) = sha_fresh(p)?;
        if after == key {
            lock(&self.memo).insert(name.clone(), (key, sha.clone()));
            if outside && key[4] < started - 1_000_000_000 {
                if let Some(flow) = lock(&self.flow).as_mut() {
                    match flow.index.get(&name) {
                        Some(&i) => flow.entries[i] = (name, key, sha.clone()),
                        None => {
                            flow.index.insert(name.clone(), flow.entries.len());
                            flow.entries.push((name, key, sha.clone()));
                        }
                    }
                    flow.dirty = true;
                }
            }
        }
        Ok(sha)
    }

    /// The bytes of the resolved path `p` and their SHA-256, read once per file version; the
    /// digest is the one of exactly these bytes.
    pub(crate) fn load(&self, p: &Path) -> Res<Arc<Loaded>> {
        let name = show(p);
        let fail = |e: io::Error| Failure::new("IO", format!("{e}: '{name}'"));
        let key = identity(&fs::metadata(p).map_err(fail)?);
        if let Some(hit) = lock(&self.loaded).get(&name) {
            if hit.0 == key {
                return Ok(hit.1.clone());
            }
        }
        let mut f = File::open(p).map_err(fail)?;
        let mut bytes = Vec::with_capacity(key[2].max(0) as usize + 1);
        f.read_to_end(&mut bytes).map_err(fail)?;
        let same = identity(&f.metadata().map_err(fail)?) == key;
        let loaded = Arc::new(Loaded {
            sha256: hex(&Sha256::digest(&bytes)),
            bytes,
        });
        if same {
            lock(&self.loaded).insert(name.clone(), (key, loaded.clone()));
            lock(&self.memo).insert(name, (key, loaded.sha256.clone()));
        }
        Ok(loaded)
    }

    /// `digest()` of every path on up to four threads: the digests land in the memo (and the
    /// flow cache) as the serial calls leave them. A path that cannot be hashed here is left to
    /// the serial code, which reports it.
    pub(crate) fn prefetch(&self, paths: &[PathBuf]) {
        let mut unique: Vec<&PathBuf> = Vec::new();
        for p in paths {
            if !unique.contains(&p) {
                unique.push(p);
            }
        }
        let workers = workers(unique.len());
        if workers <= 1 {
            for p in unique {
                let _ = self.digest(p);
            }
            return;
        }
        // The largest first: the longest read starts at once.
        unique.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).map_or(0, |m| m.len())));
        let next = Mutex::new(unique.into_iter());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let Some(p) = lock(&next).next() else { break };
                        let _ = self.digest(p);
                    }
                });
            }
        });
    }

    /// Merges this process's new entries into the flow digest cache (atomic replace). An entry
    /// already in the file stays as it is, so the final check sees every digest that was shared.
    /// The merge holds an exclusive lock on the cache's directory: helpers that run beside each
    /// other never drop each other's entries.
    pub(crate) fn save_flow(&self) {
        let mut guard = lock(&self.flow);
        let Some(flow) = guard.as_mut() else { return };
        if !flow.dirty {
            return;
        }
        let Some(dir) = flow.path.parent() else {
            return;
        };
        let tmp = dir.join(format!(".sha256-cache.json.tmp-{}", std::process::id()));
        let write = || -> io::Result<()> {
            let guard = File::open(dir)?;
            guard.lock()?;
            let merged = merged_entries(flow);
            let mut sorted: Vec<&(String, Identity, String)> = merged.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let entries = Json::Object(
                sorted
                    .into_iter()
                    .map(|(k, id, sha)| {
                        (
                            k.clone(),
                            obj(vec![
                                (
                                    "identity",
                                    Json::List(id.iter().map(|&x| Json::Int(x.into())).collect()),
                                ),
                                ("sha256", Json::Str(sha.clone())),
                            ]),
                        )
                    })
                    .collect(),
            );
            let doc = obj(vec![
                ("schema", Json::Str(FLOW_SCHEMA.into())),
                ("root", Json::Str(show(&flow.root))),
                ("entries", entries),
            ]);
            fs::write(&tmp, doc.dumps(Some(1)) + "\n")?;
            fs::rename(&tmp, &flow.path)
        };
        match write() {
            Ok(()) => flow.dirty = false,
            Err(_) => {
                let _ = fs::remove_file(&tmp);
            }
        }
    }

    /// The final check of the flow digest cache (`frontend-validate`): every entry is hashed
    /// again in full and must equal the digest that the run's helpers took from the cache;
    /// `digest-check.json` records the cache file and every digest checked.
    pub(crate) fn check_flow(&self, out: &Path) -> Res<()> {
        let check = |cache: Json, checked: Json| {
            write_json(
                &out.join("digest-check.json"),
                &obj(vec![
                    (
                        "schema",
                        Json::Str("rusteda.frontend-digest-check.v1".into()),
                    ),
                    ("cache", cache),
                    ("checked", checked),
                ]),
            )
        };
        if lock(&self.flow).is_none() {
            return check(Json::Null, Json::Object(Vec::new()));
        }
        self.save_flow();
        let guard = lock(&self.flow);
        let Some(flow) = guard.as_ref() else {
            return Ok(());
        };
        let exists = flow.path.exists();
        if exists {
            let data = read_json(&flow.path, 4 << 20)?;
            need(
                data.get("schema").and_then(Json::as_str) == Some(FLOW_SCHEMA)
                    && data.get("root").and_then(Json::as_str) == Some(&show(&flow.root)),
                "flow digest cache malformed",
            )?;
        }
        // What this process loaded and used, and what the file holds now (an entry is never
        // replaced).
        let mut entries = merged_entries(flow);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let digests: Vec<Option<String>> = {
            let slots: Vec<Mutex<Option<String>>> =
                entries.iter().map(|_| Mutex::new(None)).collect();
            // The largest first: the longest read starts at once.
            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by_key(|&k| std::cmp::Reverse(entries[k].1[2]));
            let next = Mutex::new(order.into_iter());
            std::thread::scope(|scope| {
                for _ in 0..workers(entries.len()) {
                    scope.spawn(|| {
                        loop {
                            let Some(k) = lock(&next).next() else { break };
                            let sha = sha_fresh(Path::new(&entries[k].0)).ok().map(|d| d.0);
                            *lock(&slots[k]) = sha;
                        }
                    });
                }
            });
            slots
                .into_iter()
                .map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner))
                .collect()
        };
        let differ: Vec<&str> = entries
            .iter()
            .zip(&digests)
            .filter(|(e, d)| d.as_deref() != Some(e.2.as_str()))
            .map(|(e, _)| e.0.as_str())
            .collect();
        need(
            differ.is_empty(),
            format!(
                "flow digest cache differs from the files: {}",
                differ.join(", ")
            ),
        )?;
        let cache = if exists {
            let bytes = fs::read(&flow.path)?;
            obj(vec![
                ("path", Json::Str(show(&flow.path))),
                ("sha256", Json::Str(hex(&Sha256::digest(&bytes)))),
                ("bytes", Json::Int(bytes.len() as i128)),
            ])
        } else {
            Json::Null
        };
        let checked = Json::Object(
            entries
                .iter()
                .map(|e| (e.0.clone(), Json::Str(e.2.clone())))
                .collect(),
        );
        check(cache, checked)
    }
}

/// `{**flow.entries, **entries of the file}`: this process's entries in their order, each
/// replaced by the file's entry of the same path, then the file's other entries.
fn merged_entries(flow: &Flow) -> Vec<(String, Identity, String)> {
    let mut merged = flow.entries.clone();
    let mut index: HashMap<String, usize> = flow.index.clone();
    for e in flow_entries(&flow.path, &flow.root) {
        match index.get(&e.0) {
            Some(&i) => merged[i] = e,
            None => {
                index.insert(e.0.clone(), merged.len());
                merged.push(e);
            }
        }
    }
    merged
}

/// Threads for hashing `count` files: the CPUs this process may use, at most four.
pub(crate) fn workers(count: usize) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    cpus.min(4).min(count).max(1)
}

/// `read(p, cap)`: the JSON document of a file within its byte budget.
pub(crate) fn read_json(p: &Path, cap: u64) -> Res<Json> {
    let ok = fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= cap);
    need(ok, format!("JSON byte budget: {}", show(p)))?;
    Ok(parse_json(&fs::read_to_string(p)?)?)
}

/// `write(p, data)`: `json.dumps(data, indent=2)` and a newline.
pub(crate) fn write_json(p: &Path, data: &Json) -> Res<()> {
    Ok(fs::write(p, data.dumps(Some(2)) + "\n")?)
}

/// The pinned context of a run (`rusteda.native-frontend-context.v1`).
#[derive(Debug)]
pub(crate) struct Context {
    pub(crate) doc: Json,
    /// The resolved run root.
    pub(crate) root: PathBuf,
    pub(crate) digests: Digests,
}

impl Context {
    /// Reads the context and checks every pinned input against its record (budget, path,
    /// SHA-256, size); the digests come from the flow digest cache where the identity holds.
    pub(crate) fn load(path: &Path) -> Res<Context> {
        let doc = read_json(path, 2 << 20)?;
        need(
            at(&doc, "schema")?.as_str() == Some("rusteda.native-frontend-context.v1"),
            "context schema",
        )?;
        let digests = Digests::default();
        let run_root = PathBuf::from(text(&doc, "run_root")?);
        // `Path.resolve()` of the run root, which exists.
        let root = fs::canonicalize(&run_root).unwrap_or(run_root);
        digests.open_flow(&root);
        const SMALL: [&str; 15] = [
            "sdc",
            "interfaces",
            "floorplan",
            "physical",
            "routing",
            "ecos",
            "frontend",
            "prepare",
            "producer",
            "delay_helper",
            "sizing_helper",
            "liberty_reader",
            "pin_caps_source",
            "estimator",
            "rc_policy",
        ];
        let groups = |doc: &Json| -> Res<Vec<(&'static str, Vec<(String, Json)>)>> {
            let mut out = Vec::new();
            for group in ["files", "tools"] {
                let items = at(doc, group)?
                    .as_object()
                    .ok_or_else(|| Failure::new("INPUT", format!("object required: {group}")))?;
                out.push((group, items.to_vec()));
            }
            Ok(out)
        };
        let groups = groups(&doc)?;
        for (group, items) in &groups {
            for (name, item) in items {
                let cap: i64 = if *group == "tools" {
                    256 << 20
                } else if SMALL.contains(&name.as_str()) {
                    2 << 20
                } else {
                    64 << 20
                };
                let bytes = at(item, "bytes")?
                    .number()
                    .ok_or_else(|| Failure::new("INPUT", "bytes"))?;
                need(bytes <= cap as f64, format!("input byte budget: {name}"))?;
            }
        }
        let mut paths = Vec::new();
        for (_, items) in &groups {
            for (_, item) in items {
                paths.push(resolve(Path::new(text(item, "path")?))?);
            }
        }
        digests.prefetch(&paths);
        for (_, items) in &groups {
            for (name, item) in items {
                let record = rec(&digests, Path::new(text(item, "path")?))?;
                need(record.py_eq(item), format!("pinned input changed: {name}"))?;
            }
        }
        let files = at(&doc, "files")?;
        let mut liberty = 0i128;
        for k in ["std_tt", "std_ss", "ram_tt", "ram_ss"] {
            liberty += at(at(files, k)?, "bytes")?.as_int().unwrap_or(i64::MAX) as i128;
        }
        need(liberty <= 64 << 20, "aggregate Liberty budget")?;
        Ok(Context { doc, root, digests })
    }

    /// `paths(c)[key]`: the path of a pinned file as the context writes it.
    pub(crate) fn file(&self, key: &str) -> Res<PathBuf> {
        Ok(PathBuf::from(text(
            at(at(&self.doc, "files")?, key)?,
            "path",
        )?))
    }

    pub(crate) fn selection(&self) -> Res<&Json> {
        at(&self.doc, "selection")
    }

    /// `rec(p)`.
    pub(crate) fn rec(&self, p: &Path) -> Res<Json> {
        rec(&self.digests, p)
    }

    /// `dynamic(c, path)`: a resolved path that must lie inside this run.
    pub(crate) fn dynamic(&self, p: &Path) -> Res<PathBuf> {
        let p = resolve(p)?;
        need(
            p.starts_with(&self.root),
            "historical/external dynamic input forbidden",
        )?;
        Ok(p)
    }
}

/// `rec(p)`: `{path, sha256, bytes}` of the resolved file.
pub(crate) fn rec(digests: &Digests, p: &Path) -> Res<Json> {
    let p = resolve(p)?;
    let sha = digests.digest(&p)?;
    let bytes = fs::metadata(&p)?.len();
    Ok(obj(vec![
        ("path", Json::Str(show(&p))),
        ("sha256", Json::Str(sha)),
        ("bytes", Json::Int(bytes.into())),
    ]))
}

/// `fresh(path, base)`: creates the workspace, takes its lock exclusively and requires it empty.
pub(crate) fn fresh(path: &Path, base: &str) -> Res<File> {
    fs::create_dir_all(path)?;
    let lock = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path.join("workspace.lock"))?;
    lock.try_lock().map_err(|_| {
        Failure::new(
            "WORKSPACE_LOCKED",
            "[Errno 11] Resource temporarily unavailable",
        )
    })?;
    let mut others = false;
    for entry in fs::read_dir(path)? {
        if entry?.file_name() != "workspace.lock" {
            others = true;
        }
    }
    need(base == "empty" && !others, "REVISION_CONFLICT")?;
    Ok(lock)
}

/// The peak resident set size of this process in KiB (`VmHWM`), 0 when it cannot be read.
pub(crate) fn peak_rss_kib() -> i64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_ascii_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

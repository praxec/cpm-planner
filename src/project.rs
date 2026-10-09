//! Project-root discovery and confined plan-file paths.
//!
//! Plan files live at `<root>/.cpm-planner/plans/<name>/<variant>.json`.
//! Caller-supplied paths are relative to the root and validated component by
//! component: no `..`, no absolute paths, no backslashes, no extra
//! components, slug-checked name/variant.
//!
//! # Guarantees
//!
//! All filesystem access walks from a handle on the canonical root through
//! `.cpm-planner` -> `plans` -> `<name>` using `cap-std` directories opened
//! with no-follow semantics (`O_NOFOLLOW` on unix; reparse points are not
//! followed on Windows). A symlink at any step is rejected with
//! `INVALID_PATH`, and every later operation (create temp file, write, fsync,
//! rename, read) is performed relative to the opened directory handle rather
//! than by re-resolving the original path string, so swapping a parent for a
//! symlink after the check cannot redirect I/O outside the root. Writes are
//! atomic: a create-new temp file in the final directory is fsynced and
//! renamed over `<variant>.json`; the directory is fsynced afterwards.
//! Metadata errors other than NotFound fail closed. On unix, reads open the
//! file with `O_NONBLOCK` so a FIFO cannot block the open, then verify via
//! `fstat` that the handle is a regular file before reading it.
//!
//! # Residual limitations
//!
//! - Directory fsync is best-effort on Windows (opening a directory for sync
//!   is not supported there), so crash-durability of the rename is not
//!   guaranteed on Windows. Atomicity of the rename is unaffected.
//! - The canonical root itself is resolved once (symlinks in the root path
//!   above the project are legitimate and followed at that point).
//!
//! # Residual limitations (Windows)
//!
//! On Windows, `create_dir`, `remove_file`/`remove_dir`, and `rename` are
//! path-based: `cap-std` rebuilds the target path from the directory handle
//! via `GetFinalPathNameByHandle` before issuing the Win32 call. A parent that
//! is swapped for a reparse point inside that window can therefore make those
//! calls act on the wrong location. The worst observable effect is an empty
//! directory created outside the root (or a rename that fails); no plan bytes
//! can be written outside the root, because file creation, writes, and fsync
//! all occur on handles opened relative to the confined directory.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt as _;
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};

use crate::plan::{PlanGraph, PlannerError};

/// Environment variable overriding repo-root discovery.
pub const PROJECT_ROOT_ENV: &str = "CPM_PROJECT_ROOT";

const PLANNER_DIR: &str = ".cpm-planner";
const PLANS_SUBDIR: &str = "plans";

/// A validated plan file location (not necessarily existing yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileRef {
    pub name: String,
    pub variant: String,
    /// `.cpm-planner/plans/<name>/<variant>.json`, relative to the root.
    pub rel_path: String,
}

/// A canonical project root.
#[derive(Debug, Clone)]
pub struct ProjectRoot {
    root: PathBuf,
}

fn invalid(reason: impl Into<String>) -> PlannerError {
    PlannerError::InvalidPath {
        reason: reason.into(),
    }
}

fn backend(err: impl Into<anyhow::Error>) -> PlannerError {
    PlannerError::BackendError(err.into())
}

const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate a `name` / `variant`: `^[a-z0-9][a-z0-9._-]{0,62}[a-z0-9]$` (or a
/// single `[a-z0-9]`), at most 64 chars, and not a Windows reserved device
/// name (`con`, `nul`, `com1`...; also with an extension, e.g. `con.x`),
/// enforced on every platform so plans are portable.
pub fn validate_slug(kind: &str, s: &str) -> Result<(), PlannerError> {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = s.as_bytes();
    let shape_ok = !bytes.is_empty()
        && bytes.len() <= 64
        && alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| alnum(b) || matches!(b, b'.' | b'_' | b'-'));
    if !shape_ok {
        return Err(invalid(format!(
            "{kind} {s:?} must match ^[a-z0-9][a-z0-9._-]{{0,63}}$ and end in [a-z0-9]"
        )));
    }
    let stem = s.split('.').next().unwrap_or(s);
    if WINDOWS_RESERVED.contains(&stem) {
        return Err(invalid(format!("{kind} {s:?} is a reserved device name")));
    }
    Ok(())
}

/// Open `name` under `parent` without following symlinks. `Ok(None)` only on
/// NotFound; a symlink is `INVALID_PATH`; anything else fails closed.
fn open_subdir(parent: &Dir, name: &str) -> Result<Option<Dir>, PlannerError> {
    match parent.symlink_metadata(name) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(invalid(format!(
                "{name} is a symlink; symlinks are not allowed"
            )));
        }
        Ok(m) if !m.is_dir() => {
            return Err(invalid(format!("{name} is not a directory")));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(backend(e)),
    }
    match parent.open_dir_nofollow(name) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // Raced into a symlink / non-directory between lstat and open.
        Err(e) => Err(invalid(format!(
            "cannot open {name} without following links: {e}"
        ))),
    }
}

/// Like [`open_subdir`] but creates the directory when absent (mkdirat,
/// EEXIST ignored) and fsyncs the parent when it was created.
fn ensure_subdir(parent: &Dir, name: &str) -> Result<Dir, PlannerError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(backend(e)),
    }
    open_subdir(parent, name)?.ok_or_else(|| invalid(format!("{name} vanished while being opened")))
}

/// fsync a directory handle. Best-effort (ignored) on non-unix platforms.
fn sync_dir(dir: &Dir) -> Result<(), PlannerError> {
    #[cfg(unix)]
    {
        dir.open(".").and_then(|f| f.sync_all()).map_err(backend)?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

impl ProjectRoot {
    /// `CPM_PROJECT_ROOT` if set, else the nearest ancestor of `cwd` (itself
    /// included) containing `.cpm-planner/` or `.git`.
    pub fn discover(cwd: &Path) -> Option<ProjectRoot> {
        if let Some(v) = std::env::var_os(PROJECT_ROOT_ENV).filter(|v| !v.is_empty()) {
            return Self::from_path(Path::new(&v)).ok();
        }
        let start = cwd.canonicalize().ok()?;
        start
            .ancestors()
            .find(|d| d.join(PLANNER_DIR).is_dir() || d.join(".git").exists())
            .map(|d| ProjectRoot {
                root: d.to_path_buf(),
            })
    }

    /// Build a root from an existing directory (canonicalized).
    pub fn from_path(root: &Path) -> Result<ProjectRoot, PlannerError> {
        let root = root
            .canonicalize()
            .map_err(|e| invalid(format!("project root {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(invalid(format!(
                "project root {} is not a directory",
                root.display()
            )));
        }
        Ok(ProjectRoot { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical root as a UTF-8 string (lossy for non-UTF-8 paths).
    pub fn project_key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.root.join(PLANNER_DIR).join(PLANS_SUBDIR)
    }

    /// Validate a root-relative path of the exact form
    /// `.cpm-planner/plans/<name>/<variant>.json`.
    pub fn resolve_plan_file(&self, rel: &str) -> Result<PlanFileRef, PlannerError> {
        if rel.contains('\0') || rel.contains('\\') {
            return Err(invalid("path contains NUL or backslash"));
        }
        if rel.starts_with('/') || Path::new(rel).is_absolute() {
            return Err(invalid("absolute paths are not allowed"));
        }
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.contains(&"..") {
            return Err(invalid("`..` is not allowed"));
        }
        let [planner, plans, name, file] = parts.as_slice() else {
            return Err(invalid(format!(
                "path must be {PLANNER_DIR}/{PLANS_SUBDIR}/<name>/<variant>.json, got {rel:?}"
            )));
        };
        if *planner != PLANNER_DIR || *plans != PLANS_SUBDIR {
            return Err(invalid(format!(
                "path must be inside {PLANNER_DIR}/{PLANS_SUBDIR}/, got {rel:?}"
            )));
        }
        let Some(variant) = file.strip_suffix(".json") else {
            return Err(invalid(format!("plan file {file:?} must end in .json")));
        };
        self.plan_file(name, variant)
    }

    /// Build and confinement-check the ref for `<name>/<variant>`.
    pub fn plan_file(&self, name: &str, variant: &str) -> Result<PlanFileRef, PlannerError> {
        validate_slug("name", name)?;
        validate_slug("variant", variant)?;
        let f = PlanFileRef {
            name: name.to_string(),
            variant: variant.to_string(),
            rel_path: format!("{PLANNER_DIR}/{PLANS_SUBDIR}/{name}/{variant}.json"),
        };
        self.check_confined(&f)?;
        Ok(f)
    }

    /// Walk root -> `.cpm-planner` -> `plans` -> (`name`) with no-follow
    /// opens. `create` makes missing directories. `Ok(None)` when a directory
    /// is missing (only possible without `create`).
    fn open_chain(&self, name: Option<&str>, create: bool) -> Result<Option<Dir>, PlannerError> {
        let root = Dir::open_ambient_dir(&self.root, ambient_authority()).map_err(backend)?;
        let mut cur = root;
        let mut parts = vec![PLANNER_DIR, PLANS_SUBDIR];
        parts.extend(name);
        for part in parts {
            cur = if create {
                ensure_subdir(&cur, part)?
            } else {
                match open_subdir(&cur, part)? {
                    Some(d) => d,
                    None => return Ok(None),
                }
            };
        }
        Ok(Some(cur))
    }

    /// Confinement check: no symlink anywhere on the path (including the
    /// file itself, if it exists).
    fn check_confined(&self, f: &PlanFileRef) -> Result<(), PlannerError> {
        if let Some(dir) = self.open_chain(Some(&f.name), false)? {
            match dir.symlink_metadata(format!("{}.json", f.variant)) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(invalid(format!(
                        "{} is a symlink; symlinks are not allowed",
                        f.rel_path
                    )));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(backend(e)),
            }
        }
        Ok(())
    }

    /// All plan files, sorted by (name, variant). Non-conforming entries and
    /// symlinks are ignored. Empty only when the plans dir does not exist;
    /// other errors propagate.
    pub fn list_plan_files(&self) -> Result<Vec<PlanFileRef>, PlannerError> {
        let mut out = Vec::new();
        let Some(plans) = self.open_chain(None, false)? else {
            return Ok(out);
        };
        for n in plans.entries().map_err(backend)? {
            let n = n.map_err(backend)?;
            let Some(name) = n.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !n.file_type().map_err(backend)?.is_dir() || validate_slug("name", &name).is_err() {
                continue;
            }
            let Some(dir) = open_subdir(&plans, &name)? else {
                continue;
            };
            for f in dir.entries().map_err(backend)? {
                let f = f.map_err(backend)?;
                let Some(file) = f.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Some(variant) = file.strip_suffix(".json") else {
                    continue;
                };
                if !f.file_type().map_err(backend)?.is_file()
                    || validate_slug("variant", variant).is_err()
                {
                    continue;
                }
                out.push(PlanFileRef {
                    name: name.clone(),
                    variant: variant.to_string(),
                    rel_path: format!("{PLANNER_DIR}/{PLANS_SUBDIR}/{name}/{variant}.json"),
                });
            }
        }
        out.sort_by(|a, b| (&a.name, &a.variant).cmp(&(&b.name, &b.variant)));
        Ok(out)
    }

    /// Read a plan and the sha256 hex of the exact bytes parsed. Directories
    /// are walked with no-follow handles; the file is opened no-follow
    /// relative to the final directory handle, fstat'd to be a regular file,
    /// and read from that same handle.
    pub fn read_graph(&self, f: &PlanFileRef) -> Result<(PlanGraph, String), PlannerError> {
        let bytes = self.read_bytes(f)?;
        let graph = serde_json::from_slice(&bytes).map_err(|e| PlannerError::InvalidGraph {
            reason: format!("{}: {e}", f.rel_path),
        })?;
        Ok((graph, hash_hex(&bytes)))
    }

    /// The sha256 hex of a plan file's bytes (the hash [`Self::read_graph`]
    /// and [`Self::write_graph`] return), without parsing it. Read with the
    /// same no-follow, regular-file-only rules as [`Self::read_graph`].
    pub fn file_hash(&self, f: &PlanFileRef) -> Result<String, PlannerError> {
        Ok(hash_hex(&self.read_bytes(f)?))
    }

    fn read_bytes(&self, f: &PlanFileRef) -> Result<Vec<u8>, PlannerError> {
        validate_slug("name", &f.name)?;
        validate_slug("variant", &f.variant)?;
        let dir = self
            .open_chain(Some(&f.name), false)?
            .ok_or_else(|| invalid(format!("{} does not exist", f.rel_path)))?;
        let fname = format!("{}.json", f.variant);
        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);
        // Opening a FIFO read-only blocks until a writer appears. O_NONBLOCK
        // makes the open return immediately; regular files ignore it, so the
        // subsequent fstat + read behave as before. Non-regular handles are
        // rejected below.
        #[cfg(unix)]
        opts.nonblock(true);
        let mut file = match dir.open_with(&fname, &opts) {
            Ok(file) => file,
            Err(e) => {
                return Err(match dir.symlink_metadata(&fname) {
                    Ok(m) if m.file_type().is_symlink() => {
                        invalid(format!("{} is a symlink", f.rel_path))
                    }
                    _ => backend(e),
                });
            }
        };
        if !file.metadata().map_err(backend)?.is_file() {
            return Err(invalid(format!("{} is not a regular file", f.rel_path)));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(backend)?;
        Ok(bytes)
    }

    /// Atomically write pretty JSON plus a trailing newline; returns the
    /// content hash. All directory steps use no-follow handles and every file
    /// operation is relative to the final directory handle, so a parent
    /// swapped for a symlink mid-call cannot redirect the write.
    pub fn write_graph(&self, f: &PlanFileRef, g: &PlanGraph) -> Result<String, PlannerError> {
        self.write_graph_inner(f, g, true)
    }

    /// [`Self::write_graph`] that never replaces an existing file: the new
    /// file is linked into place atomically, and an existing one (written
    /// concurrently included) is `INVALID_PATH: <rel_path> already exists`.
    pub fn write_new_graph(&self, f: &PlanFileRef, g: &PlanGraph) -> Result<String, PlannerError> {
        self.write_graph_inner(f, g, false)
    }

    fn write_graph_inner(
        &self,
        f: &PlanFileRef,
        g: &PlanGraph,
        replace: bool,
    ) -> Result<String, PlannerError> {
        validate_slug("name", &f.name)?;
        validate_slug("variant", &f.variant)?;
        let mut bytes = serde_json::to_vec_pretty(g).map_err(backend)?;
        bytes.push(b'\n');
        let dir = self
            .open_chain(Some(&f.name), true)?
            .ok_or_else(|| invalid("plan directory vanished"))?;
        let target = format!("{}.json", f.variant);
        match dir.symlink_metadata(&target) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "{} is a symlink; symlinks are not allowed",
                    f.rel_path
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(backend(e)),
        }
        let tmp = format!(".{}.tmp-{}", f.variant, uuid::Uuid::new_v4());
        let result = (|| -> std::io::Result<()> {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true).follow(FollowSymlinks::No);
            let mut file = dir.open_with(&tmp, &opts)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            if replace {
                dir.rename(&tmp, &dir, &target)
            } else {
                // link(2) fails with EEXIST instead of replacing.
                let linked = dir.hard_link(&tmp, &dir, &target);
                let _ = dir.remove_file(&tmp);
                linked
            }
        })();
        if let Err(e) = result {
            let _ = dir.remove_file(&tmp);
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(invalid(format!("{} already exists", f.rel_path)));
            }
            return Err(backend(e));
        }
        sync_dir(&dir)?;
        Ok(hash_hex(&bytes))
    }
}

fn hash_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

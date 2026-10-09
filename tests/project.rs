use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::project::{ProjectRoot, validate_slug};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV: &str = "CPM_PROJECT_ROOT";

/// Temp dir removed on drop; derefs to the canonical path.
struct Tmp {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for Tmp {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

fn tmp(label: &str) -> Tmp {
    let dir = tempfile::Builder::new()
        .prefix(&format!("cpm-project-{label}-"))
        .tempdir()
        .unwrap();
    let path = dir.path().canonicalize().unwrap();
    Tmp { _dir: dir, path }
}

fn root_with_git(label: &str) -> (Tmp, ProjectRoot) {
    let d = tmp(label);
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let r = ProjectRoot::from_path(&d).unwrap();
    (d, r)
}

fn graph() -> PlanGraph {
    serde_json::from_value(serde_json::json!({
        "deliverables": [{
            "id": "a",
            "owned_files": ["a.rs"],
            "prerequisites": []
        }]
    }))
    .unwrap()
}

fn forged_ref(name: &str, variant: &str) -> cpm_planner::project::PlanFileRef {
    cpm_planner::project::PlanFileRef {
        name: name.into(),
        variant: variant.into(),
        rel_path: format!(".cpm-planner/plans/{name}/{variant}.json"),
    }
}

fn is_invalid_path<T: std::fmt::Debug>(r: Result<T, PlannerError>) -> bool {
    matches!(r, Err(PlannerError::InvalidPath { .. }))
}

struct EnvGuard {
    old: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by ENV_LOCK, which this guard still holds.
        unsafe {
            match self.old.take() {
                Some(v) => std::env::set_var(ENV, v),
                None => std::env::remove_var(ENV),
            }
        }
    }
}

fn with_env<R>(value: Option<&Path>, f: impl FnOnce() -> R) -> R {
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard {
        old: std::env::var_os(ENV),
        _lock: lock,
    };
    // SAFETY: serialized by ENV_LOCK.
    unsafe {
        match value {
            Some(v) => std::env::set_var(ENV, v),
            None => std::env::remove_var(ENV),
        }
    }
    f()
}

#[test]
fn discover_finds_git_root_from_nested_dir() {
    let (d, _r) = root_with_git("git");
    let nested = d.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    let found = with_env(None, || ProjectRoot::discover(&nested));
    assert_eq!(found.unwrap().root(), *d);
}

#[test]
fn discover_prefers_cpm_planner_dir() {
    let (d, _r) = root_with_git("pref");
    let inner = d.join("sub");
    std::fs::create_dir_all(inner.join(".cpm-planner")).unwrap();
    let found = with_env(None, || ProjectRoot::discover(&inner.join(".cpm-planner")));
    assert_eq!(found.unwrap().root(), inner);
}

#[test]
fn env_override_wins() {
    let (d, _r) = root_with_git("envgit");
    let other = tmp("envother");
    let found = with_env(Some(&other), || ProjectRoot::discover(&d));
    assert_eq!(found.unwrap().root(), *other);
}

#[test]
fn resolve_rejects_parent_traversal() {
    let (_d, r) = root_with_git("dots");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/../../etc/x.json")
    ));
}

#[test]
fn resolve_rejects_absolute_path() {
    let (d, r) = root_with_git("abs");
    let abs = d.join(".cpm-planner/plans/a/b.json");
    assert!(is_invalid_path(r.resolve_plan_file(abs.to_str().unwrap())));
}

#[cfg(unix)]
#[test]
fn resolve_rejects_symlink_escaping_root() {
    let (d, r) = root_with_git("sym");
    let outside = tmp("symout");
    std::fs::create_dir_all(d.join(".cpm-planner/plans")).unwrap();
    std::os::unix::fs::symlink(&*outside, d.join(".cpm-planner/plans/evil")).unwrap();
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/evil/x.json")
    ));
}

#[cfg(unix)]
#[test]
fn write_refuses_to_follow_symlinked_file() {
    let (d, r) = root_with_git("symfile");
    let outside = tmp("symfileout");
    let victim = outside.join("victim.json");
    std::fs::write(&victim, "keep").unwrap();
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("b.json")).unwrap();
    let f = forged_ref("a", "b");
    assert!(is_invalid_path(r.write_graph(&f, &graph())));
}

#[cfg(unix)]
#[test]
fn write_leaves_symlink_target_untouched() {
    let (d, r) = root_with_git("symkeep");
    let outside = tmp("symkeepout");
    let victim = outside.join("victim.json");
    std::fs::write(&victim, "keep").unwrap();
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("b.json")).unwrap();
    let f = forged_ref("a", "b");
    let _ = r.write_graph(&f, &graph());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
}

#[test]
fn resolve_rejects_path_outside_plans_dir() {
    let (_d, r) = root_with_git("outside");
    assert!(is_invalid_path(r.resolve_plan_file("src/lib.rs")));
}

#[test]
fn resolve_rejects_missing_json_extension() {
    let (_d, r) = root_with_git("noext");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b.txt")
    ));
}

#[test]
fn resolve_rejects_extra_path_components() {
    let (_d, r) = root_with_git("extra");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b/c.json")
    ));
}

#[test]
fn resolve_rejects_missing_variant_component() {
    let (_d, r) = root_with_git("short");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a.json")
    ));
}

#[test]
fn resolve_rejects_backslash_separators() {
    let (_d, r) = root_with_git("bslash");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner\\plans\\a\\b.json")
    ));
}

#[test]
fn resolve_rejects_nul_byte() {
    let (_d, r) = root_with_git("nul");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/a/b\0.json")
    ));
}

#[test]
fn resolve_parses_name_and_variant() {
    let (_d, r) = root_with_git("parse");
    let f = r
        .resolve_plan_file(".cpm-planner/plans/my-plan/base.json")
        .unwrap();
    assert_eq!((f.name.as_str(), f.variant.as_str()), ("my-plan", "base"));
}

#[test]
fn plan_file_builds_canonical_rel_path() {
    let (_d, r) = root_with_git("relpath");
    let f = r.plan_file("p", "v1").unwrap();
    assert_eq!(f.rel_path, ".cpm-planner/plans/p/v1.json");
}

#[test]
fn invalid_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", "Bad Name")));
}

#[test]
fn slug_with_leading_dot_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", ".hidden")));
}

#[test]
fn slug_longer_than_64_chars_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", &"a".repeat(65))));
}

#[test]
fn slug_of_64_chars_is_accepted() {
    assert!(validate_slug("name", &"a".repeat(64)).is_ok());
}

#[test]
fn empty_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("variant", "")));
}

#[test]
fn write_then_read_round_trips_graph() {
    let (_d, r) = root_with_git("rt");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    let (g, _) = r.read_graph(&f).unwrap();
    assert_eq!(g.deliverables[0].id, "a");
}

#[test]
fn read_hash_matches_write_hash() {
    let (_d, r) = root_with_git("hash");
    let f = r.plan_file("p", "v").unwrap();
    let w = r.write_graph(&f, &graph()).unwrap();
    let (_, h) = r.read_graph(&f).unwrap();
    assert_eq!(w, h);
}

#[test]
fn hash_is_sha256_hex_of_file_bytes() {
    use sha2::{Digest, Sha256};
    let (d, r) = root_with_git("sha");
    let f = r.plan_file("p", "v").unwrap();
    let w = r.write_graph(&f, &graph()).unwrap();
    let bytes = std::fs::read(d.join(&f.rel_path)).unwrap();
    assert_eq!(w, format!("{:x}", Sha256::digest(&bytes)));
}

#[test]
fn written_file_ends_with_newline() {
    let (d, r) = root_with_git("nl");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    assert!(
        std::fs::read_to_string(d.join(&f.rel_path))
            .unwrap()
            .ends_with('\n')
    );
}

#[test]
fn write_creates_directories() {
    let (d, r) = root_with_git("mk");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    assert!(d.join(".cpm-planner/plans/p/v.json").is_file());
}

#[test]
fn write_leaves_no_temp_files_behind() {
    let (d, r) = root_with_git("atomic");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    r.write_graph(&f, &graph()).unwrap();
    let n = std::fs::read_dir(d.join(".cpm-planner/plans/p"))
        .unwrap()
        .count();
    assert_eq!(n, 1);
}

#[test]
fn list_plan_files_is_sorted() {
    let (_d, r) = root_with_git("list");
    for (n, v) in [("b", "x"), ("a", "z"), ("a", "y")] {
        let f = r.plan_file(n, v).unwrap();
        r.write_graph(&f, &graph()).unwrap();
    }
    let got: Vec<_> = r
        .list_plan_files()
        .unwrap()
        .into_iter()
        .map(|f| (f.name, f.variant))
        .collect();
    let want: Vec<_> = [("a", "y"), ("a", "z"), ("b", "x")]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn list_plan_files_is_empty_without_plans_dir() {
    let (_d, r) = root_with_git("nolist");
    assert!(r.list_plan_files().unwrap().is_empty());
}

#[test]
fn list_plan_files_ignores_non_plan_entries() {
    let (d, r) = root_with_git("junk");
    let f = r.plan_file("a", "b").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    std::fs::write(d.join(".cpm-planner/plans/a/notes.txt"), "x").unwrap();
    std::fs::write(d.join(".cpm-planner/plans/stray.json"), "x").unwrap();
    assert_eq!(r.list_plan_files().unwrap().len(), 1);
}

#[test]
fn project_key_is_canonical_root_string() {
    let (d, r) = root_with_git("key");
    assert_eq!(r.project_key(), d.to_str().unwrap());
}

#[test]
fn from_path_rejects_missing_directory() {
    let d = tmp("missing");
    assert!(ProjectRoot::from_path(&d.join("nope")).is_err());
}

#[cfg(unix)]
#[test]
fn plan_file_rejects_symlinked_plan_file() {
    let (d, r) = root_with_git("symref");
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(d.join("elsewhere.json"), dir.join("b.json")).unwrap();
    assert!(is_invalid_path(r.plan_file("a", "b")));
}

#[test]
fn invalid_project_root_env_returns_none() {
    let (d, _r) = root_with_git("badenv");
    let bogus = d.join("does-not-exist");
    let found = with_env(Some(&bogus), || ProjectRoot::discover(&d));
    assert!(found.is_none());
}

#[cfg(unix)]
#[test]
fn symlinked_cpm_planner_dir_is_rejected() {
    let (d, r) = root_with_git("symplanner");
    let outside = tmp("symplannerout");
    std::os::unix::fs::symlink(&*outside, d.join(".cpm-planner")).unwrap();
    assert!(is_invalid_path(
        r.write_graph(&forged_ref("a", "b"), &graph())
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_plans_dir_is_rejected() {
    let (d, r) = root_with_git("symplans");
    let outside = tmp("symplansout");
    std::fs::create_dir_all(d.join(".cpm-planner")).unwrap();
    std::os::unix::fs::symlink(&*outside, d.join(".cpm-planner/plans")).unwrap();
    assert!(is_invalid_path(
        r.write_graph(&forged_ref("a", "b"), &graph())
    ));
}

#[cfg(unix)]
#[test]
fn write_through_symlinked_plans_dir_creates_nothing_outside() {
    let (d, r) = root_with_git("symplansnone");
    let outside = tmp("symplansnoneout");
    std::fs::create_dir_all(d.join(".cpm-planner")).unwrap();
    std::os::unix::fs::symlink(&*outside, d.join(".cpm-planner/plans")).unwrap();
    let _ = r.write_graph(&forged_ref("a", "b"), &graph());
    assert_eq!(std::fs::read_dir(&*outside).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn read_rejects_symlinked_file() {
    let (d, r) = root_with_git("readsym");
    let outside = tmp("readsymout");
    std::fs::write(outside.join("x.json"), "{}").unwrap();
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(outside.join("x.json"), dir.join("b.json")).unwrap();
    assert!(is_invalid_path(r.read_graph(&forged_ref("a", "b"))));
}

#[test]
fn traversal_with_four_components_is_rejected() {
    let (_d, r) = root_with_git("four");
    assert!(is_invalid_path(
        r.resolve_plan_file(".cpm-planner/plans/../x.json")
    ));
}

#[test]
fn trailing_dot_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", "abc.")));
}

#[test]
fn windows_reserved_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("name", "con.x")));
}

#[test]
fn windows_reserved_com_port_slug_is_rejected() {
    assert!(is_invalid_path(validate_slug("variant", "com1")));
}

#[test]
fn slug_merely_containing_reserved_word_is_accepted() {
    assert!(validate_slug("name", "console").is_ok());
}

#[test]
fn write_is_atomic_and_creates_directories() {
    let (d, r) = root_with_git("umbrella");
    let f = r.plan_file("p", "v").unwrap();
    r.write_graph(&f, &graph()).unwrap();
    let names: Vec<_> = std::fs::read_dir(d.join(".cpm-planner/plans/p"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["v.json".to_string()]);
}

#[cfg(unix)]
#[test]
fn concurrent_parent_swap_never_writes_outside_root() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ATTEMPTS: usize = 3000;
    let (d, r) = root_with_git("race");
    let outside = tmp("raceout");
    // Pre-create `.cpm-planner/plans/a` so both directory levels can be
    // swapped for symlinks by the racing thread.
    std::fs::create_dir_all(d.join(".cpm-planner/plans/a")).unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let made = Arc::new(AtomicUsize::new(0));
    let swapper = {
        let (d, outside, done) = (d.path.clone(), outside.path.clone(), done.clone());
        std::thread::spawn(move || {
            let planner = d.join(".cpm-planner");
            let name_dir = planner.join("plans").join("a");
            while !done.load(Ordering::SeqCst) {
                // Level 1: swap `.cpm-planner` itself for an outside symlink,
                // then restore a real tree before touching the level below.
                let _ = std::fs::remove_file(&planner);
                let _ = std::fs::remove_dir_all(&planner);
                let _ = std::os::unix::fs::symlink(&*outside, &planner);
                std::thread::yield_now();
                let _ = std::fs::remove_file(&planner);
                let _ = std::fs::create_dir_all(&name_dir);
                std::thread::yield_now();

                // Level 2: swap the `<name>` directory for an outside symlink.
                let _ = std::fs::remove_file(&name_dir);
                let _ = std::fs::remove_dir_all(&name_dir);
                let _ = std::os::unix::fs::symlink(&*outside, &name_dir);
                std::thread::yield_now();
                let _ = std::fs::remove_file(&name_dir);
                let _ = std::fs::create_dir_all(&name_dir);
                std::thread::yield_now();
            }
        })
    };

    let f = forged_ref("a", "v");
    let g = graph();
    while made.fetch_add(1, Ordering::SeqCst) < ATTEMPTS {
        let _ = r.write_graph(&f, &g);
    }
    done.store(true, Ordering::SeqCst);
    swapper.join().unwrap();
    assert_eq!(std::fs::read_dir(&*outside).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn read_rejects_fifo_without_hanging() {
    let (d, r) = root_with_git("fifo");
    let dir = d.join(".cpm-planner/plans/a");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("b.json");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed");
    let f = forged_ref("a", "b");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(r.read_graph(&f));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(res) => assert!(is_invalid_path(res)),
        Err(_) => panic!("read_graph blocked on a FIFO instead of rejecting it"),
    }
}

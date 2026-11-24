//! A grant's decisions and the capability set built from it.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use barn::{Grant, PolicyError};
use nono::{AccessMode, CapabilitySet};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A scratch tree: a granted root, a sibling whose name is a string prefix of
/// it, an ungranted root, nested children and a file in each root.
struct Tree {
    dir: PathBuf,
    granted: PathBuf,
    sibling: PathBuf,
    other: PathBuf,
}

impl Tree {
    fn new() -> Result<Self, std::io::Error> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let dir = base.join(format!("barn-policy-{}-{n}", std::process::id()));

        let _ = fs::remove_dir_all(&dir);

        let granted = dir.join("granted");
        let sibling = dir.join("grantedx");
        let other = dir.join("other");

        for root in [&granted, &sibling, &other] {
            fs::create_dir_all(root.join("a/b/c"))?;
            fs::write(root.join("file.txt"), b"")?;
        }

        Ok(Self {
            dir,
            granted,
            sibling,
            other,
        })
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The error a grant build produces, failing if the grant builds cleanly.
fn build_error(grant: &Grant) -> Result<PolicyError, Box<dyn Error>> {
    grant
        .to_capability_set()
        .err()
        .ok_or_else(|| "expected a policy error".into())
}

/// The Display string the policy module promises for each error stage.
fn expected_display(err: &PolicyError) -> String {
    match err {
        PolicyError::RelativePath { path } => {
            let shown = path.display();
            format!("grant path is relative: {shown}")
        }
        PolicyError::ReadPath { path, source } => {
            let shown = path.display();
            format!("granting read on {shown}: {source}")
        }
        PolicyError::WritePath { path, source } => {
            let shown = path.display();
            format!("granting write on {shown}: {source}")
        }
        PolicyError::Executable { path, source } => {
            let shown = path.display();
            format!("granting access to the executable at {shown}: {source}")
        }
        _ => String::from("unexpected policy error"),
    }
}

#[test]
fn an_empty_grant_denies_every_path_and_host() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant::default();
    let caps = grant.to_capability_set()?;

    check_empty_grant(&grant, &caps, &tree);

    Ok(())
}

fn check_empty_grant(grant: &Grant, caps: &CapabilitySet, tree: &Tree) {
    let decisions = [
        grant.allows_read(&tree.granted),
        grant.allows_write(&tree.granted),
        grant.allows_host("example.com"),
    ];
    assert_eq!(decisions, [false, false, false]);
    assert_eq!(caps.fs_capabilities().len(), 0);
    assert!(caps.is_network_blocked());
    assert!(!caps.dns_enabled());
}

#[test]
fn only_granted_hosts_are_allowed() {
    let grant = Grant {
        network_hosts: vec!["example.com".to_owned(), "api.github.com".to_owned()],
        ..Grant::default()
    };

    let decisions = [
        grant.allows_host("example.com"),
        grant.allows_host("api.github.com"),
        grant.allows_host("ungranted.example"),
        grant.allows_host(""),
    ];

    assert_eq!(decisions, [true, true, false, false]);
}

#[test]
fn the_built_set_has_granted_modes_and_no_dns() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant {
        read_paths: vec![tree.granted.clone()],
        write_paths: vec![tree.other.clone()],
        network_hosts: vec!["example.com".to_owned()],
        executable: Some(tree.granted.join("file.txt")),
    };
    let caps = grant.to_capability_set()?;

    let read = tree.granted.canonicalize()?;
    let write = tree.other.canonicalize()?;
    let file = tree.granted.join("file.txt").canonicalize()?;
    check_built_set(&caps, &[read, write, file]);

    Ok(())
}

fn check_built_set(caps: &CapabilitySet, expected: &[PathBuf; 3]) {
    let modes: Vec<(AccessMode, bool)> = caps
        .fs_capabilities()
        .iter()
        .map(|cap| (cap.access, cap.is_file))
        .collect();

    assert_eq!(
        modes,
        [
            (AccessMode::Read, false),
            (AccessMode::Write, false),
            (AccessMode::Read, true),
        ]
    );

    let resolved: Vec<&Path> = caps
        .fs_capabilities()
        .iter()
        .map(|cap| cap.resolved.as_path())
        .collect();
    let expected: Vec<&Path> = expected.iter().map(PathBuf::as_path).collect();
    assert_eq!(resolved, expected);
    assert!(caps.is_network_blocked());
    assert!(!caps.dns_enabled());
}

#[test]
fn a_child_is_allowed_and_a_sibling_is_not() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant {
        read_paths: vec![tree.granted.clone()],
        ..Grant::default()
    };

    check_child_vs_sibling(&grant, &tree);

    Ok(())
}

fn check_child_vs_sibling(grant: &Grant, tree: &Tree) {
    let decisions = [
        grant.allows_read(&tree.granted),
        grant.allows_read(&tree.granted.join("a")),
        grant.allows_read(&tree.granted.join("a/b/c")),
        grant.allows_read(&tree.sibling),
        grant.allows_read(&tree.sibling.join("a/b/c")),
    ];
    assert_eq!(decisions, [true, true, true, false, false]);
}

#[test]
fn write_access_follows_the_write_grant() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant {
        read_paths: vec![tree.granted.clone()],
        write_paths: vec![tree.other.clone()],
        ..Grant::default()
    };

    check_mode_follows_grant(&grant, &tree);

    Ok(())
}

fn check_mode_follows_grant(grant: &Grant, tree: &Tree) {
    let decisions = [
        grant.allows_read(&tree.granted),
        grant.allows_write(&tree.granted),
        grant.allows_read(&tree.other),
        grant.allows_write(&tree.other),
    ];
    assert_eq!(decisions, [true, false, false, true]);
}

#[test]
fn trailing_slashes_and_escaping_parents() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant {
        read_paths: vec![tree.granted.clone()],
        ..Grant::default()
    };

    check_resolution(&grant, &tree);

    Ok(())
}

fn check_resolution(grant: &Grant, tree: &Tree) {
    let decisions = [
        grant.allows_read(&tree.granted.join("a/")),
        grant.allows_read(&tree.granted.join("a/./b")),
        grant.allows_read(&tree.granted.join("a/b/..")),
        grant.allows_read(&tree.granted.join("a/../../other")),
        grant.allows_read(&tree.sibling.join("a/../a")),
    ];
    assert_eq!(decisions, [true, true, true, false, false]);
}

#[test]
fn a_relative_path_is_denied() -> Result<(), Box<dyn Error>> {
    let relative = PathBuf::from("granted/a");
    let decisions = [
        Grant::default().allows_read(&relative),
        Grant::default().allows_write(&relative),
    ];
    let errors = [
        build_error(&Grant {
            read_paths: vec![relative.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            write_paths: vec![relative.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            executable: Some(relative),
            ..Grant::default()
        })?,
    ];

    check_relative(decisions, &errors);

    Ok(())
}

fn check_relative(decisions: [bool; 2], errors: &[PolicyError; 3]) {
    assert_eq!(decisions, [false, false]);

    for err in errors {
        assert!(matches!(err, PolicyError::RelativePath { .. }), "{err}");
    }
}

#[test]
fn a_missing_granted_path_reports_its_stage() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let missing = tree.dir.join("missing");
    let errors = [
        build_error(&Grant {
            read_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            write_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            executable: Some(missing.clone()),
            ..Grant::default()
        })?,
    ];

    check_missing(&errors, &missing);

    Ok(())
}

fn check_missing(errors: &[PolicyError; 3], p: &Path) {
    let read = errors
        .first()
        .map(|err| matches!(err, PolicyError::ReadPath { path, .. } if path == p));
    let write = errors
        .get(1)
        .map(|err| matches!(err, PolicyError::WritePath { path, .. } if path == p));
    let exec = errors
        .get(2)
        .map(|err| matches!(err, PolicyError::Executable { path, .. } if path == p));

    assert_eq!(read, Some(true));
    assert_eq!(write, Some(true));
    assert_eq!(exec, Some(true));
}

#[test]
fn decisions_match_the_built_set_for_many_paths() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let grant = Grant {
        read_paths: vec![tree.granted.clone()],
        write_paths: vec![tree.other.clone()],
        ..Grant::default()
    };
    let caps = grant.to_capability_set()?;

    let tails = [
        "",
        "a",
        "a/b",
        "a/b/c",
        "a/b/c/..",
        "a/b/c/../..",
        "file.txt",
        "a/../a",
    ];
    let roots = [&tree.granted, &tree.sibling, &tree.other];
    let mut cases: Vec<(PathBuf, PathBuf)> = Vec::new();

    for root in roots {
        for tail in tails {
            let path = root.join(tail);
            let resolved = path.canonicalize()?;
            cases.push((path, resolved));
        }
    }

    check_decisions_match(&grant, &caps, &cases);

    Ok(())
}

type Cases = [(PathBuf, PathBuf)];

fn check_decisions_match(grant: &Grant, caps: &CapabilitySet, cases: &Cases) {
    for (path, resolved) in cases {
        let shown = path.display();

        assert_eq!(
            grant.allows_read(path),
            caps.path_covered_with_access(resolved, AccessMode::Read),
            "{shown}"
        );
        assert_eq!(
            grant.allows_write(path),
            caps.path_covered_with_access(resolved, AccessMode::Write),
            "{shown}"
        );
    }
}

#[test]
fn every_policy_error_has_an_exact_display_string() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let missing = tree.dir.join("missing");
    let relative = PathBuf::from("granted/a");
    let errors = [
        build_error(&Grant {
            read_paths: vec![relative],
            ..Grant::default()
        })?,
        build_error(&Grant {
            read_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            write_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            executable: Some(missing),
            ..Grant::default()
        })?,
    ];

    check_display(&errors);

    Ok(())
}

fn check_display(errors: &[PolicyError; 4]) {
    let stages = [
        errors
            .first()
            .map(|err| matches!(err, PolicyError::RelativePath { .. })),
        errors
            .get(1)
            .map(|err| matches!(err, PolicyError::ReadPath { .. })),
        errors
            .get(2)
            .map(|err| matches!(err, PolicyError::WritePath { .. })),
        errors
            .get(3)
            .map(|err| matches!(err, PolicyError::Executable { .. })),
    ];
    assert_eq!(stages, [Some(true), Some(true), Some(true), Some(true)]);

    for err in errors {
        assert_eq!(err.to_string(), expected_display(err));
    }
}

#[test]
fn every_nono_backed_error_exposes_its_source() -> Result<(), Box<dyn Error>> {
    let tree = Tree::new()?;
    let missing = tree.dir.join("missing");
    let relative = PathBuf::from("granted/a");
    let errors = [
        build_error(&Grant {
            read_paths: vec![relative],
            ..Grant::default()
        })?,
        build_error(&Grant {
            read_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            write_paths: vec![missing.clone()],
            ..Grant::default()
        })?,
        build_error(&Grant {
            executable: Some(missing),
            ..Grant::default()
        })?,
    ];

    check_sources(&errors);

    Ok(())
}

fn check_sources(errors: &[PolicyError; 4]) {
    let sources: Vec<_> = errors.iter().map(|err| err.source().is_some()).collect();

    assert_eq!(sources, [false, true, true, true]);
}

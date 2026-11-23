//! A grant's decisions and the capability set built from it.

use std::path::{Path, PathBuf};

use barn::Grant;

/// A path that already exists: this crate's own directory.
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn an_empty_grant_denies_a_read_a_write_and_a_host() {
    let grant = Grant::default();
    let dir = crate_dir();

    let reads = [grant.allows_read(&dir)];
    let writes = [grant.allows_write(&dir)];
    let hosts = [grant.allows_host("example.com")];

    assert_eq!(reads, [false]);
    assert_eq!(writes, [false]);
    assert_eq!(hosts, [false]);
}

#[test]
fn only_named_paths_and_hosts_are_allowed() {
    let dir = crate_dir();
    let other = Path::new("/a/path/that/is/not/granted");
    let grant = Grant {
        read_paths: vec![dir.clone()],
        write_paths: vec![dir.clone()],
        network_hosts: vec!["example.com".to_owned()],
        ..Grant::default()
    };

    let reads = [grant.allows_read(&dir), grant.allows_read(other)];
    let writes = [grant.allows_write(&dir), grant.allows_write(other)];
    let hosts = [
        grant.allows_host("example.com"),
        grant.allows_host("ungranted.example"),
    ];

    assert_eq!(reads, [true, false]);
    assert_eq!(writes, [true, false]);
    assert_eq!(hosts, [true, false]);
}

#[test]
fn an_empty_grant_builds_a_capability_set() {
    let grant = Grant::default();

    assert!(grant.to_capability_set().is_ok());
}

#[test]
fn a_simple_grant_builds_a_capability_set() {
    let dir = crate_dir();
    let grant = Grant {
        read_paths: vec![dir.clone()],
        write_paths: vec![dir],
        network_hosts: vec!["example.com".to_owned()],
        ..Grant::default()
    };

    assert!(grant.to_capability_set().is_ok());
}

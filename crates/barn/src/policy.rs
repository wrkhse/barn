//! A stall's grant and the sandbox policy built from it.
//!
//! A grant is data: the paths a stall may touch, the host it may reach and the
//! binary it may run. barn turns the grant into a [nono] capability set at the
//! sandbox boundary, keeps the host list for the egress allow-list, and answers
//! access questions from that same capability set. Resolving a question touches
//! the filesystem, so a relative or missing path is denied.
//!
//! On macOS the capability set runs under Seatbelt. With network and DNS
//! blocked, a granted stall still has `mach-lookup`, `sysctl-read`, read access
//! to `/` itself, `process-fork`, and POSIX shared memory (`ipc-posix-shm-*`).
//! Secrets arrive at daemon start.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, NonoError};

/// What a stall may touch, held in memory. An empty grant grants no paths and
/// no hosts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grant {
    /// Paths the stall may read.
    pub read_paths: Vec<PathBuf>,
    /// Paths the stall may write.
    pub write_paths: Vec<PathBuf>,
    /// Hosts the stall may reach through barn's egress allow-list.
    pub network_hosts: Vec<String>,
    /// The binary the stall may execute.
    pub executable: Option<PathBuf>,
}

impl Grant {
    /// Whether `path` is covered by the capability set the sandbox gets.
    ///
    /// The path is resolved first: a relative or missing path is denied.
    #[must_use]
    pub fn allows(&self, path: &Path, mode: AccessMode) -> bool {
        let Ok(resolved) = path.canonicalize() else {
            return false;
        };
        let Ok(caps) = self.to_capability_set() else {
            return false;
        };

        caps.path_covered_with_access(&resolved, mode)
    }

    /// Whether `path` is granted read access.
    #[must_use]
    pub fn allows_read(&self, path: &Path) -> bool {
        self.allows(path, AccessMode::Read)
    }

    /// Whether `path` is granted write access.
    #[must_use]
    pub fn allows_write(&self, path: &Path) -> bool {
        self.allows(path, AccessMode::Write)
    }

    /// Whether `host` is a granted host, by exact equality.
    #[must_use]
    pub fn allows_host(&self, host: &str) -> bool {
        self.network_hosts.iter().any(|granted| granted == host)
    }

    /// Builds the sandbox capabilities the grant describes.
    ///
    /// Every path must be absolute, and nono canonicalizes granted paths: a
    /// missing path is an error, and directories go through nono's `allow_path`
    /// while the executable goes through `allow_file`. The set always blocks
    /// network access and the macOS DNS resolver: a named host stays grant data
    /// for the egress allow-list.
    ///
    /// # Errors
    ///
    /// Returns the stage and path when a grant is relative or nono refuses it.
    pub fn to_capability_set(&self) -> Result<CapabilitySet, PolicyError> {
        let mut caps = CapabilitySet::new();

        for path in &self.read_paths {
            require_absolute(path)?;

            caps = caps.allow_path(path, AccessMode::Read).map_err(|source| {
                PolicyError::ReadPath {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        for path in &self.write_paths {
            require_absolute(path)?;

            caps = caps.allow_path(path, AccessMode::Write).map_err(|source| {
                PolicyError::WritePath {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        // nono grants execute with read on both its macOS and Linux backends.
        if let Some(path) = &self.executable {
            require_absolute(path)?;

            caps = caps.allow_file(path, AccessMode::Read).map_err(|source| {
                PolicyError::Executable {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        Ok(caps.block_network().block_dns())
    }
}

/// A grant has to name absolute paths so the sandbox does not depend on the
/// working directory of the process that builds it.
fn require_absolute(path: &Path) -> Result<(), PolicyError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PolicyError::RelativePath {
            path: path.to_path_buf(),
        })
    }
}

/// The stage of a policy build that failed, and the path it failed on.
#[derive(Debug)]
#[non_exhaustive]
pub enum PolicyError {
    /// A grant named a relative path.
    RelativePath {
        /// The relative path that was rejected.
        path: PathBuf,
    },
    /// A read path was refused by nono.
    ReadPath {
        /// The path nono refused.
        path: PathBuf,
        /// The underlying nono error.
        source: NonoError,
    },
    /// A write path was refused by nono.
    WritePath {
        /// The path nono refused.
        path: PathBuf,
        /// The underlying nono error.
        source: NonoError,
    },
    /// The executable was refused by nono.
    Executable {
        /// The path nono refused.
        path: PathBuf,
        /// The underlying nono error.
        source: NonoError,
    },
}

impl Display for PolicyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelativePath { path } => {
                write!(f, "grant path is relative: {}", path.display())
            }
            Self::ReadPath { path, source } => {
                write!(f, "granting read on {}: {source}", path.display())
            }
            Self::WritePath { path, source } => {
                write!(f, "granting write on {}: {source}", path.display())
            }
            Self::Executable { path, source } => {
                write!(
                    f,
                    "granting access to the executable at {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for PolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadPath { source, .. }
            | Self::WritePath { source, .. }
            | Self::Executable { source, .. } => Some(source),
            Self::RelativePath { .. } => None,
        }
    }
}

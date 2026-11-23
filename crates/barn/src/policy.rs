//! A stall's grant and the sandbox policy built from it.
//!
//! A grant is data: the paths a stall may touch, the host it may reach and the
//! binary it may run. Nothing here reads a file or starts a process. barn turns
//! the grant into a [nono] capability set at the sandbox boundary and keeps the
//! host list for the egress allow-list.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use nono::{AccessMode, CapabilitySet, NonoError};

/// What a stall may touch, held in memory. An empty grant denies everything.
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
    /// Whether `path` is a granted read path, by exact equality.
    #[must_use]
    pub fn allows_read(&self, path: &Path) -> bool {
        self.read_paths.iter().any(|granted| granted == path)
    }

    /// Whether `path` is a granted write path, by exact equality.
    #[must_use]
    pub fn allows_write(&self, path: &Path) -> bool {
        self.write_paths.iter().any(|granted| granted == path)
    }

    /// Whether `host` is a granted host, by exact equality.
    #[must_use]
    pub fn allows_host(&self, host: &str) -> bool {
        self.network_hosts.iter().any(|granted| granted == host)
    }

    /// Builds the sandbox capabilities the grant describes.
    ///
    /// The capability set always blocks network access: a named host stays
    /// grant data for the egress allow-list.
    /// Granted paths must exist: nono canonicalizes them, and directories go
    /// through nono's `allow_path` while the executable goes through
    /// `allow_file`.
    ///
    /// # Errors
    ///
    /// Returns the stage and path when nono refuses one: the path is missing,
    /// or it is not the kind of path that stage names.
    pub fn to_capability_set(&self) -> Result<CapabilitySet, PolicyError> {
        let mut caps = CapabilitySet::new();

        for path in &self.read_paths {
            caps = caps.allow_path(path, AccessMode::Read).map_err(|source| {
                PolicyError::ReadPath {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        for path in &self.write_paths {
            caps = caps.allow_path(path, AccessMode::Write).map_err(|source| {
                PolicyError::WritePath {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        // nono grants execute with read on both its macOS and Linux backends.
        if let Some(path) = &self.executable {
            caps = caps.allow_file(path, AccessMode::Read).map_err(|source| {
                PolicyError::Executable {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        Ok(caps.block_network())
    }
}

/// The stage of a policy build that failed, and the path it failed on.
#[derive(Debug)]
pub enum PolicyError {
    /// A read path was refused.
    ReadPath { path: PathBuf, source: NonoError },
    /// A write path was refused.
    WritePath { path: PathBuf, source: NonoError },
    /// The executable was refused.
    Executable { path: PathBuf, source: NonoError },
}

impl Display for PolicyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
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
        }
    }
}

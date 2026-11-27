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
    /// On Linux, an ELF64 executable's `PT_INTERP` names a dynamic loader. The
    /// loader must resolve to a strict descendant of a system library root,
    /// sit in a Debian-style multiarch directory and be a regular file; it is
    /// granted read and execute. Only existing multiarch directories for the
    /// architecture are granted read; a broad root itself is never granted. No
    /// home folder or `/` is opened up. A loader outside those roots, in
    /// another layout (for example `/lib64` or `/usr/lib64`), or an ELF64 file
    /// that cannot be parsed, is an error. macOS needs no extra grants: its
    /// Seatbelt profile already allows reading `/` and executing readable
    /// paths.
    ///
    /// # Errors
    ///
    /// Returns the stage and path when a grant is relative or nono refuses it,
    /// and the ELF path when the loader cannot be found or is refused.
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

            #[cfg(target_os = "linux")]
            {
                caps = loader::grant_paths(caps, path)?;
            }
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

/// The ELF64 dynamic loader of a Linux stall: parsed from the executable's
/// bytes, without running it or asking an external tool.
#[cfg(target_os = "linux")]
mod loader {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Component, Path, PathBuf};

    use nono::{AccessMode, CapabilitySet};

    use super::PolicyError;

    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const ELFDATA2MSB: u8 = 2;
    const PT_INTERP: u32 = 3;
    const ELF_HEADER_LEN: usize = 64;
    const PROGRAM_HEADER_LEN: usize = 56;
    const MAX_PROGRAM_HEADERS: usize = 64;
    const MAX_INTERP_LEN: usize = 4096;

    /// The loader a `PT_INTERP` names, and the machine whose libraries it uses.
    #[derive(Debug, Eq, PartialEq)]
    struct Interpreter {
        path: PathBuf,
        arch: Architecture,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Architecture {
        X86_64,
        Aarch64,
        Other,
    }

    /// Grants the loader and the directories it searches for libraries.
    ///
    /// A non-ELF executable has no interpreter and adds no paths. An unusable
    /// ELF file, a loader outside the system library roots, or a loader in a
    /// non-multiarch layout is an error. Only existing Debian-style multiarch
    /// directories are added; a broad root, a home folder or `/` is never
    /// opened up.
    pub(super) fn grant_paths(
        caps: CapabilitySet,
        executable: &Path,
    ) -> Result<CapabilitySet, PolicyError> {
        let Some(interp) = read_interpreter(executable)? else {
            return Ok(caps);
        };

        require_system_loader(&interp.path)?;
        require_multiarch_layout(&interp.path, interp.arch)?;
        require_regular_file(&interp.path)?;

        let caps = caps
            .allow_file(&interp.path, AccessMode::Read)
            .map_err(|source| PolicyError::Executable {
                path: interp.path.clone(),
                source,
            })?;

        let mut caps = caps;
        for dir in loader_dirs(&interp) {
            caps = caps
                .allow_path(&dir, AccessMode::Read)
                .map_err(|source| PolicyError::ReadPath { path: dir, source })?;
        }

        Ok(caps)
    }

    /// Whether `path` is a strict descendant of `/lib`, `/lib64`,
    /// `/usr/lib` or `/usr/lib64`. A root itself, a path under a home
    /// folder and a path climbing out of a root are below no library root.
    fn is_below_library_root(path: &Path) -> bool {
        if path.components().any(|part| part == Component::ParentDir) {
            return false;
        }

        let mut parts = path.components();
        if !matches!(parts.next(), Some(Component::RootDir)) {
            return false;
        }

        let under_root = match parts.next().and_then(|part| part.as_os_str().to_str()) {
            Some("lib" | "lib64") => true,
            Some("usr") => matches!(
                parts.next().and_then(|part| part.as_os_str().to_str()),
                Some("lib" | "lib64")
            ),
            _ => false,
        };

        under_root && parts.next().is_some()
    }

    /// The loader must be a strict descendant of `/lib`, `/lib64`,
    /// `/usr/lib` or `/usr/lib64`; a root itself is not a loader. A literal
    /// path under a root can still be a symlink into a home folder, so what it
    /// resolves to must be below a library root too. A loader under a home
    /// folder or a broad root such as `/usr` is refused before any path is
    /// granted.
    fn require_system_loader(path: &Path) -> Result<(), PolicyError> {
        if !is_below_library_root(path) {
            return Err(PolicyError::InterpreterOutsideLibraryRoot {
                path: path.to_path_buf(),
            });
        }

        require_resolved_within_root(path, path.canonicalize())
    }

    /// Checks the resolved form of a loader whose literal path is below a
    /// library root. An existing path that cannot be resolved is refused:
    /// its literal path alone cannot show that the loader stays within the
    /// root. A missing path is left to the regular-file check that follows.
    fn require_resolved_within_root(
        path: &Path,
        resolved: std::io::Result<PathBuf>,
    ) -> Result<(), PolicyError> {
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(source) if path.metadata().is_ok() => {
                return Err(PolicyError::InterpreterIo {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Err(_) => return Ok(()),
        };

        if !is_below_library_root(&resolved) {
            return Err(PolicyError::InterpreterOutsideLibraryRoot {
                path: path.to_path_buf(),
            });
        }

        Ok(())
    }

    /// A loader is supported only in a Debian-style multiarch directory for
    /// its machine. Fedora, RHEL and Arch keep loaders in `/lib64` or
    /// `/usr/lib64`, and Alpine in `/lib`; those hosts need a static stall.
    ///
    /// An existing loader is checked by its resolved parent: Ubuntu's
    /// `/lib64/ld-linux-x86-64.so.2` is a symlink into a multiarch directory.
    /// A missing loader falls back to its literal parent, so a real `/lib64`
    /// or `/lib` loader stays refused by name.
    fn require_multiarch_layout(path: &Path, arch: Architecture) -> Result<(), PolicyError> {
        let resolved = path.canonicalize().ok();
        require_multiarch_layout_of(path, resolved.as_deref(), arch)
    }

    /// Checks the layout of the resolved loader for an existing path, or the
    /// literal path otherwise. Taking the resolver's result separately lets a
    /// symlinked loader be checked without writing into a system library root.
    fn require_multiarch_layout_of(
        path: &Path,
        resolved: Option<&Path>,
        arch: Architecture,
    ) -> Result<(), PolicyError> {
        let target = resolved.unwrap_or(path);
        let dir = target.parent().unwrap_or(target);
        let supported = arch_lib_dirs(arch)
            .iter()
            .any(|candidate| Path::new(candidate) == dir);

        if supported {
            Ok(())
        } else {
            Err(PolicyError::UnsupportedLibraryLayout {
                dir: dir.to_path_buf(),
            })
        }
    }

    /// Reads `PT_INTERP` from an ELF64 file. `Ok(None)` means the file is
    /// not ELF, is not ELF64, or is an ELF with no interpreter. A file that
    /// claims to be ELF64 but cannot be used is an error, never a silent
    /// `None`.
    fn read_interpreter(path: &Path) -> Result<Option<Interpreter>, PolicyError> {
        let mut file = open_elf(path)?;

        let Some(header) = read_elf_header(&mut file, path)? else {
            return Ok(None);
        };

        scan_program_headers(&mut file, path, &header)
    }

    /// Opens an ELF candidate, refusing anything that is not a regular file.
    fn open_elf(path: &Path) -> Result<File, PolicyError> {
        // Opening a FIFO for reading blocks until a writer arrives, so check
        // the file type before opening it. Only a regular file can be ELF.
        require_regular_file(path)?;

        File::open(path).map_err(|source| PolicyError::InterpreterIo {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Refuses a path that is not a regular file, and reports why its
    /// metadata could not be read.
    fn require_regular_file(path: &Path) -> Result<(), PolicyError> {
        match path.metadata() {
            Ok(meta) if meta.is_file() => Ok(()),
            Ok(_) => Err(PolicyError::InterpreterNotAFile {
                path: path.to_path_buf(),
            }),
            Err(source) => Err(PolicyError::InterpreterIo {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// The fields the parser reads from a validated ELF64 header.
    struct ElfHeader {
        machine: u16,
        phoff: u64,
        phentsize: usize,
        phnum: usize,
        little: bool,
    }

    /// Reads and validates an ELF64 header. `Ok(None)` means the file is not
    /// ELF, is not ELF64, or declares no program headers.
    fn read_elf_header(file: &mut File, path: &Path) -> Result<Option<ElfHeader>, PolicyError> {
        let mut magic = [0_u8; 4];
        if file.read_exact(&mut magic).is_err() {
            return Ok(None);
        }
        if magic != ELF_MAGIC {
            return Ok(None);
        }

        let mut header = [0_u8; ELF_HEADER_LEN];
        header[..4].copy_from_slice(&magic);
        file.read_exact(&mut header[4..])
            .map_err(|source| PolicyError::InterpreterIo {
                path: path.to_path_buf(),
                source,
            })?;

        if header[4] != ELFCLASS64 {
            return Ok(None);
        }
        let little = match header[5] {
            ELFDATA2LSB => true,
            ELFDATA2MSB => false,
            _ => return Err(malformed(path, "unknown ELF data encoding")),
        };

        let Some(machine) = u16_at(&header, 18, little) else {
            return Err(malformed(path, "truncated ELF header"));
        };
        let Some(phoff) = u64_at(&header, 32, little) else {
            return Err(malformed(path, "truncated ELF header"));
        };
        let Some(phentsize) = u16_at(&header, 54, little) else {
            return Err(malformed(path, "truncated ELF header"));
        };
        let phentsize = usize::from(phentsize);
        let Some(phnum) = u16_at(&header, 56, little) else {
            return Err(malformed(path, "truncated ELF header"));
        };
        let phnum = usize::from(phnum);
        if phnum == 0 {
            return Ok(None);
        }
        if phentsize < PROGRAM_HEADER_LEN {
            return Err(malformed(path, "program header is too small"));
        }
        if phnum > MAX_PROGRAM_HEADERS {
            return Err(PolicyError::TooManyProgramHeaders {
                path: path.to_path_buf(),
                count: phnum,
            });
        }

        Ok(Some(ElfHeader {
            machine,
            phoff,
            phentsize,
            phnum,
            little,
        }))
    }

    /// Scans the program headers for `PT_INTERP` and reads its path.
    fn scan_program_headers(
        file: &mut File,
        path: &Path,
        header: &ElfHeader,
    ) -> Result<Option<Interpreter>, PolicyError> {
        for index in 0..header.phnum {
            let size = index
                .checked_mul(header.phentsize)
                .ok_or_else(|| malformed(path, "program header offset overflows"))?;
            let at = u64::try_from(size)
                .ok()
                .and_then(|offset| offset.checked_add(header.phoff))
                .ok_or_else(|| malformed(path, "program header offset overflows"))?;
            file.seek(SeekFrom::Start(at))
                .map_err(|source| PolicyError::InterpreterIo {
                    path: path.to_path_buf(),
                    source,
                })?;

            let mut phdr = [0_u8; PROGRAM_HEADER_LEN];
            file.read_exact(&mut phdr)
                .map_err(|source| PolicyError::InterpreterIo {
                    path: path.to_path_buf(),
                    source,
                })?;
            let Some(kind) = u32_at(&phdr, 0, header.little) else {
                return Err(malformed(path, "truncated program header"));
            };
            if kind != PT_INTERP {
                continue;
            }

            let Some(offset) = u64_at(&phdr, 8, header.little) else {
                return Err(malformed(path, "truncated program header"));
            };
            let Some(length) = u64_at(&phdr, 32, header.little) else {
                return Err(malformed(path, "truncated program header"));
            };
            let len = usize::try_from(length)
                .map_err(|_| malformed(path, "PT_INTERP length overflows"))?;
            return interp_path(file, offset, len, header.machine, path).map(Some);
        }

        Ok(None)
    }

    /// Reads the NUL-terminated, absolute path from a `PT_INTERP` segment.
    fn interp_path(
        file: &mut File,
        offset: u64,
        len: usize,
        machine: u16,
        path: &Path,
    ) -> Result<Interpreter, PolicyError> {
        if len > MAX_INTERP_LEN {
            return Err(PolicyError::InterpreterTooLong {
                path: path.to_path_buf(),
                len,
            });
        }
        if len == 0 {
            return Err(malformed(path, "empty PT_INTERP"));
        }

        file.seek(SeekFrom::Start(offset))
            .map_err(|source| PolicyError::InterpreterIo {
                path: path.to_path_buf(),
                source,
            })?;
        let mut bytes = vec![0_u8; len];
        file.read_exact(&mut bytes)
            .map_err(|source| PolicyError::InterpreterIo {
                path: path.to_path_buf(),
                source,
            })?;

        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| malformed(path, "unterminated PT_INTERP"))?;
        if end == 0 {
            return Err(malformed(path, "empty PT_INTERP"));
        }
        let Some(name) = bytes.get(..end) else {
            return Err(malformed(path, "PT_INTERP ends past the segment"));
        };
        let text =
            std::str::from_utf8(name).map_err(|_| malformed(path, "PT_INTERP is not UTF-8"))?;
        let interp = Path::new(text);
        if !interp.is_absolute() {
            return Err(malformed(path, "PT_INTERP is not absolute"));
        }

        let arch = match machine {
            62 => Architecture::X86_64,
            183 => Architecture::Aarch64,
            _ => Architecture::Other,
        };

        Ok(Interpreter {
            path: interp.to_path_buf(),
            arch,
        })
    }

    fn malformed(path: &Path, reason: &'static str) -> PolicyError {
        PolicyError::MalformedElf {
            path: path.to_path_buf(),
            reason,
        }
    }

    fn u16_at(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
        let raw: [u8; 2] = bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(raw)
        } else {
            u16::from_be_bytes(raw)
        })
    }

    fn u32_at(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
        let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    }

    fn u64_at(bytes: &[u8], at: usize, little: bool) -> Option<u64> {
        let raw: [u8; 8] = bytes.get(at..at.checked_add(8)?)?.try_into().ok()?;
        Some(if little {
            u64::from_le_bytes(raw)
        } else {
            u64::from_be_bytes(raw)
        })
    }

    /// The existing Debian-style multiarch directories for the architecture,
    /// each once. A broad root is never granted: on a merged-/usr host
    /// `/lib` resolves to `/usr/lib`, and granting that path would open the
    /// whole library root.
    fn loader_dirs(interp: &Interpreter) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for candidate in arch_lib_dirs(interp.arch) {
            let path = Path::new(candidate);
            if path.is_dir()
                && let Ok(resolved) = path.canonicalize()
                && is_below_library_root(&resolved)
                && !dirs.contains(&resolved)
            {
                dirs.push(resolved);
            }
        }

        dirs
    }

    /// Debian-style multiarch directories, the only supported library layout.
    /// NixOS keeps its libraries in a store with no fixed path, and the
    /// `/lib64`, `/usr/lib64` and `/lib` layouts need a static stall, so no
    /// guessed directory is granted for them.
    const fn arch_lib_dirs(arch: Architecture) -> &'static [&'static str] {
        match arch {
            Architecture::X86_64 => &["/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu"],
            Architecture::Aarch64 => &["/usr/lib/aarch64-linux-gnu", "/lib/aarch64-linux-gnu"],
            Architecture::Other => &[],
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::Grant;

        use super::*;

        static NEXT: AtomicUsize = AtomicUsize::new(0);

        /// A scratch directory for an executable and its interpreter.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new() -> Result<Self, std::io::Error> {
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let dir =
                    std::env::temp_dir().join(format!("barn-loader-{}-{n}", std::process::id()));
                fs::create_dir_all(&dir)?;

                Ok(Self(dir))
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn word16(value: u16, little: bool) -> [u8; 2] {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        }

        fn word32(value: u32, little: bool) -> [u8; 4] {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        }

        fn word64(value: u64, little: bool) -> [u8; 8] {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        }

        /// An ELF64 file with one program header, a `PT_INTERP` naming `interp`.
        fn elf64(interp: &Path, machine: u16, little: bool) -> Vec<u8> {
            let len = interp
                .to_str()
                .expect("UTF-8 temp path")
                .len()
                .saturating_add(1);

            elf64_with(interp, machine, little, 1, len)
        }

        /// An ELF64 file with `phnum` program headers. Header zero is a
        /// `PT_INTERP` whose `interp_len`-byte segment holds `interp` followed
        /// by zero padding.
        fn elf64_with(
            interp: &Path,
            machine: u16,
            little: bool,
            phnum: u16,
            interp_len: usize,
        ) -> Vec<u8> {
            let name = interp.to_str().expect("UTF-8 temp path");
            let at = ELF_HEADER_LEN;
            let interp_at = ELF_HEADER_LEN
                .saturating_add(usize::from(phnum).saturating_mul(PROGRAM_HEADER_LEN));
            let span = interp_len.max(name.len().saturating_add(1));
            let mut bytes = vec![0_u8; interp_at.saturating_add(span)];

            bytes[..4].copy_from_slice(&ELF_MAGIC);
            bytes[4] = ELFCLASS64;
            bytes[5] = if little { ELFDATA2LSB } else { ELFDATA2MSB };
            bytes[18..20].copy_from_slice(&word16(machine, little));
            bytes[32..40].copy_from_slice(&word64(
                u64::try_from(ELF_HEADER_LEN).expect("header size fits"),
                little,
            ));
            bytes[52..54].copy_from_slice(&word16(
                u16::try_from(ELF_HEADER_LEN).expect("header size fits"),
                little,
            ));
            bytes[54..56].copy_from_slice(&word16(
                u16::try_from(PROGRAM_HEADER_LEN).expect("phdr size fits"),
                little,
            ));
            bytes[56..58].copy_from_slice(&word16(phnum, little));
            bytes[at..at.saturating_add(4)].copy_from_slice(&word32(PT_INTERP, little));
            bytes[at.saturating_add(8)..at.saturating_add(16)].copy_from_slice(&word64(
                u64::try_from(interp_at).expect("interp offset fits"),
                little,
            ));
            let filesz = u64::try_from(interp_len).expect("path length fits");
            bytes[at.saturating_add(32)..at.saturating_add(40)]
                .copy_from_slice(&word64(filesz, little));
            let end = interp_at.saturating_add(name.len());
            bytes[interp_at..end].copy_from_slice(name.as_bytes());
            bytes[end] = 0;

            bytes
        }

        #[test]
        fn an_elf64_pt_interp_names_its_interpreter() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld-linux.so.2");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(&interp, 62, true)).expect("write ELF");

            let found = read_interpreter(&exe).expect("read ELF");

            assert_eq!(
                found.as_ref().map(|it| it.path.as_path()),
                Some(interp.as_path())
            );
            assert_eq!(found.map(|it| it.arch), Some(Architecture::X86_64));
        }

        #[test]
        fn a_big_endian_elf64_pt_interp_names_its_interpreter() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld-linux-aarch64.so.1");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(&interp, 183, false)).expect("write ELF");

            let found = read_interpreter(&exe).expect("read ELF");

            assert_eq!(
                found.as_ref().map(|it| it.path.as_path()),
                Some(interp.as_path())
            );
            assert_eq!(found.map(|it| it.arch), Some(Architecture::Aarch64));
        }

        #[test]
        fn an_unknown_machine_grants_no_directory_of_its_own() {
            let interp = Interpreter {
                path: PathBuf::from("/lib/ld.so"),
                arch: Architecture::Other,
            };

            assert_eq!(loader_dirs(&interp), Vec::<PathBuf>::new());
        }

        #[test]
        fn a_non_elf_file_has_no_interpreter() {
            let scratch = Scratch::new().expect("scratch");
            let cases: [&[u8]; 3] = [
                &[],
                &[0xfe, 0xed, 0xfa, 0xce, 0x0c, 0, 0, 0],
                b"not an elf at all",
            ];

            for (n, bytes) in cases.into_iter().enumerate() {
                let exe = scratch.0.join(format!("exe-{n}"));
                fs::write(&exe, bytes).expect("write case");

                assert_eq!(read_interpreter(&exe).expect("read case"), None, "case {n}");
            }
        }

        #[test]
        fn an_elf_without_a_loader_has_no_interpreter() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let full = elf64(&interp, 62, true);

            let mut no_segment = full.clone();
            no_segment[64..68].copy_from_slice(&word32(1, true));
            let mut no_headers = full;
            no_headers[56..58].copy_from_slice(&word16(0, true));

            for (n, bytes) in [no_segment, no_headers].into_iter().enumerate() {
                let exe = scratch.0.join(format!("static-{n}"));
                fs::write(&exe, bytes).expect("write case");

                assert_eq!(read_interpreter(&exe).expect("read case"), None, "case {n}");
            }
        }

        #[test]
        fn an_elf32_file_has_no_loader_grant() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let mut bytes = elf64(&interp, 62, true);
            bytes[4] = 1;
            let exe = scratch.0.join("elf32");
            fs::write(&exe, bytes).expect("write ELF32");

            assert_eq!(read_interpreter(&exe).expect("read ELF32"), None);

            let grant = Grant {
                executable: Some(exe.clone()),
                ..Grant::default()
            };
            let caps = grant.to_capability_set().expect("grant");

            check_only_executable(&caps, &exe);
        }

        #[test]
        fn too_many_program_headers_names_their_error() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let mut bytes = elf64(&interp, 62, true);
            bytes[56..58].copy_from_slice(&word16(65, true));
            let exe = scratch.0.join("many-headers");
            fs::write(&exe, bytes).expect("write ELF");

            let error = read_interpreter(&exe).expect_err("64 is the limit");

            match error {
                PolicyError::TooManyProgramHeaders { path, count } => {
                    assert_eq!(path, exe);
                    assert_eq!(count, 65);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_too_long_pt_interp_names_its_error() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let mut bytes = elf64(&interp, 62, true);
            bytes[96..104].copy_from_slice(&word64(4097, true));
            let exe = scratch.0.join("long-interp");
            fs::write(&exe, bytes).expect("write ELF");

            let error = read_interpreter(&exe).expect_err("4096 is the limit");

            match error {
                PolicyError::InterpreterTooLong { path, len } => {
                    assert_eq!(path, exe);
                    assert_eq!(len, 4097);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn exactly_64_program_headers_are_accepted() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let exe = scratch.0.join("stall");
            let len = interp
                .to_str()
                .expect("UTF-8 temp path")
                .len()
                .saturating_add(1);
            fs::write(&exe, elf64_with(&interp, 62, true, 64, len)).expect("write ELF");

            let found = read_interpreter(&exe).expect("64 headers is the limit");

            assert_eq!(found.map(|it| it.path), Some(interp));
        }

        #[test]
        fn an_exactly_4096_byte_pt_interp_is_accepted() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64_with(&interp, 62, true, 1, 4096)).expect("write ELF");

            let found = read_interpreter(&exe).expect("4096 bytes is the limit");

            assert_eq!(found.map(|it| it.path), Some(interp));
        }

        #[test]
        fn a_malformed_elf_names_its_error() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("ld.so");
            let full = elf64(&interp, 62, true);

            let mut bad_encoding = full.clone();
            bad_encoding[5] = 9;
            let mut unterminated = full.clone();
            let last = unterminated.len().saturating_sub(1);
            unterminated[last] = b'x';
            let mut small_phdr = full.clone();
            small_phdr[54..56].copy_from_slice(&word16(48, true));
            let mut empty = full.clone();
            empty[96..104].copy_from_slice(&word64(0, true));
            let mut leading_nul = full;
            leading_nul[120] = 0;
            let relative = elf64(Path::new("ld.so"), 62, true);
            let mut bad_utf8 = elf64(Path::new("/ld.so"), 62, true);
            bad_utf8[120] = 0xff;

            let cases: [(&[u8], &str); 7] = [
                (&bad_encoding, "unknown ELF data encoding"),
                (&unterminated, "unterminated PT_INTERP"),
                (&small_phdr, "program header is too small"),
                (&empty, "empty PT_INTERP"),
                (&leading_nul, "empty PT_INTERP"),
                (&relative, "PT_INTERP is not absolute"),
                (&bad_utf8, "PT_INTERP is not UTF-8"),
            ];

            for (n, (bytes, reason)) in cases.into_iter().enumerate() {
                let exe = scratch.0.join(format!("bad-{n}"));
                fs::write(&exe, bytes).expect("write case");

                let error = read_interpreter(&exe).expect_err("malformed ELF");

                match error {
                    PolicyError::MalformedElf {
                        path,
                        reason: found,
                    } => {
                        assert_eq!(path, exe);
                        assert_eq!(found, reason);
                    }
                    other => panic!("case {n}: {other:?}"),
                }
            }
        }

        #[test]
        fn an_unopenable_or_non_regular_file_names_its_error() {
            use std::io::ErrorKind;

            let scratch = Scratch::new().expect("scratch");
            let missing = scratch.0.join("absent");

            let error = read_interpreter(&missing).expect_err("open fails");

            match error {
                PolicyError::InterpreterIo { path, source } => {
                    assert_eq!(path, missing);
                    assert_eq!(source.kind(), ErrorKind::NotFound);
                }
                other => panic!("unexpected error: {other:?}"),
            }

            let error = read_interpreter(&scratch.0).expect_err("a directory is not ELF");

            match error {
                PolicyError::InterpreterNotAFile { path } => {
                    assert_eq!(path.as_path(), scratch.0.as_path());
                }
                other => panic!("unexpected error: {other:?}"),
            }

            let magic_only = scratch.0.join("magic-only");
            fs::write(&magic_only, ELF_MAGIC).expect("write magic");

            let error = read_interpreter(&magic_only).expect_err("a truncated header is unusable");

            match error {
                PolicyError::InterpreterIo { path, .. } => {
                    assert_eq!(path, magic_only);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_pt_interp_naming_a_directory_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let interp = scratch.0.join("not-a-loader");
            fs::create_dir_all(&interp).expect("create directory");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(&interp, 62, true)).expect("write ELF");

            let found = read_interpreter(&exe)
                .expect("read ELF")
                .expect("the ELF names a loader");
            assert_eq!(found.path, interp);
            let error = require_regular_file(&found.path).expect_err("a directory is not a file");

            match error {
                PolicyError::InterpreterNotAFile { path } => assert_eq!(path, interp),
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_non_elf_executable_adds_no_loader_capabilities() {
            let scratch = Scratch::new().expect("scratch");
            let exe = scratch.0.join("mach-o");
            fs::write(&exe, [0xfe, 0xed, 0xfa, 0xce, 0x0c, 0, 0, 0]).expect("write Mach-O");
            let grant = Grant {
                executable: Some(exe.clone()),
                ..Grant::default()
            };

            let caps = grant.to_capability_set().expect("grant");

            check_only_executable(&caps, &exe);
        }

        fn check_only_executable(caps: &CapabilitySet, exe: &Path) {
            let resolved = exe.canonicalize().expect("resolve executable");
            let listed = caps.fs_capabilities();

            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].resolved, resolved);
            assert_eq!(listed[0].access, AccessMode::Read);
            assert!(listed[0].is_file);
        }

        #[test]
        fn each_system_library_root_is_refused() {
            for root in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
                match require_system_loader(Path::new(root)) {
                    Err(PolicyError::InterpreterOutsideLibraryRoot { path }) => {
                        assert_eq!(path.as_path(), Path::new(root));
                    }
                    other => panic!("{root}: unexpected result: {other:?}"),
                }
            }
        }

        #[test]
        fn a_strict_descendant_of_each_library_root_is_accepted() {
            let accepted = [
                "/lib/ld.so",
                "/lib64/ld.so",
                "/usr/lib/ld.so",
                "/usr/lib64/ld.so",
                "/lib/x86_64-linux-gnu/ld.so",
            ];

            for path in accepted {
                assert!(require_system_loader(Path::new(path)).is_ok(), "{path}");
            }
        }

        #[test]
        fn a_broad_library_root_is_below_no_library_root() {
            for root in ["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/usr"] {
                assert!(!is_below_library_root(Path::new(root)), "{root}");
            }
        }

        #[test]
        fn a_symlink_target_outside_the_roots_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let target = scratch.0.join("outside.so");
            fs::write(&target, b"loader").expect("write target");
            let link = scratch.0.join("inside.so");
            std::os::unix::fs::symlink(&target, &link).expect("symlink");

            // A symlink at a literal path under /lib or /usr/lib needs system
            // writes, so pair a synthetic under-root interpreter path with
            // the temp symlink's real target, then exercise the full loader
            // path with an ELF naming the temp symlink itself.
            let resolved = link.canonicalize().expect("resolve symlink");
            assert_eq!(resolved, target);
            assert!(!is_below_library_root(&resolved));

            let literal = Path::new("/lib/x86_64-linux-gnu/ld.so");
            let error = require_resolved_within_root(literal, Ok(resolved))
                .expect_err("a target outside the roots is refused");

            match error {
                PolicyError::InterpreterOutsideLibraryRoot { path } => {
                    assert_eq!(path.as_path(), literal);
                }
                other => panic!("unexpected error: {other:?}"),
            }

            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(&link, 62, true)).expect("write ELF");

            let error = grant_paths(CapabilitySet::new(), &exe)
                .expect_err("a loader outside the roots is refused");

            match error {
                PolicyError::InterpreterOutsideLibraryRoot { path } => {
                    assert_eq!(path, link);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn an_existing_loader_that_cannot_be_resolved_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let loader = scratch.0.join("ld.so");
            fs::write(&loader, b"loader").expect("write loader");
            let source = std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot resolve the loader",
            );

            // An existing path whose resolution fails cannot be shown to stay
            // within a root, so the check takes the resolver's result directly
            // instead of relying on a hard-to-create host state.
            let error = require_resolved_within_root(&loader, Err(source))
                .expect_err("an unresolvable existing loader is refused");

            match error {
                PolicyError::InterpreterIo { path, source } => {
                    assert_eq!(path, loader);
                    assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_merged_usr_layout_does_not_grant_the_library_root() {
            let interp = Interpreter {
                path: PathBuf::from("/lib/ld-linux-x86-64.so.2"),
                arch: Architecture::X86_64,
            };

            let dirs = loader_dirs(&interp);

            assert!(!dirs.contains(&PathBuf::from("/usr/lib")));
            if let Ok(resolved) = Path::new("/lib").canonicalize() {
                assert!(
                    !dirs.contains(&resolved),
                    "{} was granted",
                    resolved.display()
                );
            }
            for dir in &dirs {
                assert!(
                    is_below_library_root(dir),
                    "broad root granted: {}",
                    dir.display()
                );
            }
        }

        #[test]
        fn a_loader_under_usr_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let interp = Path::new("/usr/ld-linux.so.2");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(interp, 62, true)).expect("write ELF");

            let error = grant_paths(CapabilitySet::new(), &exe)
                .expect_err("a loader under /usr is outside the library roots");

            match error {
                PolicyError::InterpreterOutsideLibraryRoot { path } => {
                    assert_eq!(path.as_path(), interp);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_symlinked_lib64_loader_resolving_into_multiarch_is_accepted() {
            // Ubuntu's /lib64/ld-linux-x86-64.so.2 resolves into a Debian
            // multiarch directory, so its literal parent is not the layout.
            let literal = Path::new("/lib64/ld-linux-x86-64.so.2");
            let resolved = Path::new("/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2");

            let check = require_multiarch_layout_of(literal, Some(resolved), Architecture::X86_64);

            assert!(check.is_ok());
        }

        #[test]
        fn a_non_multiarch_loader_layout_is_refused_by_name() {
            let scratch = Scratch::new().expect("scratch");
            // A literal that does not exist: a real /usr/lib64 loader may be a
            // symlink into a multiarch directory on this host.
            let interp = Path::new("/usr/lib64/ld-barn-nonexistent.so");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(interp, 62, true)).expect("write ELF");

            let error = grant_paths(CapabilitySet::new(), &exe)
                .expect_err("a /usr/lib64 loader is an unsupported layout");
            let message = error.to_string();

            assert_eq!(
                message,
                "unsupported library layout /usr/lib64: barn-luau-stall must be statically linked on this host"
            );
            match error {
                PolicyError::UnsupportedLibraryLayout { dir } => {
                    assert_eq!(dir.as_path(), Path::new("/usr/lib64"));
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_home_folder_interpreter_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let interp = Path::new("/home/stall/.local/lib/ld-linux.so.2");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(interp, 62, true)).expect("write ELF");

            let error = grant_paths(CapabilitySet::new(), &exe)
                .expect_err("a home folder is outside the library roots");

            match error {
                PolicyError::InterpreterOutsideLibraryRoot { path } => {
                    assert_eq!(path.as_path(), interp);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        #[test]
        fn a_loader_that_climbs_out_of_a_library_root_is_refused() {
            let scratch = Scratch::new().expect("scratch");
            let interp = Path::new("/lib/../home/stall/ld.so");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(interp, 62, true)).expect("write ELF");

            let error = grant_paths(CapabilitySet::new(), &exe)
                .expect_err("a parent component escapes the library root");

            match error {
                PolicyError::InterpreterOutsideLibraryRoot { path } => {
                    assert_eq!(path.as_path(), interp);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }

        /// An existing Debian multiarch directory and a regular file in it.
        /// The ELF's interpreter is synthetic, but nono grants only real
        /// paths, so the name must resolve to a file on this host.
        fn multiarch_interpreter() -> (PathBuf, PathBuf) {
            for name in ["/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu"] {
                let Ok(root) = Path::new(name).canonicalize() else {
                    continue;
                };
                let Ok(entries) = fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let interp = entry.path();
                    let Ok(loader) = interp.canonicalize() else {
                        continue;
                    };
                    if interp.to_str().is_some()
                        && loader.is_file()
                        && loader.parent() == Some(root.as_path())
                    {
                        return (interp, loader);
                    }
                }
            }
            panic!("this host has no Debian multiarch directory with a regular file");
        }

        #[test]
        fn a_multiarch_loader_gets_only_its_file_and_multiarch_dirs() {
            let (interp, loader) = multiarch_interpreter();
            let interp = interp.as_path();
            let scratch = Scratch::new().expect("scratch");
            let exe = scratch.0.join("stall");
            fs::write(&exe, elf64(interp, 62, true)).expect("write ELF");
            let grant = Grant {
                executable: Some(exe.clone()),
                ..Grant::default()
            };

            let caps = grant.to_capability_set().expect("grant");
            let listed = caps.fs_capabilities();

            let executable = exe.canonicalize().expect("resolve executable");
            let mut files: Vec<&PathBuf> = listed
                .iter()
                .filter(|cap| cap.is_file)
                .map(|cap| &cap.resolved)
                .collect();
            files.sort();
            let mut expected_files = [&executable, &loader];
            expected_files.sort();
            assert_eq!(files, expected_files);
            assert_eq!(
                listed
                    .iter()
                    .find(|cap| cap.resolved == loader)
                    .map(|cap| cap.access),
                Some(AccessMode::Read)
            );

            let expected: Vec<PathBuf> = ["/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu"]
                .iter()
                .map(Path::new)
                .filter(|dir| dir.is_dir())
                .filter_map(|dir| dir.canonicalize().ok())
                .fold(Vec::new(), |mut dirs, dir| {
                    if !dirs.contains(&dir) {
                        dirs.push(dir);
                    }
                    dirs
                });
            let mut dirs: Vec<&PathBuf> = listed
                .iter()
                .filter(|cap| !cap.is_file)
                .map(|cap| &cap.resolved)
                .collect();
            dirs.sort();
            let mut expected_dirs: Vec<&PathBuf> = expected.iter().collect();
            expected_dirs.sort();
            assert_eq!(dirs, expected_dirs);

            assert!(caps.path_covered_with_access(interp, AccessMode::Read));
            assert!(!grant.allows_read(Path::new("/")));
            assert!(!caps.path_covered_with_access(Path::new("/"), AccessMode::Read));
            assert!(!caps.path_covered_with_access(Path::new("/usr"), AccessMode::Read));

            for root in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
                assert!(
                    !caps.path_covered_with_access(Path::new(root), AccessMode::Read),
                    "{root} was granted"
                );
            }
        }
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
    /// A loader named outside the system library roots.
    InterpreterOutsideLibraryRoot {
        /// The interpreter path that was refused.
        path: PathBuf,
    },
    /// A loader in a library layout barn does not support.
    UnsupportedLibraryLayout {
        /// The loader's parent directory.
        dir: PathBuf,
    },
    /// An ELF file that could not be opened or read.
    InterpreterIo {
        /// The ELF file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A path that has to be a regular file but is not.
    InterpreterNotAFile {
        /// The path that was refused.
        path: PathBuf,
    },
    /// An ELF file with more program headers than the parser accepts.
    TooManyProgramHeaders {
        /// The ELF file.
        path: PathBuf,
        /// The declared header count.
        count: usize,
    },
    /// An ELF file with a `PT_INTERP` longer than the parser accepts.
    InterpreterTooLong {
        /// The ELF file.
        path: PathBuf,
        /// The declared segment length.
        len: usize,
    },
    /// An ELF file that is structurally invalid.
    MalformedElf {
        /// The ELF file.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
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
            Self::InterpreterOutsideLibraryRoot { path } => {
                write!(
                    f,
                    "loader outside the system library roots: {}",
                    path.display()
                )
            }
            Self::UnsupportedLibraryLayout { dir } => {
                write!(
                    f,
                    "unsupported library layout {}: barn-luau-stall must be statically linked on this host",
                    dir.display()
                )
            }
            Self::InterpreterIo { path, source } => {
                write!(f, "reading ELF at {}: {source}", path.display())
            }
            Self::InterpreterNotAFile { path } => {
                write!(f, "not a regular file: {}", path.display())
            }
            Self::TooManyProgramHeaders { path, count } => {
                write!(
                    f,
                    "{count} program headers at {} exceeds the limit",
                    path.display()
                )
            }
            Self::InterpreterTooLong { path, len } => {
                write!(
                    f,
                    "PT_INTERP of {len} bytes at {} exceeds the limit",
                    path.display()
                )
            }
            Self::MalformedElf { path, reason } => {
                write!(f, "malformed ELF at {}: {reason}", path.display())
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
            Self::InterpreterIo { source, .. } => {
                let source: &(dyn Error + 'static) = source;
                Some(source)
            }
            Self::RelativePath { .. }
            | Self::InterpreterOutsideLibraryRoot { .. }
            | Self::UnsupportedLibraryLayout { .. }
            | Self::InterpreterNotAFile { .. }
            | Self::TooManyProgramHeaders { .. }
            | Self::InterpreterTooLong { .. }
            | Self::MalformedElf { .. } => None,
        }
    }
}

//! A scoped replacement for Luau's require, confined to one plugin folder.
//!
//! The stock loader reaches arbitrary files and participates in its own cache.
//! This one resolves every specifier against the requiring module's folder,
//! confines every load to the plugin folder, refuses escapes and cycles, and
//! evaluates each module into the same VM. Each module's chunk gets an
//! environment whose require closes over that module's folder, so a deferred
//! call still resolves against the file that defined it.
//!
//! Luau's stock global require (`FsRequirer`) is replaced with a refuser before
//! the VM is sandboxed. A module's scoped require shadows it in its own
//! environment; code that reaches globals through getfenv or setfenv finds the
//! refuser instead of a loader for arbitrary files.

use std::collections::HashMap;
use std::fmt;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, Result as LuaResult, Table, Value as LuaValue};

/// The largest module source the loader will read, in bytes.
const MAX_MODULE_BYTES: u64 = 1024 * 1024;

/// Error raised by the global require refuser. Scoped requires shadow it.
pub const REQUIRE_CONFINED: &str = "require is confined to the plugin folder";

/// A refused module load, carrying the specifier that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequireCause {
    Absolute(String),
    Parent(String),
    Escape(String),
    HardLink(String),
    Cycle(String),
    Missing(String),
    TooLarge(String),
    Unreadable { spec: String, detail: String },
    Poisoned,
}

impl fmt::Display for RequireCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute(spec) => {
                write!(f, "require rejects absolute path: {spec}")
            }
            Self::Parent(spec) => write!(f, "require rejects parent path: {spec}"),
            Self::Escape(spec) => write!(f, "require escapes plugin folder: {spec}"),
            Self::HardLink(spec) => write!(f, "require rejects hard-linked module: {spec}"),
            Self::Cycle(spec) => write!(f, "require cycle detected: {spec}"),
            Self::Missing(spec) => write!(f, "require module not found: {spec}"),
            Self::TooLarge(spec) => write!(f, "require module too large: {spec}"),
            Self::Unreadable { spec, detail } => {
                write!(f, "require cannot read {spec}: {detail}")
            }
            Self::Poisoned => f.write_str("require state lock poisoned"),
        }
    }
}

impl std::error::Error for RequireCause {}

/// Test-only failure points between taking the loading marker and evaluating
/// the chunk. Each is checked after the `LoadingGuard` owns the marker, so
/// failing there must still release it before a later require can proceed.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupFailure {
    RequireCreation,
    ModuleEnvironment,
    ChunkSetup,
}

pub struct RequireState {
    root: PathBuf,
    cache: HashMap<PathBuf, LuaValue>,
    loading: Vec<PathBuf>,
    #[cfg(test)]
    setup_failure: Option<SetupFailure>,
}

enum Resolved {
    Cached(LuaValue),
    Load {
        canonical: PathBuf,
        source: String,
        name: String,
    },
}

/// Clears one loading marker on scope exit. `resolve` pushes the path; the
/// fallible work between that push and `finish` can return early, so cleanup
/// lives in `Drop` rather than only on the success path.
struct LoadingGuard {
    state: Arc<Mutex<RequireState>>,
    path: PathBuf,
}

impl LoadingGuard {
    fn new(state: &Arc<Mutex<RequireState>>, path: PathBuf) -> Self {
        Self {
            state: Arc::clone(state),
            path,
        }
    }
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.loading.retain(|loading| loading != &self.path);
        }
    }
}

impl RequireState {
    pub fn new(folder: &Path) -> Result<Self, String> {
        let root = std::fs::canonicalize(folder)
            .map_err(|error| format!("plugin folder unavailable: {error}"))?;
        Ok(Self {
            root,
            cache: HashMap::new(),
            loading: Vec::new(),
            #[cfg(test)]
            setup_failure: None,
        })
    }

    fn resolve(&mut self, spec: &str, base: &Path) -> Result<Resolved, RequireCause> {
        let relative = Path::new(spec);
        if relative.is_absolute() {
            return Err(RequireCause::Absolute(spec.to_owned()));
        }
        if relative
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(RequireCause::Parent(spec.to_owned()));
        }

        let candidate = base.join(relative);
        let canonical = self.locate(&candidate, spec)?;
        if let Some(value) = self.cache.get(&canonical) {
            return Ok(Resolved::Cached(value.clone()));
        }
        if self.loading.contains(&canonical) {
            return Err(RequireCause::Cycle(spec.to_owned()));
        }

        let name = canonical.to_string_lossy().into_owned();
        let source = read_source(&canonical, spec)?;
        self.loading.push(canonical.clone());
        Ok(Resolved::Load {
            canonical,
            source,
            name,
        })
    }

    /// Resolve a specifier through a small, fixed list of Luau module paths.
    fn locate(&self, candidate: &Path, spec: &str) -> Result<PathBuf, RequireCause> {
        let mut paths = Vec::with_capacity(4);
        if matches!(
            candidate.extension().and_then(|ext| ext.to_str()),
            Some("luau" | "lua")
        ) {
            paths.push(candidate.to_path_buf());
        }
        paths.push(candidate.with_extension("luau"));
        paths.push(candidate.with_extension("lua"));
        paths.push(candidate.join("init.luau"));

        for path in paths {
            if !path.is_file() {
                continue;
            }
            let canonical =
                std::fs::canonicalize(&path).map_err(|error| RequireCause::Unreadable {
                    spec: spec.to_owned(),
                    detail: error.to_string(),
                })?;
            if !canonical.starts_with(&self.root) {
                return Err(RequireCause::Escape(spec.to_owned()));
            }
            let metadata =
                std::fs::metadata(&canonical).map_err(|error| RequireCause::Unreadable {
                    spec: spec.to_owned(),
                    detail: error.to_string(),
                })?;
            if metadata.is_file() && metadata.nlink() > 1 {
                return Err(RequireCause::HardLink(spec.to_owned()));
            }
            return Ok(canonical);
        }
        Err(RequireCause::Missing(spec.to_owned()))
    }

    /// Cache a completed module. Its loading marker is cleared by the guard.
    fn finish(&mut self, canonical: PathBuf, outcome: LuaResult<LuaValue>) -> LuaResult<LuaValue> {
        let value = outcome?;
        self.cache.insert(canonical, value.clone());
        Ok(value)
    }
}

/// Read a module source under a hard size cap, with one read and no growth.
fn read_source(path: &Path, spec: &str) -> Result<String, RequireCause> {
    let file = std::fs::File::open(path).map_err(|error| RequireCause::Unreadable {
        spec: spec.to_owned(),
        detail: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MODULE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RequireCause::Unreadable {
            spec: spec.to_owned(),
            detail: error.to_string(),
        })?;
    if u64::try_from(bytes.len()).is_ok_and(|len| len > MAX_MODULE_BYTES) {
        return Err(RequireCause::TooLarge(spec.to_owned()));
    }
    String::from_utf8(bytes).map_err(|error| RequireCause::Unreadable {
        spec: spec.to_owned(),
        detail: error.to_string(),
    })
}

/// Build the scoped loader rooted at the plugin folder, returning the function
/// so the host can load the entry module through the same path and cache.
pub fn install(lua: &Lua, state: RequireState) -> LuaResult<Function> {
    let root = state.root.clone();
    let state = Arc::new(Mutex::new(state));
    scoped_require(lua, state, root)
}

/// Replace Luau's stock global require with a function that always refuses.
/// Called before `Lua::sandbox(true)` makes the globals table read-only; each
/// module's scoped require shadows this global in its own environment.
pub fn confine_global_require(lua: &Lua) -> LuaResult<()> {
    let source = format!("return function() error('{REQUIRE_CONFINED}', 0) end");
    let refuser: Function = lua.load(&source).eval()?;
    lua.globals().set("require", refuser)
}

/// Return an injected runtime error when the step is armed on the state.
#[cfg(test)]
fn fail_setup(state: &Arc<Mutex<RequireState>>, step: SetupFailure) -> LuaResult<()> {
    let held = state
        .lock()
        .map_err(|_| mlua::Error::external(RequireCause::Poisoned))?;
    if held.setup_failure == Some(step) {
        return Err(mlua::Error::runtime(format!(
            "injected setup failure: {step:?}"
        )));
    }
    drop(held);
    Ok(())
}

/// Build a require whose relative specs resolve against one fixed folder.
fn scoped_require(
    lua: &Lua,
    state: Arc<Mutex<RequireState>>,
    base: PathBuf,
) -> LuaResult<Function> {
    lua.create_function(move |lua, requested: LuaValue| {
        let spec = match requested {
            LuaValue::String(value) => value.to_str()?.to_string(),
            _ => return Err(mlua::Error::runtime("require expects a string path")),
        };

        let resolved = {
            let mut guard = state
                .lock()
                .map_err(|_| mlua::Error::external(RequireCause::Poisoned))?;
            guard.resolve(&spec, &base).map_err(mlua::Error::external)?
        };
        match resolved {
            Resolved::Cached(value) => Ok(value),
            Resolved::Load {
                canonical,
                source,
                name,
            } => {
                // Own the loading marker from here: every step below can fail.
                let _guard = LoadingGuard::new(&state, canonical.clone());
                #[cfg(test)]
                fail_setup(&state, SetupFailure::RequireCreation)?;
                let module_base = canonical
                    .parent()
                    .map_or_else(|| base.clone(), Path::to_path_buf);
                let require = scoped_require(lua, Arc::clone(&state), module_base)?;
                #[cfg(test)]
                fail_setup(&state, SetupFailure::ModuleEnvironment)?;
                let env = module_env(lua, require)?;
                #[cfg(test)]
                fail_setup(&state, SetupFailure::ChunkSetup)?;
                // The mutex guard is dropped before entering Lua: a module
                // may call require again for its own siblings.
                let outcome = lua
                    .load(source)
                    .set_mode(ChunkMode::Text)
                    .set_name(name)
                    .set_environment(env)
                    .eval::<LuaValue>();
                {
                    let mut loaded = state
                        .lock()
                        .map_err(|_| mlua::Error::external(RequireCause::Poisoned))?;
                    loaded.finish(canonical, outcome)
                }
            }
        }
    })
}

/// The chunk environment: sandbox globals with a require bound to its folder.
fn module_env(lua: &Lua, require: Function) -> LuaResult<Table> {
    let env = lua.create_table()?;
    env.set("require", require)?;
    let metatable = lua.create_table()?;
    metatable.set("__index", lua.globals())?;
    env.set_metatable(Some(metatable))?;
    Ok(env)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A path component unique across processes and concurrent test threads.
    fn unique_token() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    struct Source {
        path: PathBuf,
    }

    impl Source {
        fn new(bytes: usize) -> Self {
            let token = unique_token();
            let path =
                std::env::temp_dir().join(format!("barn-require-source-{token}-{bytes}.luau"));
            std::fs::write(&path, "x".repeat(bytes)).expect("write source");
            Self { path }
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn require_causes_display_their_messages() {
        for (cause, expected) in [
            (
                RequireCause::Absolute("/etc/passwd".into()),
                "require rejects absolute path: /etc/passwd".to_owned(),
            ),
            (
                RequireCause::Parent("../x".into()),
                "require rejects parent path: ../x".to_owned(),
            ),
            (
                RequireCause::Escape("./linked".into()),
                "require escapes plugin folder: ./linked".to_owned(),
            ),
            (
                RequireCause::Cycle("./a".into()),
                "require cycle detected: ./a".to_owned(),
            ),
            (
                RequireCause::HardLink("./linked".into()),
                "require rejects hard-linked module: ./linked".to_owned(),
            ),
            (
                RequireCause::Missing("./nope".into()),
                "require module not found: ./nope".to_owned(),
            ),
            (
                RequireCause::TooLarge("./big".into()),
                "require module too large: ./big".to_owned(),
            ),
            (
                RequireCause::Unreadable {
                    spec: "./x".into(),
                    detail: "boom".into(),
                },
                "require cannot read ./x: boom".to_owned(),
            ),
            (
                RequireCause::Poisoned,
                "require state lock poisoned".to_owned(),
            ),
        ] {
            assert_eq!(cause.to_string(), expected);
        }
    }

    #[test]
    fn read_source_accepts_the_cap_and_rejects_one_byte_over() {
        let cap = usize::try_from(MAX_MODULE_BYTES).expect("cap fits in usize");

        let at_cap = Source::new(cap);
        let source = read_source(&at_cap.path, "./at-cap").expect("the cap is allowed");
        assert_eq!(source.len(), cap);

        let over = Source::new(cap.saturating_add(1));
        assert_eq!(
            read_source(&over.path, "./over").expect_err("one byte over is refused"),
            RequireCause::TooLarge("./over".into())
        );
    }

    struct Folder {
        path: PathBuf,
    }

    impl Folder {
        fn new() -> Self {
            let token = unique_token();
            let path = std::env::temp_dir().join(format!("barn-require-folder-{token}"));
            std::fs::create_dir_all(&path).expect("create scratch folder");
            Self { path }
        }

        fn write(&self, name: &str, bytes: Vec<u8>) {
            std::fs::write(self.path.join(name), bytes).expect("write module");
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scoped_require_enforces_a_fixed_one_mib_cap() {
        let folder = Folder::new();

        // Literal sizes, so a change to MAX_MODULE_BYTES cannot move the
        // boundary this test checks.
        folder.write("huge.luau", vec![b'x'; 1_048_577]);

        let lua = Lua::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let loader = install(&lua, state).expect("install require");

        let error = loader
            .call::<LuaValue>(("./huge",))
            .expect_err("a module past one MiB is refused");
        assert_eq!(
            error
                .downcast_ref::<RequireCause>()
                .expect("error carries a require cause"),
            &RequireCause::TooLarge("./huge".into())
        );

        // A module of exactly one MiB still loads through the same path.
        let mut at_cap = b"return {}".to_vec();
        at_cap.push(b'\n');
        at_cap.extend_from_slice(b"--");
        at_cap.resize(1_048_576, b'x');
        folder.write("exact.luau", at_cap);

        loader
            .call::<LuaValue>(("./exact",))
            .expect("a module at one MiB loads");
    }

    #[test]
    fn scoped_require_refuses_a_hard_link_into_the_folder() {
        let folder = Folder::new();
        let outside = Source::new(0);
        std::fs::hard_link(&outside.path, folder.path.join("linked.luau"))
            .expect("hard link into the scratch folder");

        let lua = Lua::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let loader = install(&lua, state).expect("install require");

        let error = loader
            .call::<LuaValue>(("./linked",))
            .expect_err("a hard link into the folder is refused");
        assert_eq!(
            error
                .downcast_ref::<RequireCause>()
                .expect("error carries a require cause"),
            &RequireCause::HardLink("./linked".into())
        );
    }

    #[test]
    fn scoped_require_reports_a_directory_as_missing_not_a_hard_link() {
        let folder = Folder::new();
        std::fs::create_dir(folder.path.join("package"))
            .expect("create a directory in the scratch folder");

        let lua = Lua::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let loader = install(&lua, state).expect("install require");

        let error = loader
            .call::<LuaValue>(("./package",))
            .expect_err("a directory is not a module");
        assert_eq!(
            error
                .downcast_ref::<RequireCause>()
                .expect("error carries a require cause"),
            &RequireCause::Missing("./package".into())
        );
    }

    #[test]
    fn loading_guard_removes_only_its_path_on_drop() {
        let folder = Folder::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let guarded = folder.path.join("guarded.luau");
        let unrelated = folder.path.join("unrelated.luau");

        let shared = Arc::new(Mutex::new(state));
        {
            let mut state = shared.lock().expect("lock state");
            state.loading.push(guarded.clone());
            state.loading.push(unrelated.clone());
        }

        {
            let _guard = LoadingGuard::new(&shared, guarded);
        }

        assert_eq!(shared.lock().expect("lock state").loading, vec![unrelated]);
    }

    #[test]
    fn a_failed_module_load_can_be_required_again() {
        let folder = Folder::new();
        folder.write("bad.luau", b"error('boom')".to_vec());

        let lua = Lua::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let loader = install(&lua, state).expect("install require");

        let first = loader
            .call::<LuaValue>(("./bad",))
            .expect_err("a module that raises at load time fails");
        assert_eq!(first.downcast_ref::<RequireCause>(), None);

        let second = loader
            .call::<LuaValue>(("./bad",))
            .expect_err("a failed module can be required again and fails again");
        assert_eq!(second.downcast_ref::<RequireCause>(), None);
    }

    #[test]
    fn scoped_require_refuses_a_hard_link_within_the_folder() {
        let folder = Folder::new();
        folder.write("original.luau", b"return {}".to_vec());
        std::fs::hard_link(
            folder.path.join("original.luau"),
            folder.path.join("alias.luau"),
        )
        .expect("hard link within the scratch folder");

        let lua = Lua::new();
        let state = RequireState::new(&folder.path).expect("require state");
        let loader = install(&lua, state).expect("install require");

        // Both links live inside the plugin folder, yet the blanket rule still
        // refuses regular files with more than one link: no link may escape.
        let error = loader
            .call::<LuaValue>(("./original",))
            .expect_err("a hard link within the folder is refused");
        assert_eq!(
            error
                .downcast_ref::<RequireCause>()
                .expect("error carries a require cause"),
            &RequireCause::HardLink("./original".into())
        );

        // The refusal happens before the loading marker is taken, so a second
        // call reaches the same check instead of reporting a cycle.
        let second = loader
            .call::<LuaValue>(("./original",))
            .expect_err("a second require is refused for the same reason");
        assert_eq!(
            second
                .downcast_ref::<RequireCause>()
                .expect("error carries a require cause"),
            &RequireCause::HardLink("./original".into())
        );
    }

    /// A loader plus a handle on its state, so a test can arm a setup failure
    /// and inspect the loading markers left behind by a failed load.
    fn loader_with_state(lua: &Lua, folder: &Folder) -> (Function, Arc<Mutex<RequireState>>) {
        let state = RequireState::new(&folder.path).expect("require state");
        let root = state.root.clone();
        let state = Arc::new(Mutex::new(state));
        let loader = scoped_require(lua, Arc::clone(&state), root).expect("scoped require");
        (loader, state)
    }

    fn assert_early_failure_releases_loading_marker(step: SetupFailure) {
        let folder = Folder::new();
        folder.write("mod.luau", b"return {}".to_vec());

        let lua = Lua::new();
        let (loader, state) = loader_with_state(&lua, &folder);
        state.lock().expect("lock state").setup_failure = Some(step);

        let first = loader
            .call::<LuaValue>(("./mod",))
            .expect_err("an injected setup failure stops the load");
        assert_eq!(first.downcast_ref::<RequireCause>(), None);
        assert_eq!(
            state.lock().expect("lock state").loading,
            Vec::<PathBuf>::new()
        );

        let second = loader
            .call::<LuaValue>(("./mod",))
            .expect_err("a second require reaches setup again instead of cycling");
        assert_eq!(
            second.downcast_ref::<RequireCause>(),
            None,
            "the second failure is the injected runtime error, not RequireCause::Cycle"
        );
    }

    #[test]
    fn a_failed_require_creation_can_be_required_again() {
        assert_early_failure_releases_loading_marker(SetupFailure::RequireCreation);
    }

    #[test]
    fn a_failed_module_environment_can_be_required_again() {
        assert_early_failure_releases_loading_marker(SetupFailure::ModuleEnvironment);
    }

    #[test]
    fn a_failed_chunk_setup_can_be_required_again() {
        assert_early_failure_releases_loading_marker(SetupFailure::ChunkSetup);
    }
}

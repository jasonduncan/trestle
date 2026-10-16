//! Where a project's migration history comes from (design §3.2).
//!
//! By default artifacts are read at runtime from `migrations/` (or
//! `$TRESTLE_MIGRATIONS_DIR`). A project that ships one binary embeds them
//! with [`embed_migrations!`](crate::embed_migrations); setting
//! `TRESTLE_MIGRATIONS_DIR` still overrides an embedded copy. Data steps
//! come from the registry (`app.data_step(..)`).

use std::path::{Path, PathBuf};

use trestle_historical::load::{self, ArtifactFile};
use trestle_historical::{DataStep, HistoryError, MigrationSet};

pub(crate) type HistoryFn = Box<dyn Fn() -> Result<MigrationSet, HistoryError>>;

pub(crate) enum Source {
    /// Read the directory when a command needs history.
    Dir,
    /// Files compiled into the binary.
    Embedded(Vec<ArtifactFile>),
    /// The project supplies the set itself (escape hatch).
    Function(HistoryFn),
    /// Another source, with apps whose ledger rows belong to another
    /// project ([`MigrationSet::with_foreign_apps`]).
    Foreign(Box<Source>, Vec<String>),
}

/// Embedded artifact files, built by [`embed_migrations!`](crate::embed_migrations).
pub fn embedded(dir: &include_dir::Dir<'static>) -> Vec<ArtifactFile> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn collect(dir: &include_dir::Dir<'static>, out: &mut Vec<ArtifactFile>) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(sub) => collect(sub, out),
            include_dir::DirEntry::File(file) => {
                let path = file.path().to_string_lossy().replace('\\', "/");
                let name = path.rsplit('/').next().unwrap_or_default();
                if name.starts_with('.') || !(name.ends_with(".json") || name.ends_with(".sql")) {
                    continue;
                }
                if let Some(text) = file.contents_utf8() {
                    out.push(ArtifactFile {
                        path,
                        text: text.to_owned(),
                    });
                }
            }
        }
    }
}

impl Source {
    pub(crate) fn with_foreign(self, apps: Vec<String>) -> Source {
        if apps.is_empty() {
            self
        } else {
            Source::Foreign(Box::new(self), apps)
        }
    }

    pub(crate) fn resolve(&self, dir: &Path, steps: &[DataStep]) -> Result<MigrationSet, String> {
        if let Source::Foreign(inner, apps) = self {
            return inner
                .resolve(dir, steps)?
                .with_foreign_apps(apps.iter().cloned())
                .map_err(|e| e.to_string());
        }
        let overridden = std::env::var_os(load::DIR_ENV).is_some_and(|v| !v.is_empty());
        match self {
            Source::Function(f) => f().map_err(|e| e.to_string()),
            Source::Embedded(files) if !overridden => {
                load::history(files, steps).map_err(|e| e.to_string())
            }
            _ => load::history_from_dir(dir, steps).map_err(|e| e.to_string()),
        }
    }

    /// Problems that make an embedded copy disagree with the files on disk
    /// (cargo does not notice new files in an embedded directory; rebuild).
    pub(crate) fn staleness(&self, dir: &Path) -> Vec<String> {
        if let Source::Foreign(inner, _) = self {
            return inner.staleness(dir);
        }
        let Source::Embedded(files) = self else {
            return Vec::new();
        };
        let Ok(on_disk) = load::read_dir(dir) else {
            return Vec::new();
        };
        if on_disk.is_empty() || on_disk == *files {
            return Vec::new();
        }
        vec![format!(
            "the migrations embedded in this binary differ from {}; rebuild \
             (e.g. `touch src/main.rs && cargo build`) so they match",
            dir.display()
        )]
    }
}

/// The directory to read and to write new artifacts to.
pub(crate) fn directory(configured: Option<&PathBuf>) -> PathBuf {
    load::dir_from_env(
        configured
            .cloned()
            .unwrap_or_else(|| PathBuf::from(load::DEFAULT_DIR)),
    )
}

/// Embeds a migrations directory in the binary for single-file deploys:
/// `.embedded_migrations(trestle::embed_migrations!())` embeds
/// `$CARGO_MANIFEST_DIR/migrations`; pass a path literal for another one
/// (`embed_migrations!("$CARGO_MANIFEST_DIR/db/migrations")`).
///
/// Changed files trigger a rebuild; *new* files may not, so `check` reports
/// when the embedded copy and the directory on disk differ.
#[macro_export]
macro_rules! embed_migrations {
    () => {
        $crate::embed_migrations!("$CARGO_MANIFEST_DIR/migrations")
    };
    ($path:tt) => {{
        use $crate::__private::include_dir;
        static DIR: include_dir::Dir<'static> = include_dir::include_dir!($path);
        $crate::migrations::embedded(&DIR)
    }};
}

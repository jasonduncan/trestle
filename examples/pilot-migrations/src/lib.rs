//! The pilot's committed migration history.
//!
//! Artifacts live in `migrations/<app>/<id>.json` (logical state deltas) and
//! `<id>.sql`. They are embedded in the binary and loaded at startup by
//! `trestle_historical::load`, the same loader every Trestle project uses;
//! there is no code generation. Applied migrations are immutable:
//! never edit one that has run anywhere that matters. Add a new one with
//! `pilot makemigrations` instead.

#![forbid(unsafe_code)]

pub mod data;

use std::path::PathBuf;

use include_dir::{Dir, DirEntry, include_dir};
use trestle_historical::load::{self, ArtifactFile};
use trestle_historical::{HistoryError, MigrationRef, MigrationSet};

static EMBEDDED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/migrations");

pub const SCHOOLS_0001: MigrationRef = MigrationRef::new("schools", "0001_initial");
pub const AUDIT_0001: MigrationRef = MigrationRef::new("audit", "0001_initial");
pub const LESSONS_0001: MigrationRef = MigrationRef::new("lessons", "0001_initial");
pub const LESSONS_0002: MigrationRef = MigrationRef::new("lessons", "0002_subtitle");
pub const LESSONS_0003: MigrationRef = MigrationRef::new("lessons", "0003_unit");
pub const LESSONS_0004: MigrationRef = MigrationRef::new("lessons", "0004_duration");
pub const UNITS_0001: MigrationRef = MigrationRef::new("units", "0001_initial");
pub const IDENTITY_0001: MigrationRef = MigrationRef::new("identity", "0001_initial");
pub const IDENTITY_0002: MigrationRef = MigrationRef::new("identity", "0002_accounts");

/// Apps whose rows pilot databases may hold but whose migrations now live
/// in another project: `smartfolder` moved out of this repository.
pub const FOREIGN: &[&str] = &["smartfolder"];

/// The artifact files compiled into this crate, sorted by path.
pub fn files() -> Vec<ArtifactFile> {
    fn collect(dir: &Dir<'static>, out: &mut Vec<ArtifactFile>) {
        for entry in dir.entries() {
            match entry {
                DirEntry::Dir(sub) => collect(sub, out),
                DirEntry::File(file) => {
                    let path = file.path().to_string_lossy().replace('\\', "/");
                    if path.ends_with(".json") || path.ends_with(".sql") {
                        out.push(ArtifactFile {
                            path,
                            text: file.contents_utf8().expect("UTF-8 artifact").to_owned(),
                        });
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    collect(&EMBEDDED, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The committed history, with this crate's data steps linked by name.
pub fn history() -> Result<MigrationSet, HistoryError> {
    load::history(&files(), &data::steps())
        .map_err(|e| HistoryError::State(e.to_string()))?
        .with_foreign_apps(FOREIGN.iter().copied())
}

/// Where `makemigrations` writes new artifacts.
pub fn artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

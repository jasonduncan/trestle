//! Runtime loading of migration artifacts (`<app>/<id>.json` + `<id>.sql`),
//! from a directory or from files embedded in the binary.
//!
//! This replaces build-time code generation: a project needs no migrations
//! crate. Loading never changes a digest: a [`Migration`] built here holds
//! exactly the bytes the files hold, and data steps are linked by name to
//! the [`DataStep`]s the project registered.
//!
//! Loaded text lives for the rest of the process (it is leaked once, at
//! startup), because [`Migration`] borrows `'static` strings.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::{DataStep, Migration, MigrationRef, MigrationSet, valid_name};

/// Default directory, relative to the working directory.
pub const DEFAULT_DIR: &str = "migrations";
/// Environment variable that overrides the directory.
pub const DIR_ENV: &str = "TRESTLE_MIGRATIONS_DIR";

/// One artifact file: its path relative to the migrations root, with `/`
/// separators (`lessons/0001_initial.json`), and its exact text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    pub path: String,
    pub text: String,
}

/// Everything wrong with a migrations directory, reported together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub problems: Vec<String>,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "cannot load migrations ({} problem(s)):",
            self.problems.len()
        )?;
        for problem in &self.problems {
            writeln!(f, "  - {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LoadError {}

/// The migrations directory: `$TRESTLE_MIGRATIONS_DIR`, else `fallback`.
pub fn dir_from_env(fallback: impl Into<PathBuf>) -> PathBuf {
    std::env::var_os(DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.into())
}

/// Reads every artifact under `root` (two levels: `<app>/<file>`).
///
/// A missing directory is an empty history (a new project). Symlinks are
/// refused: the directory is runtime input, and a link could pull in a file
/// from elsewhere. Files other than `.json` / `.sql` are ignored, as are
/// dot-files.
pub fn read_dir(root: &Path) -> Result<Vec<ArtifactFile>, LoadError> {
    let mut problems = Vec::new();
    let mut files = Vec::new();
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => {
            return Err(LoadError {
                problems: vec![format!("{}: {e}", root.display())],
            });
        }
    };
    if !meta.is_dir() {
        return Err(LoadError {
            problems: vec![format!("{} is not a directory", root.display())],
        });
    }
    for app in sorted_entries(root, &mut problems) {
        let name = app.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Some(kind) = file_kind(&app.path(), &mut problems) else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        for file in sorted_entries(&app.path(), &mut problems) {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let path = file.path();
            if file_name.starts_with('.')
                || !(file_name.ends_with(".json") || file_name.ends_with(".sql"))
            {
                continue;
            }
            let Some(kind) = file_kind(&path, &mut problems) else {
                continue;
            };
            if !kind.is_file() {
                problems.push(format!("{} is not a regular file", path.display()));
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => files.push(ArtifactFile {
                    path: format!("{name}/{file_name}"),
                    text,
                }),
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        }
    }
    if problems.is_empty() {
        Ok(files)
    } else {
        Err(LoadError { problems })
    }
}

fn sorted_entries(dir: &Path, problems: &mut Vec<String>) -> Vec<std::fs::DirEntry> {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            entries
        }
        Err(e) => {
            problems.push(format!("{}: {e}", dir.display()));
            Vec::new()
        }
    }
}

/// The entry's own type; symlinks are a problem, not followed.
fn file_kind(path: &Path, problems: &mut Vec<String>) -> Option<std::fs::FileType> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            problems.push(format!(
                "{} is a symlink; migrations must be plain files inside the migrations directory",
                path.display()
            ));
            None
        }
        Ok(meta) => Some(meta.file_type()),
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// Builds migrations from artifact files, linking `"data": "<name>"` to the
/// registered steps. Every problem is reported, not just the first: a
/// `.json` without its `.sql`, a manifest whose app or ID disagrees with its
/// path, a bad dependency, or a data step that is not registered.
pub fn migrations(files: &[ArtifactFile], steps: &[DataStep]) -> Result<Vec<Migration>, LoadError> {
    let mut problems = Vec::new();
    let mut registered: BTreeMap<&str, &DataStep> = BTreeMap::new();
    for step in steps {
        if registered.insert(step.name, step).is_some() {
            problems.push(format!("data step {:?} is registered twice", step.name));
        }
    }
    let by_path: BTreeMap<&str, &str> = files
        .iter()
        .map(|f| (f.path.as_str(), f.text.as_str()))
        .collect();
    let mut out = Vec::new();
    for (path, text) in &by_path {
        let Some((app, file)) = path.split_once('/') else {
            problems.push(format!("{path}: expected <app>/<id>.json"));
            continue;
        };
        if let Some(id) = file.strip_suffix(".sql") {
            if !by_path.contains_key(format!("{app}/{id}.json").as_str()) {
                problems.push(format!(
                    "{path} has no manifest {app}/{id}.json; every migration needs both files"
                ));
            }
            continue;
        }
        let Some(id) = file.strip_suffix(".json") else {
            continue;
        };
        let label = format!("migration {app}.{id}");
        if !valid_name(app) || !valid_name(id) {
            problems.push(format!(
                "{path}: invalid migration name; use lowercase letters, digits and underscores"
            ));
            continue;
        }
        let Some(sql) = by_path.get(format!("{app}/{id}.sql").as_str()) else {
            problems.push(format!(
                "{path} has no matching {app}/{id}.sql; every migration needs both files"
            ));
            continue;
        };
        let manifest: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!("{path}: invalid JSON: {e}"));
                continue;
            }
        };
        if manifest["app"] != *app {
            problems.push(format!(
                "{path}: \"app\" does not match its directory {app:?}"
            ));
        }
        if manifest["id"] != *id {
            problems.push(format!(
                "{path}: \"id\" does not match its file name {id:?}"
            ));
        }
        let mut depends_on = Vec::new();
        for dep in manifest["depends_on"].as_array().into_iter().flatten() {
            match dep.as_str().and_then(|d| d.split_once('.')) {
                Some((a, i)) => {
                    depends_on.push(MigrationRef::new(leak(a.to_owned()), leak(i.to_owned())))
                }
                None => problems.push(format!("{path}: dependency {dep} must be \"<app>.<id>\"")),
            }
        }
        let data = match &manifest["data"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(name) => match registered.get(name.as_str()) {
                Some(step) => Some(**step),
                None => {
                    problems.push(format!(
                        "{label} needs data step {name:?}; register it with \
                         app.data_step({name:?}, {name}) in the app {app:?}"
                    ));
                    None
                }
            },
            other => {
                problems.push(format!("{path}: \"data\" must be a step name, not {other}"));
                None
            }
        };
        let migration = Migration {
            app: leak(app.to_string()),
            id: leak(id.to_string()),
            depends_on: Box::leak(depends_on.into_boxed_slice()),
            sql: leak(sql.to_string()),
            manifest: Some(leak(text.to_string())),
            data,
        };
        problems.extend(migration.problems());
        out.push(migration);
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(LoadError { problems })
    }
}

/// Registered steps no migration uses. Not a load error (a step is
/// registered before `makemigrations --empty` creates its migration), but
/// `check` reports it.
pub fn unused_steps(set: &MigrationSet, steps: &[DataStep]) -> Vec<String> {
    steps
        .iter()
        .filter(|step| {
            !set.iter()
                .any(|m| m.data.is_some_and(|d| d.name == step.name))
        })
        .map(|step| {
            format!(
                "data step {:?} is registered but no migration uses it; create one with \
                 `makemigrations --empty --app <app> --data {}` or remove the registration",
                step.name, step.name
            )
        })
        .collect()
}

/// Loads, links and orders a history: [`migrations`] then
/// [`MigrationSet::ordered`].
pub fn history(files: &[ArtifactFile], steps: &[DataStep]) -> Result<MigrationSet, LoadError> {
    let loaded = migrations(files, steps)?;
    MigrationSet::ordered(loaded).map_err(|e| LoadError {
        problems: vec![e.to_string()],
    })
}

/// [`read_dir`] then [`history`].
pub fn history_from_dir(root: &Path, steps: &[DataStep]) -> Result<MigrationSet, LoadError> {
    history(&read_dir(root)?, steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> ArtifactFile {
        ArtifactFile {
            path: path.into(),
            text: text.into(),
        }
    }

    fn manifest(app: &str, id: &str, deps: &[&str], data: Option<&str>) -> String {
        serde_json::json!({
            "format": 2, "app": app, "id": id, "depends_on": deps,
            "operations": [], "data": data,
        })
        .to_string()
    }

    fn step(name: &'static str) -> DataStep {
        fn run<'a>(_: &'a mut crate::HistoricalTx<'_>) -> crate::data::DataFuture<'a> {
            Box::pin(async { Ok(()) })
        }
        DataStep {
            name,
            source: "fn step() {}",
            run,
        }
    }

    #[test]
    fn loads_orders_and_links_data_steps() {
        let files = [
            file(
                "b/0001.json",
                &manifest("b", "0001", &["a.0001"], Some("fill")),
            ),
            file("b/0001.sql", "SELECT 2;"),
            file("a/0001.json", &manifest("a", "0001", &[], None)),
            file("a/0001.sql", "SELECT 1;"),
        ];
        let set = history(&files, &[step("fill")]).unwrap();
        let keys: Vec<String> = set.iter().map(|m| m.key().to_string()).collect();
        assert_eq!(keys, ["a.0001", "b.0001"]);
        let b = set.iter().nth(1).unwrap();
        assert_eq!(b.sql, "SELECT 2;");
        assert_eq!(b.data.as_ref().unwrap().name, "fill");
    }

    #[test]
    fn reports_every_problem_with_a_fix() {
        let files = [
            file(
                "a/0001.json",
                &manifest("x", "0001", &["nodot"], Some("missing")),
            ),
            file("a/0001.sql", "SELECT 1;"),
            file("a/0002.json", &manifest("a", "0002", &[], None)),
            file("a/0003.sql", "SELECT 3;"),
        ];
        let err = migrations(&files, &[step("orphan")]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("\"app\" does not match"), "{text}");
        assert!(text.contains("must be \"<app>.<id>\""), "{text}");
        assert!(
            text.contains(
                "migration a.0001 needs data step \"missing\"; register it with app.data_step"
            ),
            "{text}"
        );
        assert!(
            text.contains("a/0002.json has no matching a/0002.sql"),
            "{text}"
        );
        assert!(text.contains("a/0003.sql has no manifest"), "{text}");
        assert_eq!(err.problems.len(), 5, "{text}");

        // An unused step is not a load error; `check` reports it.
        let ok = [
            file("a/0001.json", &manifest("a", "0001", &[], None)),
            file("a/0001.sql", "SELECT 1;"),
        ];
        let set = history(&ok, &[step("orphan")]).unwrap();
        let unused = unused_steps(&set, &[step("orphan")]);
        assert!(unused[0].contains("\"orphan\" is registered but no migration uses it"));
    }

    #[test]
    fn a_missing_directory_is_an_empty_history() {
        let dir = std::env::temp_dir().join(format!("trestle-load-none-{}", std::process::id()));
        assert!(read_dir(&dir).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused() {
        let base = std::env::temp_dir().join(format!("trestle-load-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("migrations");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(base.join("outside.sql"), "DROP TABLE x;").unwrap();
        std::os::unix::fs::symlink(base.join("outside.sql"), root.join("a/0001.sql")).unwrap();
        std::fs::write(root.join("a/0001.json"), manifest("a", "0001", &[], None)).unwrap();
        let err = read_dir(&root).unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "{err}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn digests_are_the_bytes_of_the_files() {
        let json = manifest("a", "0001", &[], None);
        let files = [
            file("a/0001.json", &json),
            file("a/0001.sql", "SELECT 1;\n"),
        ];
        let loaded = &migrations(&files, &[]).unwrap()[0];
        let direct = Migration {
            app: "a",
            id: "0001",
            depends_on: &[],
            sql: "SELECT 1;\n",
            manifest: Some(Box::leak(json.into_boxed_str())),
            data: None,
        };
        assert_eq!(loaded.digest(), direct.digest());
    }
}

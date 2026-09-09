use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::model::{Repository, Strategy};

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

struct ConfigLock {
    _file: fs::File,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Explicitly unlock: another thread may briefly inherit this descriptor
            // while spawning Git, before close-on-exec takes effect.
            unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        let directory = match env::var_os("REPO_SYNC_HOME") {
            Some(value) => PathBuf::from(value),
            None => default_config_directory(
                &env::current_exe().context("cannot locate the repo-sync executable")?,
            )?,
        };
        Ok(Self::at(directory.join("repos.tsv")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("configuration file has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config dir: {}", display_path(parent)))?;
        if !self.path.exists() {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot write config file: {}", display_path(&self.path))
                    });
                }
            }
        }
        Ok(())
    }

    pub fn load(&self) -> Result<Vec<Repository>> {
        self.ensure()?;
        let file = fs::File::open(&self.path)
            .with_context(|| format!("cannot read config file: {}", display_path(&self.path)))?;
        let mut repositories = Vec::new();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 4 {
                bail!(
                    "invalid config line {}: expected 4 tab-separated fields",
                    index + 1
                );
            }
            repositories.push(Repository {
                name: fields[0].to_owned(),
                path: PathBuf::from(fields[1]),
                strategy: Strategy::parse(fields[2])
                    .with_context(|| format!("invalid strategy on config line {}", index + 1))?,
                submodules: match fields[3] {
                    "true" => true,
                    "false" => false,
                    _ => bail!("invalid submodules value on config line {}", index + 1),
                },
            });
        }
        validate_all(&repositories)?;
        Ok(repositories)
    }

    #[cfg(test)]
    pub fn save(&self, repositories: &[Repository]) -> Result<()> {
        let _lock = self.lock()?;
        self.write(repositories)
    }

    fn write(&self, repositories: &[Repository]) -> Result<()> {
        validate_all(repositories)?;
        let mut sorted = repositories.to_vec();
        sorted.sort_by(|left, right| left.name.cmp(&right.name));

        let parent = self
            .path
            .parent()
            .context("config file has no parent directory")?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_path = parent.join(format!("repos.{}.{}.tmp", std::process::id(), nonce));
        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .context("cannot create temporary config file")?;
            for repository in &sorted {
                writeln!(
                    file,
                    "{}\t{}\t{}\t{}",
                    repository.name,
                    repository.path.display(),
                    repository.strategy,
                    repository.submodules
                )?;
            }
            file.sync_all()?;
            fs::rename(&temp_path, &self.path).context("cannot replace config file")?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }

    pub fn save_if_unchanged(
        &self,
        expected: &[Repository],
        repositories: &[Repository],
    ) -> Result<()> {
        let _lock = self.lock()?;
        let mut current = self.load()?;
        let mut expected = expected.to_vec();
        current.sort_by(|left, right| left.name.cmp(&right.name));
        expected.sort_by(|left, right| left.name.cmp(&right.name));
        if current != expected {
            bail!("Registry changed in another process; reload and retry your change.");
        }
        self.write(repositories)
    }

    fn lock(&self) -> Result<ConfigLock> {
        self.ensure()?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        // Keep a separate, persistent lock file: the TSV inode is replaced on save.
        let file = options
            .open(self.path.with_extension("lock"))
            .context("Cannot lock registry; another process may be saving. Retry your change.")?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // The open descriptor owns the lock, which is released even on process exit.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Registry is busy in another process; retry your change.");
            }
        }
        Ok(ConfigLock { _file: file })
    }

    pub fn find_index(repositories: &[Repository], target: &str) -> Option<usize> {
        let target_path = Path::new(target);
        let canonical_target = target_path.canonicalize().ok();
        repositories.iter().position(|repository| {
            repository.name == target
                || repository.path == target_path
                || canonical_target
                    .as_ref()
                    .is_some_and(|path| repository.path == *path)
        })
    }
}

fn default_config_directory(executable: &Path) -> Result<PathBuf> {
    let parent = executable
        .parent()
        .context("repo-sync executable has no parent directory")?;
    let is_cargo_profile = parent
        .file_name()
        .is_some_and(|name| name == "debug" || name == "release");
    if is_cargo_profile {
        let cargo_root = parent
            .parent()
            .filter(|target| target.file_name().is_some_and(|name| name == "target"))
            .and_then(Path::parent);
        if let Some(cargo_root) = cargo_root.filter(|root| root.join("Cargo.toml").is_file()) {
            return Ok(cargo_root.join("repo-sync-data"));
        }
    }
    Ok(parent.join("repo-sync-data"))
}

pub fn absolute_path(input: &str) -> Result<PathBuf> {
    let expanded = if input == "~" {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(input),
        }
    } else {
        PathBuf::from(input)
    };
    expanded
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {}", display_path(&expanded)))
}

pub fn display_path(path: &Path) -> String {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    if home == Path::new("/") {
        return path.display().to_string();
    }
    if path == home {
        return "~".to_owned();
    }
    match path.strip_prefix(&home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn validate_repository(repository: &Repository) -> Result<()> {
    if repository.name.is_empty() {
        bail!("repository name cannot be empty");
    }
    if repository.name.contains(['\t', '\n', '\r']) {
        bail!("repository name cannot contain tabs or newlines");
    }
    let path = repository.path.to_string_lossy();
    if path.contains(['\t', '\n', '\r']) {
        bail!("repository path cannot contain tabs or newlines");
    }
    Ok(())
}

fn validate_all(repositories: &[Repository]) -> Result<()> {
    for (index, repository) in repositories.iter().enumerate() {
        validate_repository(repository)?;
        if repositories[..index]
            .iter()
            .any(|other| other.name == repository.name)
        {
            bail!("repository name already exists: {}", repository.name);
        }
        if repositories[..index]
            .iter()
            .any(|other| other.path == repository.path)
        {
            bail!(
                "repository path already exists: {}",
                display_path(&repository.path)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn repository(name: &str, path: &Path) -> Repository {
        Repository {
            name: name.to_owned(),
            path: path.to_owned(),
            strategy: Strategy::Rebase,
            submodules: false,
        }
    }

    #[test]
    fn round_trips_and_sorts_tsv() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store
            .save(&[
                repository("zeta", Path::new("/tmp/zeta")),
                repository("alpha", Path::new("/tmp/alpha")),
            ])
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded[0].name, "alpha");
        assert_eq!(loaded[1].name, "zeta");
    }

    #[test]
    fn rejects_duplicate_names_and_paths() {
        let path = Path::new("/tmp/repository");
        let duplicate_name = vec![
            repository("same", path),
            repository("same", Path::new("/tmp/b")),
        ];
        assert!(validate_all(&duplicate_name).is_err());

        let duplicate_path = vec![repository("a", path), repository("b", path)];
        assert!(validate_all(&duplicate_path).is_err());
    }

    #[test]
    fn rejects_malformed_tsv() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        fs::write(store.path(), "missing\tfields\n").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn rejects_stale_snapshot_without_losing_external_changes() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let original = vec![repository("a", Path::new("/tmp/a"))];
        store.save(&original).unwrap();
        let mut external = original.clone();
        external.push(repository("b", Path::new("/tmp/b")));
        store.save_if_unchanged(&original, &external).unwrap();

        assert!(store.save_if_unchanged(&original, &[]).is_err());
        assert_eq!(store.load().unwrap(), external);
    }

    #[test]
    fn concurrent_writers_cannot_both_replace_the_same_snapshot() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store.ensure().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|name| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let candidate = vec![repository(name, &PathBuf::from(format!("/tmp/{name}")))];
                    barrier.wait();
                    store.save_if_unchanged(&[], &candidate).is_ok()
                })
            })
            .collect();
        let successes = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(successes, 1);
        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn development_binary_uses_the_project_registry() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("Cargo.toml"), "[package]\n").unwrap();
        let executable = directory.path().join("target/debug/repo-sync");
        assert_eq!(
            default_config_directory(&executable).unwrap(),
            directory.path().join("repo-sync-data")
        );
    }
}

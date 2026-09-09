use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::display_path;
use crate::model::{Inspection, Repository, Strategy, SyncKind, SyncResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Settings {
    pub jobs: usize,
    pub fetch_attempts: usize,
    pub terminal_prompt: bool,
    pub command_timeout: Duration,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            jobs: positive_env("REPO_SYNC_JOBS", 4)?,
            fetch_attempts: positive_env("REPO_SYNC_FETCH_ATTEMPTS", 2)?,
            terminal_prompt: true,
            command_timeout: Duration::from_secs(positive_env("REPO_SYNC_TIMEOUT_SECS", 30)? as u64),
        })
    }
}

fn positive_env(name: &str, default: usize) -> Result<usize> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    parse_positive(name, &value.to_string_lossy())
}

fn parse_positive(name: &str, text: &str) -> Result<usize> {
    let parsed = text
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be a positive integer");
    }
    Ok(parsed)
}

#[derive(Clone, Debug)]
pub struct SyncPlan {
    pub repository: Repository,
    pub branch: String,
    pub compare_ref: String,
    pub update_count: u64,
    pub dirty: bool,
    pub configured_upstream: bool,
    pub terminal_prompt: bool,
    pub command_timeout: Duration,
}

#[derive(Clone, Debug)]
pub enum PreparedSync {
    Update(SyncPlan),
    Terminal(SyncResult),
}

#[derive(Debug)]
struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    fn combined(&self) -> String {
        match (self.stdout.trim(), self.stderr.trim()) {
            ("", "") => String::new(),
            (stdout, "") => stdout.to_owned(),
            ("", stderr) => stderr.to_owned(),
            (stdout, stderr) => format!("{stdout}\n{stderr}"),
        }
    }
}

fn run_git_with_prompt(
    path: &Path,
    args: &[&str],
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<GitOutput> {
    let mut command = build_git_command(path, args, terminal_prompt);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot run git in {}", display_path(path)))?;
    let stdout = child.stdout.take().context("cannot capture git stdout")?;
    let stderr = child.stderr.take().context("cannot capture git stderr")?;
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = stdout_sender.send(read_all(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_sender.send(read_all(stderr));
    });

    let started = Instant::now();
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let timed_out = loop {
        if status.is_none() {
            status = child.try_wait().context("cannot wait for git")?;
        }
        receive_output(&stdout_receiver, &mut stdout, "git stdout");
        receive_output(&stderr_receiver, &mut stderr, "git stderr");
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            break false;
        }
        if started.elapsed() >= timeout {
            terminate_child_tree(&mut child);
            if status.is_none() {
                status = Some(child.wait().context("cannot reap timed-out git process")?);
            }
            break true;
        }
        thread::sleep(Duration::from_millis(25));
    };
    if timed_out {
        let output_deadline = Instant::now() + Duration::from_millis(250);
        while (stdout.is_none() || stderr.is_none()) && Instant::now() < output_deadline {
            receive_output(&stdout_receiver, &mut stdout, "git stdout");
            receive_output(&stderr_receiver, &mut stderr, "git stderr");
            thread::sleep(Duration::from_millis(10));
        }
    }
    let stdout = stdout.unwrap_or_else(|| b"failed to finish reading git stdout".to_vec());
    let mut stderr = stderr.unwrap_or_else(|| b"failed to finish reading git stderr".to_vec());
    if timed_out {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(
            format!("git command timed out after {}s", timeout.as_secs()).as_bytes(),
        );
    }
    Ok(GitOutput {
        success: status.is_some_and(|status| status.success()) && !timed_out,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn receive_output(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Option<Vec<u8>>, name: &str) {
    if output.is_some() {
        return;
    }
    match receiver.try_recv() {
        Ok(value) => *output = Some(value),
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            *output = Some(format!("failed to read {name}").into_bytes());
        }
    }
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let _ = reader.read_to_end(&mut output);
    output
}

fn build_git_command(path: &Path, args: &[&str], terminal_prompt: bool) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if !terminal_prompt {
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("SSH_ASKPASS_REQUIRE", "never");
        let ssh_command = match std::env::var_os("GIT_SSH_COMMAND") {
            Some(existing) => format!("{} -oBatchMode=yes", existing.to_string_lossy()),
            None => "ssh -oBatchMode=yes".to_owned(),
        };
        command.env("GIT_SSH_COMMAND", ssh_command);
    }
    command
}

fn terminate_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Git may be waiting on ssh or a credential helper. The command is started in its own
        // process group, so terminating the group also closes descendants that hold output pipes.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn git_stdout(
    path: &Path,
    args: &[&str],
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<String> {
    let output = run_git_with_prompt(path, args, terminal_prompt, timeout)?;
    if !output.success {
        bail!("{}", summary(&output.combined()));
    }
    Ok(output.stdout.trim().to_owned())
}

pub fn is_git_repository(path: &Path, terminal_prompt: bool, timeout: Duration) -> bool {
    run_git_with_prompt(
        path,
        &["rev-parse", "--is-inside-work-tree"],
        terminal_prompt,
        timeout,
    )
    .is_ok_and(|output| output.success && output.stdout.trim() == "true")
}

fn current_branch(path: &Path, terminal_prompt: bool, timeout: Duration) -> Result<String> {
    git_stdout(
        path,
        &["branch", "--show-current"],
        terminal_prompt,
        timeout,
    )
}

fn upstream(
    path: &Path,
    branch: &str,
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<Option<String>> {
    let mut configured = false;
    for key in [
        format!("branch.{branch}.remote"),
        format!("branch.{branch}.merge"),
    ] {
        let output =
            run_git_with_prompt(path, &["config", "--get", &key], terminal_prompt, timeout)?;
        if output.success {
            configured = true;
        } else if !output.stderr.trim().is_empty() {
            bail!(
                "Cannot read upstream configuration: {}",
                summary(&output.combined())
            );
        }
    }
    if !configured {
        return Ok(None);
    }
    let reference = git_stdout(
        path,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        terminal_prompt,
        timeout,
    )
    .context("Configured upstream is unavailable")?;
    Ok(Some(reference))
}

fn compare_ref(
    path: &Path,
    branch: &str,
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<Option<String>> {
    if let Some(upstream) = upstream(path, branch, terminal_prompt, timeout)? {
        return Ok(Some(upstream));
    }
    let reference = format!("refs/remotes/origin/{branch}");
    let output = run_git_with_prompt(
        path,
        &["show-ref", "--verify", "--quiet", &reference],
        terminal_prompt,
        timeout,
    )?;
    if !output.success && !output.stderr.trim().is_empty() {
        bail!("Cannot read tracking ref: {}", summary(&output.combined()));
    }
    Ok(output.success.then(|| format!("origin/{branch}")))
}

fn has_remotes(path: &Path, terminal_prompt: bool, timeout: Duration) -> bool {
    git_stdout(path, &["remote"], terminal_prompt, timeout).is_ok_and(|output| !output.is_empty())
}

fn dirty(path: &Path, terminal_prompt: bool, timeout: Duration) -> Result<bool> {
    Ok(!git_stdout(
        path,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
        terminal_prompt,
        timeout,
    )?
    .is_empty())
}

fn remote_update_count(
    path: &Path,
    reference: &str,
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<u64> {
    let range = format!("HEAD..{reference}");
    git_stdout(
        path,
        &["rev-list", "--count", &range],
        terminal_prompt,
        timeout,
    )?
    .parse()
    .context("git returned an invalid update count")
}

fn fetch(
    path: &Path,
    attempts: usize,
    sync_tags: bool,
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<()> {
    let args: &[&str] = if sync_tags {
        &["fetch", "--prune", "--tags", "--force"]
    } else {
        &["fetch", "--quiet", "--prune", "--no-tags"]
    };
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        let output = run_git_with_prompt(path, args, terminal_prompt, timeout)?;
        if output.success {
            return Ok(());
        }
        last_error = summary(&output.combined());
        if attempt < attempts {
            thread::sleep(Duration::from_secs(attempt as u64));
        }
    }
    if last_error.is_empty() {
        bail!("fetch failed");
    }
    bail!("fetch failed: {last_error}")
}

pub fn inspect_repository(
    repository: &Repository,
    refresh: bool,
    attempts: usize,
    terminal_prompt: bool,
    timeout: Duration,
) -> Inspection {
    let mut inspection = Inspection::default();
    if !repository.path.is_dir() {
        inspection.error = Some(format!(
            "Path does not exist: {}",
            display_path(&repository.path)
        ));
        return inspection;
    }
    if !is_git_repository(&repository.path, terminal_prompt, timeout) {
        inspection.error = Some(format!(
            "Not a git repository: {}",
            display_path(&repository.path)
        ));
        return inspection;
    }

    let branch = match current_branch(&repository.path, terminal_prompt, timeout) {
        Ok(branch) if !branch.is_empty() => branch,
        _ => {
            inspection.error = Some("Detached HEAD or no current branch.".to_owned());
            return inspection;
        }
    };
    inspection.branch = Some(branch.clone());

    let fetch_failed = refresh
        && has_remotes(&repository.path, terminal_prompt, timeout)
        && fetch(&repository.path, attempts, false, terminal_prompt, timeout).is_err();

    match dirty(&repository.path, terminal_prompt, timeout) {
        Ok(value) => inspection.dirty = value,
        Err(error) => {
            inspection.error = Some(format!("Cannot read git status: {error}"));
            return inspection;
        }
    }

    let reference = match compare_ref(&repository.path, &branch, terminal_prompt, timeout) {
        Ok(reference) => reference,
        Err(error) => {
            inspection.error = Some(format!("{error:#}"));
            return inspection;
        }
    };
    let Some(reference) = reference else {
        if fetch_failed && !inspection.dirty {
            inspection.error =
                Some("Fetch failed and no local tracking ref is available.".to_owned());
        } else {
            inspection.updates = Some(0);
        }
        return inspection;
    };
    inspection.compare_ref = Some(reference.clone());
    match remote_update_count(&repository.path, &reference, terminal_prompt, timeout) {
        Ok(count) => inspection.updates = Some(count),
        Err(error) if !inspection.dirty => {
            inspection.error = Some(format!("Cannot compare {branch} with {reference}: {error}"));
        }
        Err(_) => {}
    }
    inspection
}

pub fn prepare_sync(
    repository: &Repository,
    attempts: usize,
    terminal_prompt: bool,
    timeout: Duration,
) -> PreparedSync {
    let failure = |detail: String, dirty| {
        PreparedSync::Terminal(SyncResult {
            repository: repository.clone(),
            kind: SyncKind::Failed,
            dirty,
            detail,
        })
    };
    if !repository.path.is_dir() {
        return failure(
            format!("Path does not exist: {}", display_path(&repository.path)),
            false,
        );
    }
    if !is_git_repository(&repository.path, terminal_prompt, timeout) {
        return failure(
            format!("Not a git repository: {}", display_path(&repository.path)),
            false,
        );
    }
    let branch = match current_branch(&repository.path, terminal_prompt, timeout) {
        Ok(branch) if !branch.is_empty() => branch,
        _ => return failure("Detached HEAD or no current branch.".to_owned(), false),
    };
    let dirty = match dirty(&repository.path, terminal_prompt, timeout) {
        Ok(value) => value,
        Err(error) => return failure(format!("Cannot read git status: {error}"), false),
    };
    if let Err(error) = fetch(&repository.path, attempts, true, terminal_prompt, timeout) {
        return failure(error.to_string(), dirty);
    }
    let configured_upstream = match upstream(&repository.path, &branch, terminal_prompt, timeout) {
        Ok(reference) => reference,
        Err(error) => return failure(format!("{error:#}"), dirty),
    };
    let compare_ref = match configured_upstream.clone() {
        Some(reference) => Some(reference),
        None => match compare_ref(&repository.path, &branch, terminal_prompt, timeout) {
            Ok(reference) => reference,
            Err(error) => return failure(format!("{error:#}"), dirty),
        },
    };
    let Some(compare_ref) = compare_ref else {
        return failure(
            format!("No upstream or origin/{branch} for current branch."),
            dirty,
        );
    };
    let update_count =
        match remote_update_count(&repository.path, &compare_ref, terminal_prompt, timeout) {
            Ok(value) => value,
            Err(error) => {
                return failure(
                    format!("Cannot compare {branch} with {compare_ref}: {error}"),
                    dirty,
                );
            }
        };
    if update_count == 0 && !repository.submodules {
        return PreparedSync::Terminal(SyncResult {
            repository: repository.clone(),
            kind: SyncKind::Skipped,
            dirty,
            detail: String::new(),
        });
    }
    PreparedSync::Update(SyncPlan {
        repository: repository.clone(),
        branch,
        compare_ref,
        update_count,
        dirty,
        configured_upstream: configured_upstream.is_some(),
        terminal_prompt,
        command_timeout: timeout,
    })
}

pub fn apply_sync(plan: &SyncPlan) -> SyncResult {
    match apply_sync_inner(plan) {
        Ok(detail) => SyncResult {
            repository: plan.repository.clone(),
            kind: SyncKind::Updated,
            dirty: plan.dirty,
            detail,
        },
        Err(error) => SyncResult {
            repository: plan.repository.clone(),
            kind: SyncKind::Failed,
            dirty: dirty(
                &plan.repository.path,
                plan.terminal_prompt,
                plan.command_timeout,
            )
            .unwrap_or(plan.dirty),
            detail: error.to_string(),
        },
    }
}

fn apply_sync_inner(plan: &SyncPlan) -> Result<String> {
    let path = &plan.repository.path;
    let terminal_prompt = plan.terminal_prompt;
    let timeout = plan.command_timeout;
    let active_branch =
        current_branch(path, terminal_prompt, timeout).context("Cannot read current branch")?;
    if active_branch != plan.branch {
        bail!(
            "Current branch changed after fetch/check (expected {}, found {}).",
            plan.branch,
            if active_branch.is_empty() {
                "detached HEAD"
            } else {
                &active_branch
            }
        );
    }
    let active_upstream = upstream(path, &plan.branch, terminal_prompt, timeout)?;
    if plan.configured_upstream {
        if active_upstream.as_deref() != Some(plan.compare_ref.as_str()) {
            bail!(
                "Configured upstream changed after fetch/check (expected {}, found {}).",
                plan.compare_ref,
                active_upstream.as_deref().unwrap_or("none")
            );
        }
    } else if active_upstream.is_some()
        || compare_ref(path, &plan.branch, terminal_prompt, timeout)?.as_deref()
            != Some(plan.compare_ref.as_str())
    {
        bail!(
            "Pull target changed after fetch/check (expected {}).",
            plan.compare_ref
        );
    }
    let before = git_stdout(path, &["rev-parse", "HEAD"], terminal_prompt, timeout)
        .context("Cannot read current HEAD")?;
    if plan.update_count > 0 {
        let mut arguments = vec!["pull", "--no-tags"];
        match plan.repository.strategy {
            Strategy::Rebase => arguments.push("--rebase"),
            Strategy::Merge => arguments.push("--no-rebase"),
        }
        arguments.push("--recurse-submodules=on-demand");
        if !plan.configured_upstream {
            arguments.extend(["origin", plan.branch.as_str()]);
        }
        run_checked_with_prompt(
            path,
            &arguments,
            "pull failed",
            plan.terminal_prompt,
            plan.command_timeout,
        )?;
    }

    if plan.repository.submodules {
        run_checked_with_prompt(
            path,
            &["submodule", "sync", "--recursive"],
            "submodule sync failed",
            plan.terminal_prompt,
            plan.command_timeout,
        )?;
        run_checked_with_prompt(
            path,
            &["submodule", "update", "--init", "--recursive"],
            "submodule update failed",
            plan.terminal_prompt,
            plan.command_timeout,
        )?;
    }

    let output = run_git_with_prompt(
        path,
        &["diff", "--shortstat", &before, "HEAD"],
        terminal_prompt,
        timeout,
    )?;
    if !output.success {
        return Ok("diffstat unavailable".to_owned());
    }
    let stat = output.stdout.trim();
    Ok(if stat.is_empty() {
        "0 files changed".to_owned()
    } else {
        stat.to_owned()
    })
}

fn run_checked_with_prompt(
    path: &Path,
    args: &[&str],
    context: &str,
    terminal_prompt: bool,
    timeout: Duration,
) -> Result<()> {
    let output = run_git_with_prompt(path, args, terminal_prompt, timeout)?;
    if output.success {
        return Ok(());
    }
    let detail = summary(&output.combined());
    if detail.is_empty() {
        bail!("{context}");
    }
    bail!("{context}: {detail}")
}

fn summary(output: &str) -> String {
    output
        .lines()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::{TempDir, tempdir};

    fn test_timeout() -> Duration {
        Duration::from_secs(5)
    }

    struct Fixture {
        _directory: TempDir,
        origin: PathBuf,
        local: PathBuf,
        peer: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let origin = directory.path().join("origin.git");
            let seed = directory.path().join("seed");
            let local = directory.path().join("local checkout");
            let peer = directory.path().join("peer");
            git(
                None,
                &["init", "--bare", "--initial-branch", "main", path(&origin)],
            );
            fs::create_dir(&seed).unwrap();
            git(Some(&seed), &["init", "--initial-branch", "main"]);
            configure_identity(&seed);
            fs::write(seed.join("README.md"), "initial\n").unwrap();
            git(Some(&seed), &["add", "README.md"]);
            git(Some(&seed), &["commit", "-m", "initial"]);
            git(Some(&seed), &["remote", "add", "origin", path(&origin)]);
            git(Some(&seed), &["push", "-u", "origin", "main"]);
            git(None, &["clone", path(&origin), path(&local)]);
            git(None, &["clone", path(&origin), path(&peer)]);
            configure_identity(&local);
            configure_identity(&peer);
            Self {
                _directory: directory,
                origin,
                local,
                peer,
            }
        }

        fn repository(&self) -> Repository {
            Repository {
                name: "fixture".to_owned(),
                path: self.local.clone(),
                strategy: Strategy::Rebase,
                submodules: false,
            }
        }

        fn push_peer_commit(&self, contents: &str) -> String {
            fs::write(self.peer.join("README.md"), contents).unwrap();
            git(Some(&self.peer), &["add", "README.md"]);
            git(Some(&self.peer), &["commit", "-m", "remote change"]);
            git(Some(&self.peer), &["push", "origin", "main"]);
            git_output(Some(&self.peer), &["rev-parse", "HEAD"])
        }
    }

    fn path(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    fn configure_identity(repository: &Path) {
        git(Some(repository), &["config", "user.name", "Repo Sync Test"]);
        git(
            Some(repository),
            &["config", "user.email", "repo-sync@example.invalid"],
        );
    }

    fn git(repository: Option<&Path>, arguments: &[&str]) {
        let output = git_command(repository, arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(repository: Option<&Path>, arguments: &[&str]) -> String {
        let output = git_command(repository, arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_command(repository: Option<&Path>, arguments: &[&str]) -> Command {
        let mut command = Command::new("git");
        if let Some(repository) = repository {
            command.arg("-C").arg(repository);
        }
        command.args(arguments);
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    #[test]
    fn validates_positive_environment_values() {
        assert_eq!(parse_positive("TEST", "3").unwrap(), 3);
        assert!(parse_positive("TEST", "0").is_err());
        assert!(parse_positive("TEST", "nope").is_err());
    }

    #[test]
    fn summarizes_only_three_lines() {
        assert_eq!(
            summary("one\ntwo  words\nthree\nfour"),
            "one two words three"
        );
    }

    #[test]
    fn noninteractive_git_disables_git_and_ssh_prompts() {
        let command = super::build_git_command(Path::new("/tmp/repository"), &["fetch"], false);
        let environment = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_TERMINAL_PROMPT")),
            Some(&std::ffi::OsString::from("0"))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("SSH_ASKPASS_REQUIRE")),
            Some(&std::ffi::OsString::from("never"))
        );
        assert!(
            environment[std::ffi::OsStr::new("GIT_SSH_COMMAND")]
                .to_string_lossy()
                .contains("BatchMode=yes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_command_timeout_stops_the_process_group() {
        let directory = tempdir().unwrap();
        let started = Instant::now();
        let output = run_git_with_prompt(
            directory.path(),
            &["-c", "alias.wait=!sleep 2 &", "wait"],
            false,
            Duration::from_millis(100),
        )
        .unwrap();

        assert!(!output.success);
        assert!(output.stderr.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn detects_and_applies_remote_updates() {
        let fixture = Fixture::new();
        let remote_head = fixture.push_peer_commit("remote\n");
        let repository = fixture.repository();

        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert_eq!(inspection.updates, Some(1));
        assert_eq!(inspection.label(), "yes");

        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("behind repository should produce an update plan");
        };
        let result = apply_sync(&plan);
        assert_eq!(result.kind, SyncKind::Updated);
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "HEAD"]),
            remote_head
        );
    }

    #[test]
    fn ahead_only_repository_is_skipped() {
        let fixture = Fixture::new();
        fs::write(fixture.local.join("local.txt"), "local\n").unwrap();
        git(Some(&fixture.local), &["add", "local.txt"]);
        git(Some(&fixture.local), &["commit", "-m", "local change"]);
        let repository = fixture.repository();

        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert_eq!(inspection.updates, Some(0));
        assert_eq!(inspection.label(), "no");
        let PreparedSync::Terminal(result) = prepare_sync(&repository, 1, true, test_timeout())
        else {
            panic!("ahead-only repository must not be pulled");
        };
        assert_eq!(result.kind, SyncKind::Skipped);
    }

    #[test]
    fn dirty_state_is_preserved_while_syncing() {
        let fixture = Fixture::new();
        fixture.push_peer_commit("remote\n");
        fs::write(fixture.local.join("untracked.txt"), "keep me\n").unwrap();
        let repository = fixture.repository();

        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert!(inspection.dirty);
        assert_eq!(inspection.label(), "dirty");
        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("dirty repository should still be planned");
        };
        assert!(plan.dirty);
        let result = apply_sync(&plan);
        assert_eq!(result.kind, SyncKind::Updated);
        assert!(result.dirty);
        assert_eq!(
            fs::read_to_string(fixture.local.join("untracked.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn falls_back_to_origin_current_branch_without_upstream() {
        let fixture = Fixture::new();
        fixture.push_peer_commit("remote\n");
        git(
            Some(&fixture.local),
            &["branch", "--unset-upstream", "main"],
        );
        let repository = fixture.repository();
        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert_eq!(inspection.compare_ref.as_deref(), Some("origin/main"));

        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("origin/current fallback should produce an update plan");
        };
        assert_eq!(apply_sync(&plan).kind, SyncKind::Updated);
    }

    #[test]
    fn missing_configured_upstream_never_falls_back_to_origin() {
        let fixture = Fixture::new();
        fixture.push_peer_commit("remote\n");
        git(Some(&fixture.local), &["fetch", "origin"]);
        git(
            Some(&fixture.local),
            &["remote", "add", "upstream", path(&fixture.origin)],
        );
        git(
            Some(&fixture.local),
            &["config", "branch.main.remote", "upstream"],
        );
        git(
            Some(&fixture.local),
            &["config", "branch.main.merge", "refs/heads/deleted"],
        );
        let repository = fixture.repository();
        let inspection = inspect_repository(&repository, false, 1, false, test_timeout());
        assert!(inspection.error.is_some());
        let PreparedSync::Terminal(result) = prepare_sync(&repository, 1, false, test_timeout())
        else {
            panic!("invalid configured upstream must fail");
        };
        assert_eq!(result.kind, SyncKind::Failed);
        assert!(
            result.detail.contains("Configured upstream"),
            "{}",
            result.detail
        );
    }

    #[test]
    fn submodules_are_initialized_without_pulling_an_up_to_date_parent() {
        let fixture = Fixture::new();
        git(
            Some(&fixture.peer),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                path(&fixture.origin),
                "module",
            ],
        );
        git(Some(&fixture.peer), &["commit", "-m", "add module"]);
        git(Some(&fixture.peer), &["push", "origin", "main"]);
        git(Some(&fixture.local), &["pull", "--ff-only"]);
        fs::write(fixture.local.join("README.md"), "local edits\n").unwrap();
        let mut repository = fixture.repository();
        repository.submodules = true;
        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, false, test_timeout()) else {
            panic!("submodules must still be synchronized");
        };
        assert_eq!(plan.update_count, 0);
        // Initialize the module explicitly under a test-only protocol override, then
        // remove its worktree to reproduce a retry without requiring another clone.
        git(
            Some(&fixture.local),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        );
        git(
            Some(&fixture.local),
            &["submodule", "deinit", "--force", "module"],
        );
        let result = apply_sync(&plan);
        assert_eq!(result.kind, SyncKind::Updated, "{}", result.detail);
        assert!(fixture.local.join("module/README.md").is_file());
        assert_eq!(
            fs::read_to_string(fixture.local.join("README.md")).unwrap(),
            "local edits\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inspection_honors_timeout_for_local_status() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let hook = fixture._directory.path().join("slow-fsmonitor");
        fs::write(&hook, "#!/bin/sh\nsleep 3\nprintf '\\0'\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(
            Some(&fixture.local),
            &["config", "core.fsmonitor", path(&hook)],
        );
        let started = Instant::now();
        let inspection = inspect_repository(
            &fixture.repository(),
            false,
            1,
            false,
            Duration::from_millis(500),
        );
        assert!(
            inspection
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out")),
            "{:?}",
            inspection.error
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn detached_head_is_reported_without_mutation() {
        let fixture = Fixture::new();
        let before = git_output(Some(&fixture.local), &["rev-parse", "HEAD"]);
        git(Some(&fixture.local), &["checkout", "--detach"]);
        let repository = fixture.repository();

        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert_eq!(inspection.label(), "unknown");
        assert!(inspection.error.unwrap().contains("Detached HEAD"));
        let PreparedSync::Terminal(result) = prepare_sync(&repository, 1, true, test_timeout())
        else {
            panic!("detached HEAD should fail planning");
        };
        assert_eq!(result.kind, SyncKind::Failed);
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "HEAD"]),
            before
        );
    }

    #[test]
    fn refuses_to_pull_if_branch_changes_after_planning() {
        let fixture = Fixture::new();
        fixture.push_peer_commit("remote\n");
        let repository = fixture.repository();
        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("behind repository should produce an update plan");
        };
        git(Some(&fixture.local), &["checkout", "-b", "other"]);
        let other_head = git_output(Some(&fixture.local), &["rev-parse", "HEAD"]);

        let result = apply_sync(&plan);
        assert_eq!(result.kind, SyncKind::Failed);
        assert!(result.detail.contains("Current branch changed"));
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "HEAD"]),
            other_head
        );
    }

    #[test]
    fn refuses_to_pull_if_upstream_changes_after_planning() {
        let fixture = Fixture::new();
        fixture.push_peer_commit("remote\n");
        git(Some(&fixture.peer), &["checkout", "-b", "other"]);
        fs::write(fixture.peer.join("other.txt"), "other\n").unwrap();
        git(Some(&fixture.peer), &["add", "other.txt"]);
        git(Some(&fixture.peer), &["commit", "-m", "other branch"]);
        git(Some(&fixture.peer), &["push", "-u", "origin", "other"]);
        let repository = fixture.repository();
        let PreparedSync::Update(plan) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("behind repository should produce an update plan");
        };
        git(
            Some(&fixture.local),
            &["branch", "--set-upstream-to", "origin/other", "main"],
        );
        let before = git_output(Some(&fixture.local), &["rev-parse", "HEAD"]);

        let result = apply_sync(&plan);
        assert_eq!(result.kind, SyncKind::Failed);
        assert!(result.detail.contains("upstream changed"));
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "HEAD"]),
            before
        );
    }

    #[test]
    fn list_does_not_move_tags_but_sync_force_fetches_them() {
        let fixture = Fixture::new();
        let original = git_output(Some(&fixture.local), &["rev-parse", "HEAD"]);
        git(Some(&fixture.local), &["tag", "release"]);
        let remote_head = fixture.push_peer_commit("remote\n");
        git(Some(&fixture.peer), &["tag", "-f", "release"]);
        git(
            Some(&fixture.peer),
            &["push", "--force", "origin", "refs/tags/release"],
        );
        let repository = fixture.repository();

        let inspection = inspect_repository(&repository, true, 1, true, test_timeout());
        assert_eq!(inspection.updates, Some(1));
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "release"]),
            original
        );

        let PreparedSync::Update(_) = prepare_sync(&repository, 1, true, test_timeout()) else {
            panic!("behind repository should be planned");
        };
        assert_eq!(
            git_output(Some(&fixture.local), &["rev-parse", "release"]),
            remote_head
        );
        assert_ne!(original, remote_head);
        assert!(fixture.origin.is_dir());
    }
}

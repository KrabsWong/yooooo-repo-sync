use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{ConfigStore, absolute_path, display_path};
use crate::engine::{EngineEvent, EventSink, inspect_all, sync_all};
use crate::git::{Settings, is_git_repository};
use crate::model::{Repository, Strategy, SyncKind};

#[derive(Debug, Parser)]
#[command(name = "repo-sync", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the interactive terminal interface.
    Tui,
    /// Register a local Git repository.
    Add(AddArgs),
    /// List registered repositories and their update state.
    List(ListArgs),
    /// Remove a repository from the registry without deleting it.
    Remove { target: String },
    /// Change a registered repository.
    Set(SetArgs),
    /// Fetch and update registered repositories.
    Sync(SyncArgs),
    /// Print the configuration file path.
    Config,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    pub path: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_enum, default_value_t)]
    pub strategy: Strategy,
    #[arg(long, conflicts_with = "no_submodules")]
    pub submodules: bool,
    #[arg(long, conflicts_with = "submodules")]
    pub no_submodules: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long, conflicts_with = "no_fetch")]
    pub fetch: bool,
    #[arg(long, conflicts_with = "fetch")]
    pub no_fetch: bool,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    pub target: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long, value_enum)]
    pub strategy: Option<Strategy>,
    #[arg(long, conflicts_with = "no_submodules")]
    pub submodules: bool,
    #[arg(long, conflicts_with = "submodules")]
    pub no_submodules: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    pub targets: Vec<String>,
    #[arg(long, value_enum)]
    pub strategy: Option<Strategy>,
}

pub fn run(command: Command, store: &ConfigStore) -> Result<bool> {
    match command {
        Command::Tui => crate::tui::run(store.clone()).map(|()| true),
        Command::Add(arguments) => add(store, arguments).map(|()| true),
        Command::List(arguments) => list(store, arguments).map(|()| true),
        Command::Remove { target } => remove(store, &target).map(|()| true),
        Command::Set(arguments) => set(store, arguments).map(|()| true),
        Command::Sync(arguments) => sync(store, arguments),
        Command::Config => {
            store.ensure()?;
            println!("{}", display_path(store.path()));
            Ok(true)
        }
    }
}

fn add(store: &ConfigStore, arguments: AddArgs) -> Result<()> {
    let settings = Settings::from_env()?;
    let path = absolute_path(&arguments.path)?;
    if !is_git_repository(&path, settings.terminal_prompt, settings.command_timeout) {
        bail!("not a git repository: {}", display_path(&path));
    }
    let name = arguments.name.unwrap_or_else(|| {
        path.file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    });
    let submodules = if arguments.submodules {
        true
    } else if arguments.no_submodules {
        false
    } else {
        path.join(".gitmodules").is_file()
    };
    let repository = Repository {
        name,
        path,
        strategy: arguments.strategy,
        submodules,
    };
    let mut repositories = store.load()?;
    let expected = repositories.clone();
    if repositories
        .iter()
        .any(|other| other.name == repository.name)
    {
        bail!("repository name already exists: {}", repository.name);
    }
    if repositories
        .iter()
        .any(|other| other.path == repository.path)
    {
        bail!(
            "repository path already exists: {}",
            display_path(&repository.path)
        );
    }
    repositories.push(repository.clone());
    store.save_if_unchanged(&expected, &repositories)?;
    println!(
        "Added: {} -> {} ({}, submodules={})",
        repository.name,
        display_path(&repository.path),
        repository.strategy,
        repository.submodules
    );
    Ok(())
}

fn list(store: &ConfigStore, arguments: ListArgs) -> Result<()> {
    let settings = Settings::from_env()?;
    let repositories = store.load()?;
    if repositories.is_empty() {
        println!(
            "No repositories registered. Config: {}",
            display_path(store.path())
        );
        return Ok(());
    }
    let refresh = !arguments.no_fetch;
    let sink: EventSink = Arc::new(|_| {});
    let inspections = inspect_all(&repositories, refresh, settings, sink);
    println!("Config: {}", display_path(store.path()));
    println!(
        "{:<24}  {:<8}  {:<10}  {:<10}  path",
        "name", "strategy", "submodules", "updates"
    );
    println!(
        "{:<24}  {:<8}  {:<10}  {:<10}  ----",
        "----", "--------", "----------", "-------"
    );
    for (repository, inspection) in repositories.iter().zip(inspections) {
        println!(
            "{:<24}  {:<8}  {:<10}  {:<10}  {}",
            repository.name,
            repository.strategy,
            repository.submodules,
            inspection.label(),
            display_path(&repository.path)
        );
    }
    Ok(())
}

fn remove(store: &ConfigStore, target: &str) -> Result<()> {
    let mut repositories = store.load()?;
    let expected = repositories.clone();
    let index = ConfigStore::find_index(&repositories, target)
        .with_context(|| format!("repository not registered: {target}"))?;
    let removed = repositories.remove(index);
    store.save_if_unchanged(&expected, &repositories)?;
    println!(
        "Removed: {} -> {}",
        removed.name,
        display_path(&removed.path)
    );
    Ok(())
}

fn set(store: &ConfigStore, arguments: SetArgs) -> Result<()> {
    let settings = Settings::from_env()?;
    let mut repositories = store.load()?;
    let expected = repositories.clone();
    let index = ConfigStore::find_index(&repositories, &arguments.target)
        .with_context(|| format!("repository not registered: {}", arguments.target))?;
    let mut updated = repositories[index].clone();
    if let Some(name) = arguments.name {
        updated.name = name;
    }
    if let Some(path) = arguments.path {
        updated.path = absolute_path(&path)?;
    }
    if let Some(strategy) = arguments.strategy {
        updated.strategy = strategy;
    }
    if arguments.submodules {
        updated.submodules = true;
    } else if arguments.no_submodules {
        updated.submodules = false;
    }
    if !is_git_repository(
        &updated.path,
        settings.terminal_prompt,
        settings.command_timeout,
    ) {
        bail!("not a git repository: {}", display_path(&updated.path));
    }
    if repositories
        .iter()
        .enumerate()
        .any(|(other_index, other)| other_index != index && other.name == updated.name)
    {
        bail!("repository name already exists: {}", updated.name);
    }
    if repositories
        .iter()
        .enumerate()
        .any(|(other_index, other)| other_index != index && other.path == updated.path)
    {
        bail!(
            "repository path already exists: {}",
            display_path(&updated.path)
        );
    }
    repositories[index] = updated.clone();
    store.save_if_unchanged(&expected, &repositories)?;
    println!(
        "Updated: {} -> {} ({}, submodules={})",
        updated.name,
        display_path(&updated.path),
        updated.strategy,
        updated.submodules
    );
    Ok(())
}

fn sync(store: &ConfigStore, arguments: SyncArgs) -> Result<bool> {
    let settings = Settings::from_env()?;
    let repositories = store.load()?;
    if repositories.is_empty() {
        println!("No repositories to sync.");
        return Ok(true);
    }
    let mut selected = Vec::new();
    let mut selected_names = HashSet::new();
    if arguments.targets.is_empty() {
        selected = repositories;
    } else {
        for target in &arguments.targets {
            let index = ConfigStore::find_index(&repositories, target)
                .with_context(|| format!("repository not registered: {target}"))?;
            let repository = repositories[index].clone();
            if selected_names.insert(repository.name.clone()) {
                selected.push(repository);
            }
        }
    }
    if let Some(strategy) = arguments.strategy {
        for repository in &mut selected {
            repository.strategy = strategy;
        }
    }
    println!(
        "Phase 1: fetch and check updates ({} repos, up to {} parallel)",
        selected.len(),
        settings.jobs
    );
    let names: Vec<_> = selected
        .iter()
        .map(|repository| repository.name.clone())
        .collect();
    let sink: EventSink = Arc::new(move |event| match event {
        EngineEvent::SyncFetching(index) => println!("  Fetching: {}", names[index]),
        EngineEvent::SyncChecksFinished(count) => {
            println!("\nPhase 2: serial updates ({count} repos)\n");
        }
        EngineEvent::SyncUpdating(_, plan) => {
            let action = if plan.update_count == 0 {
                "Syncing submodules"
            } else {
                "Pulling"
            };
            println!("  {action}: {}", plan.repository.name);
        }
        EngineEvent::SyncFinished(_, result) => {
            println!("  {:?}: {}", result.kind, result.repository.name);
        }
        _ => {}
    });
    let results = sync_all(&selected, settings, sink);
    println!();
    print_sync_section("Updated", SyncKind::Updated, &results);
    print_sync_section("Skipped", SyncKind::Skipped, &results);
    print_sync_section("Failed", SyncKind::Failed, &results);
    let failed = results
        .iter()
        .filter(|result| result.kind == SyncKind::Failed)
        .count();
    let updated = results
        .iter()
        .filter(|result| result.kind == SyncKind::Updated)
        .count();
    let skipped = results.len() - updated - failed;
    let outcome = if failed == 0 {
        "SYNC COMPLETE"
    } else {
        "SYNC ENDED WITH ERRORS"
    };
    println!("\n=== {outcome} ===");
    println!(
        "{}/{} repos finished: {updated} updated, {skipped} skipped, {failed} failed",
        results.len(),
        selected.len()
    );
    Ok(failed == 0)
}

fn print_sync_section(title: &str, kind: SyncKind, results: &[crate::model::SyncResult]) {
    let matching: Vec<_> = results
        .iter()
        .filter(|result| result.kind == kind)
        .collect();
    println!("{title} ({}):", matching.len());
    if matching.is_empty() {
        println!("  none");
        return;
    }
    for result in matching {
        let dirty = result.dirty.then_some("local dirty");
        let detail = match (dirty, result.detail.is_empty()) {
            (Some(dirty), false) => format!("{dirty}; {}", result.detail),
            (Some(dirty), true) => dirty.to_owned(),
            (None, false) => result.detail.clone(),
            (None, true) => String::new(),
        };
        print!(
            "  {:<24}  {}",
            result.repository.name,
            display_path(&result.repository.path)
        );
        if !detail.is_empty() {
            print!("  | {detail}");
        }
        println!();
    }
}

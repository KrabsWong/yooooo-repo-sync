use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::config::{ConfigStore, absolute_path, display_path};
use crate::engine::{EngineEvent, EventSink, SyncLimiter, inspect_all, sync_all_with_limiter};
use crate::git::{Settings, is_git_repository};
use crate::model::{Inspection, Repository, Strategy, SyncKind, SyncResult};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Debug)]
enum RowState {
    Idle,
    Queued,
    Checking,
    Waiting,
    Ready(Inspection),
    Planning,
    Prepared(String),
    Updating(String),
    Finished(SyncResult),
}

impl RowState {
    fn label(&self) -> String {
        match self {
            Self::Idle => "idle".to_owned(),
            Self::Queued => "queued".to_owned(),
            Self::Checking => "checking".to_owned(),
            Self::Waiting => "waiting".to_owned(),
            Self::Ready(inspection) => inspection.label().to_owned(),
            Self::Planning => "fetching".to_owned(),
            Self::Prepared(_) => "queued".to_owned(),
            Self::Updating(_) => "pulling".to_owned(),
            Self::Finished(result) => match result.kind {
                SyncKind::Updated => "updated".to_owned(),
                SyncKind::Skipped => "skipped".to_owned(),
                SyncKind::Failed => "failed".to_owned(),
            },
        }
    }

    fn style(&self) -> Style {
        let color = match self {
            Self::Ready(inspection) if inspection.error.is_some() => Color::Red,
            Self::Ready(inspection) if inspection.dirty => Color::Yellow,
            Self::Ready(inspection) if inspection.updates.unwrap_or_default() > 0 => Color::Green,
            Self::Ready(_) => Color::DarkGray,
            Self::Finished(result) if result.kind == SyncKind::Updated => Color::Green,
            Self::Finished(result) if result.kind == SyncKind::Failed => Color::Red,
            Self::Finished(_) => Color::DarkGray,
            Self::Checking | Self::Planning | Self::Prepared(_) | Self::Updating(_) => Color::Cyan,
            _ => Color::Gray,
        };
        Style::default().fg(color)
    }

    fn detail(&self) -> String {
        match self {
            Self::Ready(inspection) => {
                if let Some(error) = &inspection.error {
                    return error.clone();
                }
                let mut parts = Vec::new();
                if let Some(branch) = &inspection.branch {
                    parts.push(format!("branch={branch}"));
                }
                if let Some(reference) = &inspection.compare_ref {
                    parts.push(format!("compare={reference}"));
                }
                if let Some(updates) = inspection.updates {
                    parts.push(format!("remote updates={updates}"));
                }
                if inspection.dirty {
                    parts.push("local dirty".to_owned());
                }
                parts.join("  ·  ")
            }
            Self::Finished(result) => {
                let dirty = result.dirty.then_some("local dirty");
                match (dirty, result.detail.is_empty()) {
                    (Some(dirty), false) => format!("{dirty}; {}", result.detail),
                    (Some(dirty), true) => dirty.to_owned(),
                    (None, _) => result.detail.clone(),
                }
            }
            Self::Prepared(detail) | Self::Updating(detail) => detail.clone(),
            _ => self.label(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormSubmodules {
    Auto,
    Enabled,
    Disabled,
}

impl FormSubmodules {
    fn toggle(self) -> Self {
        match self {
            Self::Auto => Self::Enabled,
            Self::Enabled => Self::Disabled,
            Self::Disabled => Self::Auto,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Enabled => "true",
            Self::Disabled => "false",
        }
    }
}

#[derive(Clone, Debug)]
struct RepoForm {
    editing_name: Option<String>,
    field: usize,
    name: String,
    path: String,
    strategy: Strategy,
    submodules: FormSubmodules,
    error: Option<String>,
}

impl RepoForm {
    fn add() -> Self {
        Self {
            editing_name: None,
            field: 0,
            name: String::new(),
            path: String::new(),
            strategy: Strategy::Rebase,
            submodules: FormSubmodules::Auto,
            error: None,
        }
    }

    fn edit(repository: &Repository) -> Self {
        Self {
            editing_name: Some(repository.name.clone()),
            field: 0,
            name: repository.name.clone(),
            path: repository.path.display().to_string(),
            strategy: repository.strategy,
            submodules: if repository.submodules {
                FormSubmodules::Enabled
            } else {
                FormSubmodules::Disabled
            },
            error: None,
        }
    }
}

#[derive(Clone, Debug)]
enum Mode {
    Normal,
    Help,
    Form(RepoForm),
    ConfirmDelete,
}

#[derive(Debug)]
enum UiMessage {
    Engine {
        run_id: u64,
        event: EngineEvent,
    },
    InspectionComplete {
        run_id: u64,
        results: Vec<Inspection>,
    },
    SyncComplete {
        run_id: u64,
        results: Vec<SyncResult>,
    },
    OperationFailed {
        run_id: u64,
        operation: &'static str,
        detail: String,
    },
}

#[derive(Debug)]
struct SyncBatch {
    indices: Vec<usize>,
    keys: Vec<PathBuf>,
    completed: HashSet<usize>,
}

#[derive(Debug, Default)]
struct SyncTotals {
    launched: usize,
    finished: usize,
    updated: usize,
    skipped: usize,
    failed: usize,
}

struct Completion {
    title: &'static str,
    summary: String,
    failed: bool,
}

struct App {
    store: ConfigStore,
    settings: Settings,
    repositories: Vec<Repository>,
    states: Vec<RowState>,
    table_state: TableState,
    marked: HashSet<String>,
    sync_batches: HashMap<u64, SyncBatch>,
    active_repositories: HashMap<PathBuf, u64>,
    sync_totals: SyncTotals,
    refresh_run: Option<u64>,
    sync_limiter: SyncLimiter,
    color_enabled: bool,
    mode: Mode,
    status: String,
    completion: Option<Completion>,
    quit: bool,
    quit_pending: bool,
    run_id: u64,
    sender: mpsc::Sender<UiMessage>,
    receiver: mpsc::Receiver<UiMessage>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl App {
    fn new(store: ConfigStore, settings: Settings) -> Result<Self> {
        let repositories = store.load()?;
        let (sender, receiver) = mpsc::channel();
        let mut table_state = TableState::default();
        if !repositories.is_empty() {
            table_state.select(Some(0));
        }
        let states = vec![RowState::Idle; repositories.len()];
        Ok(Self {
            store,
            settings,
            repositories,
            states,
            table_state,
            marked: HashSet::new(),
            sync_batches: HashMap::new(),
            active_repositories: HashMap::new(),
            sync_totals: SyncTotals::default(),
            refresh_run: None,
            sync_limiter: SyncLimiter::new(settings.jobs),
            color_enabled: std::env::var_os("NO_COLOR").is_none(),
            mode: Mode::Normal,
            status: "ready".to_owned(),
            completion: None,
            quit: false,
            quit_pending: false,
            run_id: 0,
            sender,
            receiver,
            workers: Vec::new(),
        })
    }

    fn selected(&self) -> Option<usize> {
        self.table_state.selected()
    }

    fn next(&mut self) {
        if self.repositories.is_empty() {
            return;
        }
        let index = self.selected().unwrap_or(0);
        self.table_state
            .select(Some((index + 1) % self.repositories.len()));
    }

    fn previous(&mut self) {
        if self.repositories.is_empty() {
            return;
        }
        let index = self.selected().unwrap_or(0);
        self.table_state.select(Some(if index == 0 {
            self.repositories.len() - 1
        } else {
            index - 1
        }));
    }

    fn toggle_mark(&mut self) {
        let Some(index) = self.selected() else { return };
        let name = self.repositories[index].name.clone();
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
    }

    fn has_active_operations(&self) -> bool {
        self.refresh_run.is_some() || !self.sync_batches.is_empty()
    }

    fn allocate_run_id(&mut self) -> u64 {
        self.run_id += 1;
        self.run_id
    }

    fn available_sync_indices(&self, requested: Vec<usize>) -> (Vec<usize>, usize) {
        let mut available = Vec::new();
        let mut skipped = 0;
        let mut seen = HashSet::new();
        for index in requested {
            let key = self.repository_key(index);
            if self.active_repositories.contains_key(&key) || !seen.insert(key) {
                skipped += 1;
            } else {
                available.push(index);
            }
        }
        (available, skipped)
    }

    fn reserve_sync_batch(&mut self, indices: Vec<usize>) -> u64 {
        self.completion = None;
        if self.sync_batches.is_empty() {
            self.sync_totals = SyncTotals::default();
        }
        let run_id = self.allocate_run_id();
        let keys = indices
            .iter()
            .map(|index| self.repository_key(*index))
            .collect::<Vec<_>>();
        for (index, key) in indices.iter().zip(&keys) {
            self.active_repositories.insert(key.clone(), run_id);
            self.marked.remove(&self.repositories[*index].name);
            self.states[*index] = RowState::Waiting;
        }
        self.sync_totals.launched += indices.len();
        self.sync_batches.insert(
            run_id,
            SyncBatch {
                indices: indices.clone(),
                keys,
                completed: HashSet::new(),
            },
        );
        run_id
    }

    fn repository_key(&self, index: usize) -> PathBuf {
        self.repositories[index]
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.repositories[index].path.clone())
    }

    fn start_refresh(&mut self, refresh: bool) {
        if self.quit_pending || self.quit {
            return;
        }
        if self.has_active_operations() {
            self.status = "An operation is already running".to_owned();
            return;
        }
        let run_id = self.allocate_run_id();
        self.refresh_run = Some(run_id);
        self.completion = None;
        self.states.fill(RowState::Queued);
        self.status = if refresh {
            "Refreshing remote state…".to_owned()
        } else {
            "Reading local tracking state…".to_owned()
        };
        let repositories = self.repositories.clone();
        let settings = self.settings;
        let sender = self.sender.clone();
        let spawn_result = thread::Builder::new()
            .name(format!("repo-refresh-{run_id}"))
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    let event_sender = sender.clone();
                    let sink: EventSink = Arc::new(move |event| {
                        let _ = event_sender.send(UiMessage::Engine { run_id, event });
                    });
                    inspect_all(&repositories, refresh, settings, sink)
                }));
                let message = match outcome {
                    Ok(results) => UiMessage::InspectionComplete { run_id, results },
                    Err(payload) => UiMessage::OperationFailed {
                        run_id,
                        operation: "Refresh",
                        detail: panic_detail(payload),
                    },
                };
                let _ = sender.send(message);
            });
        match spawn_result {
            Ok(worker) => self.workers.push(worker),
            Err(error) => {
                self.refresh_run = None;
                self.states.fill(RowState::Idle);
                self.status = format!("Cannot start background worker: {error}");
            }
        }
    }

    fn start_sync(&mut self, all: bool) {
        if self.quit_pending || self.quit {
            return;
        }
        if self.refresh_run.is_some() {
            self.status = "Wait for refresh to finish before syncing".to_owned();
            return;
        }
        let requested = if all {
            (0..self.repositories.len()).collect::<Vec<_>>()
        } else if !self.marked.is_empty() {
            self.repositories
                .iter()
                .enumerate()
                .filter_map(|(index, repository)| {
                    self.marked.contains(&repository.name).then_some(index)
                })
                .collect()
        } else {
            self.selected().into_iter().collect()
        };
        if requested.is_empty() {
            self.status = "No repositories selected".to_owned();
            return;
        }
        let (indices, skipped) = self.available_sync_indices(requested);
        if indices.is_empty() {
            self.status = "Selected repositories are already syncing".to_owned();
            return;
        }

        let run_id = self.reserve_sync_batch(indices.clone());
        self.update_sync_status();
        if skipped > 0 {
            self.status
                .push_str(&format!("; {skipped} already syncing"));
        }
        let repositories = indices
            .iter()
            .map(|index| self.repositories[*index].clone())
            .collect::<Vec<_>>();
        let settings = self.settings;
        let sender = self.sender.clone();
        let limiter = self.sync_limiter.clone();
        let spawn_result = thread::Builder::new()
            .name(format!("repo-sync-{run_id}"))
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    let event_sender = sender.clone();
                    let sink: EventSink = Arc::new(move |event| {
                        let _ = event_sender.send(UiMessage::Engine { run_id, event });
                    });
                    sync_all_with_limiter(&repositories, settings, sink, limiter)
                }));
                let message = match outcome {
                    Ok(results) => UiMessage::SyncComplete { run_id, results },
                    Err(payload) => UiMessage::OperationFailed {
                        run_id,
                        operation: "Sync",
                        detail: panic_detail(payload),
                    },
                };
                let _ = sender.send(message);
            });
        match spawn_result {
            Ok(worker) => self.workers.push(worker),
            Err(error) => {
                self.fail_sync_batch(run_id, format!("cannot start background worker: {error}"))
            }
        }
    }

    fn drain_messages(&mut self) {
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                let _ = self.workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                UiMessage::Engine { run_id, event } => match event {
                    EngineEvent::InspectStarted(index) => {
                        if self.refresh_run == Some(run_id)
                            && let Some(state) = self.states.get_mut(index)
                        {
                            *state = RowState::Checking;
                        }
                    }
                    EngineEvent::InspectFinished(index, inspection) => {
                        if self.refresh_run == Some(run_id)
                            && let Some(state) = self.states.get_mut(index)
                        {
                            *state = RowState::Ready(inspection);
                        }
                    }
                    EngineEvent::SyncChecksFinished(_) => {}
                    EngineEvent::SyncFetching(index) => {
                        if let Some(global) = self.sync_global_index(run_id, index) {
                            self.states[global] = RowState::Planning;
                            self.update_sync_status();
                        }
                    }
                    EngineEvent::SyncPrepared(index, plan) => {
                        if let Some(global) = self.sync_global_index(run_id, index) {
                            self.states[global] = RowState::Prepared(sync_plan_detail(&plan));
                            self.update_sync_status();
                        }
                    }
                    EngineEvent::SyncUpdating(index, plan) => {
                        if let Some(global) = self.sync_global_index(run_id, index) {
                            self.states[global] = RowState::Updating(sync_plan_detail(&plan));
                            self.update_sync_status();
                        }
                    }
                    EngineEvent::SyncFinished(index, result) => {
                        if let Some(global) = self.sync_global_index(run_id, index) {
                            self.record_sync_result(run_id, index, result.kind);
                            self.states[global] = RowState::Finished(result);
                            self.update_sync_status();
                        }
                    }
                },
                UiMessage::InspectionComplete { run_id, results }
                    if self.refresh_run == Some(run_id) =>
                {
                    self.states = results.into_iter().map(RowState::Ready).collect();
                    self.refresh_run = None;
                    self.status = "Refresh complete".to_owned();
                    let failed = self.states.iter().filter(|state| {
                        matches!(state, RowState::Ready(inspection) if inspection.error.is_some())
                    }).count();
                    self.completion = Some(Completion {
                        title: if failed == 0 {
                            "REFRESH COMPLETE"
                        } else {
                            "REFRESH ENDED WITH ERRORS"
                        },
                        summary: format!(
                            "{} repos checked, {failed} failed",
                            self.repositories.len()
                        ),
                        failed: failed > 0,
                    });
                    self.maybe_finish_quit();
                }
                UiMessage::SyncComplete { run_id, results } => {
                    let Some(indices) = self
                        .sync_batches
                        .get(&run_id)
                        .map(|batch| batch.indices.clone())
                    else {
                        continue;
                    };
                    for (relative, (index, result)) in
                        indices.into_iter().zip(results.iter().cloned()).enumerate()
                    {
                        self.record_sync_result(run_id, relative, result.kind);
                        self.states[index] = RowState::Finished(result);
                    }
                    self.release_sync_batch(run_id);
                    if self.sync_batches.is_empty() {
                        self.finish_sync();
                    } else {
                        self.update_sync_status();
                    }
                    self.maybe_finish_quit();
                }
                UiMessage::OperationFailed {
                    run_id,
                    operation,
                    detail,
                } => {
                    if self.refresh_run == Some(run_id) {
                        for state in &mut self.states {
                            if matches!(state, RowState::Queued | RowState::Checking) {
                                *state = RowState::Idle;
                            }
                        }
                        self.refresh_run = None;
                        self.status = format!("{operation} failed: {detail}");
                        self.completion = Some(Completion {
                            title: "REFRESH FAILED",
                            summary: detail,
                            failed: true,
                        });
                        self.maybe_finish_quit();
                    } else if self.sync_batches.contains_key(&run_id) {
                        self.fail_sync_batch(run_id, detail);
                    }
                }
                _ => {}
            }
        }
    }

    fn sync_global_index(&self, run_id: u64, relative_index: usize) -> Option<usize> {
        self.sync_batches
            .get(&run_id)?
            .indices
            .get(relative_index)
            .copied()
    }

    fn release_sync_batch(&mut self, run_id: u64) -> Option<SyncBatch> {
        let batch = self.sync_batches.remove(&run_id)?;
        for key in &batch.keys {
            if self.active_repositories.get(key) == Some(&run_id) {
                self.active_repositories.remove(key);
            }
        }
        Some(batch)
    }

    fn fail_sync_batch(&mut self, run_id: u64, detail: String) {
        let Some(indices) = self
            .sync_batches
            .get(&run_id)
            .map(|batch| batch.indices.clone())
        else {
            return;
        };
        for (relative, index) in indices.into_iter().enumerate() {
            let already_finished = self
                .sync_batches
                .get(&run_id)
                .is_some_and(|batch| batch.completed.contains(&relative));
            if !already_finished {
                self.record_sync_result(run_id, relative, SyncKind::Failed);
                self.states[index] = RowState::Finished(SyncResult {
                    repository: self.repositories[index].clone(),
                    kind: SyncKind::Failed,
                    dirty: false,
                    detail: detail.clone(),
                });
            }
        }
        self.release_sync_batch(run_id);
        if self.sync_batches.is_empty() {
            self.finish_sync();
        } else {
            self.update_sync_status();
        }
        self.maybe_finish_quit();
    }

    fn update_sync_status(&mut self) {
        let mut waiting = 0;
        let mut fetching = 0;
        let mut queued = 0;
        let mut pulling = 0;
        for batch in self.sync_batches.values() {
            for index in &batch.indices {
                match self.states[*index] {
                    RowState::Waiting => waiting += 1,
                    RowState::Planning => fetching += 1,
                    RowState::Prepared(_) => queued += 1,
                    RowState::Updating(_) => pulling += 1,
                    _ => {}
                }
            }
        }
        self.status = format!(
            "sync {}/{}: {waiting} waiting, {fetching} fetching, {queued} queued, {pulling} pulling",
            self.sync_totals.finished, self.sync_totals.launched
        );
    }

    fn record_sync_result(&mut self, run_id: u64, relative_index: usize, kind: SyncKind) {
        let Some(batch) = self.sync_batches.get_mut(&run_id) else {
            return;
        };
        if !batch.completed.insert(relative_index) {
            return;
        }
        self.sync_totals.finished += 1;
        match kind {
            SyncKind::Updated => self.sync_totals.updated += 1,
            SyncKind::Skipped => self.sync_totals.skipped += 1,
            SyncKind::Failed => self.sync_totals.failed += 1,
        }
    }

    fn sync_completion_summary(&self) -> String {
        format!(
            "Sync complete: {} updated, {} skipped, {} failed",
            self.sync_totals.updated, self.sync_totals.skipped, self.sync_totals.failed
        )
    }

    fn finish_sync(&mut self) {
        self.status = self.sync_completion_summary();
        self.completion = Some(Completion {
            title: if self.sync_totals.failed == 0 {
                "SYNC COMPLETE"
            } else {
                "SYNC ENDED WITH ERRORS"
            },
            summary: format!(
                "{}/{} repos: {} updated, {} skipped, {} failed",
                self.sync_totals.finished,
                self.sync_totals.launched,
                self.sync_totals.updated,
                self.sync_totals.skipped,
                self.sync_totals.failed,
            ),
            failed: self.sync_totals.failed > 0,
        });
    }

    fn maybe_finish_quit(&mut self) {
        if self.quit_pending && !self.has_active_operations() {
            self.quit = true;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.quit_pending || self.quit {
            return Ok(());
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit();
            return Ok(());
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Help => {
                self.mode = if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    Mode::Normal
                } else {
                    Mode::Help
                };
            }
            Mode::ConfirmDelete => self.handle_delete_key(key)?,
            Mode::Form(mut form) => {
                if !self.handle_form_key(key, &mut form)? {
                    self.mode = Mode::Form(form);
                }
            }
        }
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Down | KeyCode::Char('j') => self.next(),
            KeyCode::Up | KeyCode::Char('k') => self.previous(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('r') => self.start_refresh(true),
            KeyCode::Char('s') => self.start_sync(false),
            KeyCode::Char('S') => self.start_sync(true),
            KeyCode::Char('a') if !self.has_active_operations() => {
                self.completion = None;
                self.mode = Mode::Form(RepoForm::add());
            }
            KeyCode::Char('e') if !self.has_active_operations() => {
                self.completion = None;
                if let Some(index) = self.selected() {
                    self.mode = Mode::Form(RepoForm::edit(&self.repositories[index]));
                }
            }
            KeyCode::Char('d') if !self.has_active_operations() && self.selected().is_some() => {
                self.completion = None;
                self.mode = Mode::ConfirmDelete;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn request_quit(&mut self) {
        if self.has_active_operations() {
            self.quit_pending = true;
            self.status = "Will quit when all active Git operations finish".to_owned();
        } else {
            self.quit = true;
        }
    }

    fn handle_delete_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let Some(index) = self.selected() else {
                    self.mode = Mode::Normal;
                    return Ok(());
                };
                let mut candidate = self.repositories.clone();
                let removed = candidate.remove(index);
                if let Err(error) = self.store.save_if_unchanged(&self.repositories, &candidate) {
                    self.status = format!(
                        "Cannot remove repository: {error:#}{}",
                        self.reload_after_save_error()
                    );
                    self.mode = Mode::Normal;
                    return Ok(());
                }
                self.repositories = candidate;
                self.states.remove(index);
                self.marked.remove(&removed.name);
                if self.repositories.is_empty() {
                    self.table_state.select(None);
                } else {
                    self.table_state
                        .select(Some(index.min(self.repositories.len() - 1)));
                }
                self.status = format!("Removed {} from the registry", removed.name);
                self.mode = Mode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Esc => self.mode = Mode::Normal,
            _ => self.mode = Mode::ConfirmDelete,
        }
        Ok(())
    }

    fn handle_form_key(&mut self, key: KeyEvent, form: &mut RepoForm) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Ok(true);
            }
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + 3) % 4,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 2 => {
                form.strategy = form.strategy.toggle();
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 3 => {
                form.submodules = form.submodules.toggle();
            }
            KeyCode::Backspace if form.field < 2 => {
                if form.field == 0 {
                    form.name.pop();
                } else {
                    form.path.pop();
                }
            }
            KeyCode::Char(character)
                if form.field < 2 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if form.field == 0 {
                    form.name.push(character);
                } else {
                    form.path.push(character);
                }
            }
            KeyCode::Enter => match self.save_form(form) {
                Ok(()) => {
                    self.mode = Mode::Normal;
                    return Ok(true);
                }
                Err(error) => form.error = Some(format!("{error:#}")),
            },
            _ => {}
        }
        Ok(false)
    }

    fn reload_after_save_error(&mut self) -> String {
        match self.store.load() {
            Ok(repositories) => {
                self.repositories = repositories;
                self.states = vec![RowState::Idle; self.repositories.len()];
                self.marked.clear();
                self.table_state
                    .select((!self.repositories.is_empty()).then_some(0));
                "; registry reloaded; review changes before retrying".to_owned()
            }
            Err(error) => format!("; cannot reload registry: {error:#}"),
        }
    }

    fn save_form(&mut self, form: &RepoForm) -> Result<()> {
        if form.name.trim().is_empty() {
            bail!("repository name cannot be empty");
        }
        if form.path.trim().is_empty() {
            bail!("repository path cannot be empty");
        }
        let path = absolute_path(form.path.trim())?;
        if !is_git_repository(
            &path,
            self.settings.terminal_prompt,
            self.settings.command_timeout,
        ) {
            bail!("not a git repository: {}", display_path(&path));
        }
        let repository = Repository {
            name: form.name.trim().to_owned(),
            submodules: match form.submodules {
                FormSubmodules::Auto => path.join(".gitmodules").is_file(),
                FormSubmodules::Enabled => true,
                FormSubmodules::Disabled => false,
            },
            path,
            strategy: form.strategy,
        };
        let editing_index = form
            .editing_name
            .as_ref()
            .and_then(|name| self.repositories.iter().position(|item| &item.name == name));
        if form.editing_name.is_some() && editing_index.is_none() {
            bail!("repository was removed; close this form and reopen the registry entry");
        }
        if self
            .repositories
            .iter()
            .enumerate()
            .any(|(index, other)| Some(index) != editing_index && other.name == repository.name)
        {
            bail!("repository name already exists: {}", repository.name);
        }
        if self
            .repositories
            .iter()
            .enumerate()
            .any(|(index, other)| Some(index) != editing_index && other.path == repository.path)
        {
            bail!(
                "repository path already exists: {}",
                display_path(&repository.path)
            );
        }
        let mut candidate = self.repositories.clone();
        match editing_index {
            Some(index) => candidate[index] = repository,
            None => candidate.push(repository),
        }
        if let Err(error) = self.store.save_if_unchanged(&self.repositories, &candidate) {
            bail!("{error:#}{}", self.reload_after_save_error());
        }
        candidate.sort_by(|left, right| left.name.cmp(&right.name));
        self.repositories = candidate;
        self.states = vec![RowState::Idle; self.repositories.len()];
        self.marked.clear();
        self.table_state
            .select((!self.repositories.is_empty()).then_some(0));
        self.status = "Registry saved".to_owned();
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Join actual workers even when terminal errors prevent completion messages
        // from being processed, or a worker fails before it can send one.
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn sync_plan_detail(plan: &crate::git::SyncPlan) -> String {
    format!(
        "branch={}  ·  compare={}  ·  remote updates={}",
        plan.branch, plan.compare_ref, plan.update_count
    )
}

fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "background worker panicked".to_owned()
    }
}

pub fn run(store: ConfigStore) -> Result<()> {
    let mut settings = Settings::from_env()?;
    settings.terminal_prompt = false;
    let mut app = App::new(store, settings)?;
    let shutdown = install_shutdown_flag()?;
    enable_raw_mode().context("cannot enable terminal raw mode")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("cannot enter alternate screen");
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => terminal,
        Err(error) => {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error).context("cannot initialize terminal");
        }
    };

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        terminal.clear()?;
        run_loop(&mut terminal, &mut app, &shutdown)
    }));
    let cleanup_result = restore_terminal(&mut terminal);
    match result {
        Ok(loop_result) => {
            loop_result?;
            cleanup_result
        }
        Err(payload) => {
            let _ = cleanup_result;
            resume_unwind(payload)
        }
    }
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    let raw_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();
    raw_result.context("cannot disable terminal raw mode")?;
    screen_result.context("cannot leave alternate screen")?;
    cursor_result.context("cannot show terminal cursor")?;
    Ok(())
}

fn run_loop(terminal: &mut TuiTerminal, app: &mut App, shutdown: &AtomicBool) -> Result<()> {
    while !app.quit {
        if shutdown.swap(false, Ordering::Relaxed) {
            app.request_quit();
        }
        app.drain_messages();
        if app.quit {
            break;
        }
        terminal.draw(|frame| render(frame, app))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => app.handle_key(key)?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn install_shutdown_flag() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
        for signal in [SIGHUP, SIGINT, SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&shutdown))
                .context("cannot install terminal shutdown handler")?;
        }
    }
    Ok(shutdown)
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let full_area = frame.area();
    let area = Rect {
        x: full_area.x.saturating_add(1),
        width: full_area.width.saturating_sub(2),
        ..full_area
    };
    let (label, color, summary) = if app.quit_pending {
        (
            "WAITING TO QUIT".to_owned(),
            Color::Yellow,
            app.status.clone(),
        )
    } else if let Some(completion) = &app.completion {
        (
            completion.title.to_owned(),
            if completion.failed {
                Color::LightRed
            } else {
                Color::Green
            },
            completion.summary.clone(),
        )
    } else if app.refresh_run.is_some() {
        let completed = app
            .states
            .iter()
            .filter(|state| matches!(state, RowState::Ready(_)))
            .count();
        (
            format!("REFRESHING  {completed}/{}", app.repositories.len()),
            Color::Blue,
            app.status.clone(),
        )
    } else if !app.sync_batches.is_empty() {
        let active_states = app
            .sync_batches
            .values()
            .flat_map(|batch| batch.indices.iter())
            .map(|index| &app.states[*index]);
        let (stage, color) = if app.sync_totals.finished == app.sync_totals.launched {
            ("FINISHING", Color::Blue)
        } else if active_states
            .clone()
            .any(|state| matches!(state, RowState::Updating(_)))
        {
            ("SYNCING", Color::Magenta)
        } else if active_states
            .clone()
            .any(|state| matches!(state, RowState::Planning))
        {
            ("FETCHING", Color::Cyan)
        } else if active_states
            .clone()
            .any(|state| matches!(state, RowState::Prepared(_)))
        {
            ("QUEUED", Color::Blue)
        } else {
            ("WAITING", Color::DarkGray)
        };
        let percent =
            app.sync_totals.finished.saturating_mul(100) / app.sync_totals.launched.max(1);
        (
            format!(
                "{stage}  {}/{} ({percent}%)",
                app.sync_totals.finished, app.sync_totals.launched
            ),
            color,
            app.status.clone(),
        )
    } else {
        ("READY".to_owned(), Color::DarkGray, app.status.clone())
    };
    let style = if app.color_enabled {
        Style::default()
            .fg(if matches!(color, Color::Blue | Color::DarkGray) {
                Color::White
            } else {
                Color::Black
            })
            .bg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    };
    let summary = if !app.sync_batches.is_empty() && !app.quit_pending {
        summary
            .split_once(": ")
            .map_or(summary.as_str(), |(_, counts)| counts)
    } else {
        summary.as_str()
    };
    let text = if summary.eq_ignore_ascii_case(&label) {
        format!(" {label} ")
    } else {
        format!(" {label}  |  {summary} ")
    };
    let bar = Paragraph::new(text).style(style).wrap(Wrap { trim: true });
    let bar_height = u16::try_from(bar.line_count(area.width))
        .unwrap_or(u16::MAX)
        .max(1);
    let table_height = u16::try_from(app.repositories.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(6 + bar_height));
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(bar_height + 1),
        Constraint::Length(table_height),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);

    let accent = if app.color_enabled {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let quiet = if app.color_enabled {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled("repo-sync", accent),
        Span::styled(
            format!(
                "  {} repos · {} marked",
                app.repositories.len(),
                app.marked.len()
            ),
            quiet,
        ),
    ]));
    frame.render_widget(title, chunks[0]);

    let rows = app
        .repositories
        .iter()
        .enumerate()
        .map(|(index, repository)| {
            let marker = if app.marked.contains(&repository.name) {
                "●"
            } else {
                " "
            };
            let branch = match &app.states[index] {
                RowState::Ready(inspection) => inspection.branch.as_deref().unwrap_or("-"),
                _ => "-",
            };
            let mut cells = vec![
                Cell::from(marker),
                Cell::from(repository.name.clone()),
                Cell::from(app.states[index].label()).style(if app.color_enabled {
                    app.states[index].style()
                } else {
                    Style::default()
                }),
            ];
            if area.width >= 100 {
                cells.extend([
                    Cell::from(branch.to_owned()),
                    Cell::from(repository.strategy.to_string()),
                    Cell::from(if repository.submodules { "yes" } else { "no" }),
                ]);
            }
            if area.width >= 60 {
                cells.push(Cell::from(display_path(&repository.path)));
            }
            Row::new(cells)
        });
    let mut headings = vec!["", "name", "status"];
    let mut widths = vec![
        Constraint::Length(2),
        if area.width < 60 {
            Constraint::Min(12)
        } else {
            Constraint::Length(26)
        },
        Constraint::Length(10),
    ];
    if area.width >= 100 {
        headings.extend(["branch", "strategy", "sub"]);
        widths.extend([
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(3),
        ]);
    }
    if area.width >= 60 {
        headings.push("path");
        widths.push(Constraint::Min(12));
    }
    let header = Row::new(headings).style(accent);
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(if app.color_enabled {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, chunks[2], &mut app.table_state);

    let detail = app.selected().map_or_else(
        || "No repositories registered. Press a to add one.".to_owned(),
        |index| {
            let repository = &app.repositories[index];
            if area.width < 100 {
                format!(
                    "{}\n{} · {} · sub={}",
                    app.states[index].detail(),
                    display_path(&repository.path),
                    repository.strategy,
                    repository.submodules
                )
            } else {
                app.states[index].detail()
            }
        },
    );
    frame.render_widget(
        Paragraph::new(format!("details  {detail}")).wrap(Wrap { trim: true }),
        chunks[3],
    );

    let shortcuts = if area.width < 60 {
        "j/k move s sync S all ? help q quit"
    } else {
        "↑/↓ move  Space mark  r refresh  s selected  S all  a/e/d edit  ? help  q quit"
    };
    frame.render_widget(
        bar,
        Rect {
            height: bar_height.min(chunks[1].height),
            ..chunks[1]
        },
    );
    frame.render_widget(Paragraph::new(shortcuts).style(quiet), chunks[4]);

    match &app.mode {
        Mode::Help => render_help(frame, full_area),
        Mode::Form(form) => render_form(frame, full_area, form, app.color_enabled),
        Mode::ConfirmDelete => render_confirm(frame, full_area, app),
        Mode::Normal => {}
    }
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let popup = centered_rect(70, 65, area);
    frame.render_widget(Clear, popup);
    let help = Paragraph::new(
        "Repository manager\n\n\
         a        add a repository\n\
         e        edit the current repository\n\
         d        remove it from the registry (files are untouched)\n\
         Space    mark/unmark for a batch sync\n\
         r        refresh all remote states\n\
         s        sync marked repositories, or the current row\n\
         S        sync every repository\n\
         q        quit when no Git operation is active\n\n\
         Each batch fetches concurrently and pulls serially.\n\
         Separate batches can run at the same time.\n\
         Press ?, q, or Esc to close this help.",
    )
    .wrap(Wrap { trim: false })
    .block(Block::default().title(" Help ").borders(Borders::ALL));
    frame.render_widget(help, popup);
}

fn render_form(frame: &mut ratatui::Frame<'_>, area: Rect, form: &RepoForm, color_enabled: bool) {
    let popup = centered_rect(72, 58, area);
    frame.render_widget(Clear, popup);
    let title = if form.editing_name.is_some() {
        " Edit repository "
    } else {
        " Add repository "
    };
    let labels = ["Name", "Path", "Strategy", "Submodules"];
    let values = [
        form.name.clone(),
        form.path.clone(),
        form.strategy.to_string(),
        form.submodules.label().to_owned(),
    ];
    let mut lines = vec![Line::raw("")];
    for (index, (label, value)) in labels.iter().zip(values).enumerate() {
        let style = if index == form.field && color_enabled {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if index == form.field {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<12}"), style),
            Span::styled(value, style),
        ]));
        lines.push(Line::raw(""));
    }
    if let Some(error) = &form.error {
        lines.push(Line::styled(
            error.clone(),
            if color_enabled {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            },
        ));
    }
    lines.push(Line::raw(
        "Tab/↑/↓ field  ←/→ toggle  Enter save  Esc cancel",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL)),
        popup,
    );
}

fn render_confirm(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let popup = centered_rect(58, 24, area);
    frame.render_widget(Clear, popup);
    let name = app
        .selected()
        .map(|index| app.repositories[index].name.as_str())
        .unwrap_or("this repository");
    frame.render_widget(
        Paragraph::new(format!(
            "Remove {name} from the registry?\nRepository files will not be deleted.\n\n[y/Enter] remove    [n/Esc] cancel"
        ))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(Block::default().title(" Confirm ").borders(Borders::ALL)),
        popup,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn app_with_repositories(names: &[&str]) -> App {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let repositories = names
            .iter()
            .map(|name| Repository {
                name: (*name).to_owned(),
                path: PathBuf::from(format!("/tmp/{name}")),
                strategy: Strategy::Rebase,
                submodules: false,
            })
            .collect::<Vec<_>>();
        store.save(&repositories).unwrap();
        App::new(
            store,
            Settings {
                jobs: 2,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap()
    }

    fn sync_plan(repository: &Repository) -> crate::git::SyncPlan {
        crate::git::SyncPlan {
            repository: repository.clone(),
            branch: "main".to_owned(),
            compare_ref: "origin/main".to_owned(),
            update_count: 2,
            dirty: false,
            configured_upstream: true,
            terminal_prompt: false,
            command_timeout: Duration::from_secs(5),
        }
    }

    fn sync_result(repository: &Repository, kind: SyncKind) -> SyncResult {
        SyncResult {
            repository: repository.clone(),
            kind,
            dirty: false,
            detail: String::new(),
        }
    }

    #[test]
    fn abnormal_exit_joins_workers_without_completion_messages() {
        for panic in [false, true] {
            let finished = Arc::new(AtomicBool::new(false));
            let worker_finished = Arc::clone(&finished);
            let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                let mut app = app_with_repositories(&[]);
                app.workers.push(thread::spawn(move || {
                    thread::sleep(Duration::from_millis(20));
                    worker_finished.store(true, Ordering::SeqCst);
                }));
                if panic {
                    panic!("terminal panic");
                }
                Err(io::Error::other("terminal error").into())
            }));
            assert!(finished.load(Ordering::SeqCst));
            if panic {
                assert!(outcome.is_err());
            } else {
                assert!(outcome.unwrap().is_err());
            }
        }
    }

    #[test]
    fn pending_quit_rejects_new_work_and_input_after_completion() {
        let mut app = app_with_repositories(&["one", "two"]);
        let run_id = app.reserve_sync_batch(vec![0]);
        app.request_quit();
        app.table_state.select(Some(1));
        app.start_sync(false);
        app.start_refresh(true);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.sync_batches.len(), 1);
        assert!(app.refresh_run.is_none());
        assert!(matches!(app.mode, Mode::Normal));
        app.sender
            .send(UiMessage::SyncComplete {
                run_id,
                results: vec![sync_result(&app.repositories[0], SyncKind::Skipped)],
            })
            .unwrap();
        app.drain_messages();
        app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.quit);
        assert!(app.sync_batches.is_empty());
        assert!(app.workers.is_empty());
    }

    #[test]
    fn form_save_conflict_keeps_external_entries_before_retry() {
        let directory = tempdir().unwrap();
        let repository_path = directory.path().join("git");
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&repository_path)
                .status()
                .unwrap()
                .success()
        );
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let external = vec![Repository {
            name: "external".to_owned(),
            path: directory.path().join("external"),
            strategy: Strategy::Rebase,
            submodules: false,
        }];
        app.store.save(&external).unwrap();
        let mut form = RepoForm::add();
        form.name = "new".to_owned();
        form.path = repository_path.to_string_lossy().into_owned();
        assert!(app.save_form(&form).is_err());
        assert_eq!(app.store.load().unwrap(), external);
        assert_eq!(app.repositories, external);
        app.save_form(&form).unwrap();
        assert_eq!(app.repositories.len(), 2);
        assert_eq!(app.repositories[0].name, "external");
        assert_eq!(app.repositories[1].name, "new");
        assert_eq!(app.store.load().unwrap(), app.repositories);
    }

    #[test]
    fn delete_conflict_preserves_external_changes_and_allows_retry() {
        let mut app = app_with_repositories(&["one"]);
        let mut external = app.repositories.clone();
        let mut added = external[0].clone();
        added.name = "two".to_owned();
        added.path = PathBuf::from("/tmp/two");
        external.push(added);
        app.store.save(&external).unwrap();
        app.handle_delete_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.store.load().unwrap(), external);
        assert_eq!(app.repositories, external);
        assert!(app.status.contains("registry reloaded"));
        assert!(!app.quit);
        app.handle_delete_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.repositories.len(), 1);
        assert_eq!(app.repositories[0].name, "two");
        assert_eq!(app.store.load().unwrap(), app.repositories);
    }

    #[test]
    fn form_submodule_choice_cycles() {
        assert_eq!(FormSubmodules::Auto.toggle(), FormSubmodules::Enabled);
        assert_eq!(FormSubmodules::Enabled.toggle(), FormSubmodules::Disabled);
        assert_eq!(FormSubmodules::Disabled.toggle(), FormSubmodules::Auto);
    }

    #[test]
    fn finished_state_keeps_dirty_and_failure_detail() {
        let state = RowState::Finished(SyncResult {
            repository: Repository {
                name: "demo".to_owned(),
                path: PathBuf::from("/tmp/demo"),
                strategy: Strategy::Rebase,
                submodules: false,
            },
            kind: SyncKind::Failed,
            dirty: true,
            detail: "pull failed".to_owned(),
        });
        assert_eq!(state.detail(), "local dirty; pull failed");
    }

    #[test]
    fn startup_is_immediately_ready_for_sync() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store
            .save(&[Repository {
                name: "demo".to_owned(),
                path: PathBuf::from("/tmp/demo"),
                strategy: Strategy::Rebase,
                submodules: false,
            }])
            .unwrap();
        let app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();

        assert!(!app.has_active_operations());
        assert!(matches!(app.states.as_slice(), [RowState::Idle]));
        assert_eq!(app.status, "ready");
    }

    #[test]
    fn background_failure_releases_busy_state() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store
            .save(&[Repository {
                name: "demo".to_owned(),
                path: PathBuf::from("/tmp/demo"),
                strategy: Strategy::Rebase,
                submodules: false,
            }])
            .unwrap();
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let run_id = app.reserve_sync_batch(vec![0]);
        app.states[0] = RowState::Planning;
        app.sender
            .send(UiMessage::OperationFailed {
                run_id,
                operation: "Sync",
                detail: "worker failed".to_owned(),
            })
            .unwrap();

        app.drain_messages();

        assert!(!app.has_active_operations());
        assert!(matches!(
            &app.states[0],
            RowState::Finished(result)
                if result.kind == SyncKind::Failed && result.detail == "worker failed"
        ));
        assert_eq!(app.status, "Sync complete: 0 updated, 0 skipped, 1 failed");
    }

    #[test]
    fn sync_progress_moves_through_each_stage() {
        let mut app = app_with_repositories(&["one"]);
        let run_id = app.reserve_sync_batch(vec![0]);
        let plan = sync_plan(&app.repositories[0]);

        app.sender
            .send(UiMessage::Engine {
                run_id,
                event: EngineEvent::SyncFetching(0),
            })
            .unwrap();
        app.drain_messages();
        assert_eq!(app.states[0].label(), "fetching");

        app.sender
            .send(UiMessage::Engine {
                run_id,
                event: EngineEvent::SyncPrepared(0, plan.clone()),
            })
            .unwrap();
        app.drain_messages();
        assert_eq!(app.states[0].label(), "queued");

        app.sender
            .send(UiMessage::Engine {
                run_id,
                event: EngineEvent::SyncUpdating(0, plan),
            })
            .unwrap();
        app.drain_messages();
        assert_eq!(app.states[0].label(), "pulling");

        app.sender
            .send(UiMessage::Engine {
                run_id,
                event: EngineEvent::SyncFinished(
                    0,
                    sync_result(&app.repositories[0], SyncKind::Updated),
                ),
            })
            .unwrap();
        app.drain_messages();
        assert_eq!(app.states[0].label(), "updated");
        assert!(app.status.starts_with("sync 1/1:"));
    }

    #[test]
    fn parallel_batches_route_events_and_preserve_new_marks() {
        let mut app = app_with_repositories(&["one", "two", "zulu"]);
        app.marked.insert("one".to_owned());
        let first = app.reserve_sync_batch(vec![0]);
        app.marked.insert("zulu".to_owned());
        let second = app.reserve_sync_batch(vec![1]);
        let first_plan = sync_plan(&app.repositories[0]);
        let second_plan = sync_plan(&app.repositories[1]);

        app.sender
            .send(UiMessage::Engine {
                run_id: second,
                event: EngineEvent::SyncUpdating(0, second_plan),
            })
            .unwrap();
        app.sender
            .send(UiMessage::Engine {
                run_id: first,
                event: EngineEvent::SyncPrepared(0, first_plan),
            })
            .unwrap();
        app.drain_messages();

        assert_eq!(app.states[0].label(), "queued");
        assert_eq!(app.states[1].label(), "pulling");
        assert!(app.marked.contains("zulu"));

        app.sender
            .send(UiMessage::SyncComplete {
                run_id: first,
                results: vec![sync_result(&app.repositories[0], SyncKind::Updated)],
            })
            .unwrap();
        app.drain_messages();

        assert!(matches!(app.states[0], RowState::Finished(_)));
        assert_eq!(app.states[1].label(), "pulling");
        assert!(app.sync_batches.contains_key(&second));
        assert!(app.marked.contains("zulu"));
        assert!(app.status.starts_with("sync 1/2:"));
    }

    #[test]
    fn already_active_repositories_are_filtered_from_new_batches() {
        let mut app = app_with_repositories(&["one", "two"]);
        let first = app.reserve_sync_batch(vec![0]);
        let (available, skipped) = app.available_sync_indices(vec![0, 1]);

        assert_eq!(available, vec![1]);
        assert_eq!(skipped, 1);
        assert_eq!(
            app.active_repositories.get(&app.repository_key(0)),
            Some(&first)
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_aliases_share_the_same_active_lock() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let repository_path = directory.path().join("repository");
        let alias_path = directory.path().join("alias");
        let replacement_path = directory.path().join("replacement");
        std::fs::create_dir(&repository_path).unwrap();
        symlink(&repository_path, &alias_path).unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store
            .save(&[
                Repository {
                    name: "canonical".to_owned(),
                    path: repository_path.clone(),
                    strategy: Strategy::Rebase,
                    submodules: false,
                },
                Repository {
                    name: "alias".to_owned(),
                    path: alias_path.clone(),
                    strategy: Strategy::Rebase,
                    submodules: false,
                },
            ])
            .unwrap();
        let mut app = App::new(
            store,
            Settings {
                jobs: 2,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let canonical_index = app
            .repositories
            .iter()
            .position(|repository| repository.name == "canonical")
            .unwrap();
        let alias_index = app
            .repositories
            .iter()
            .position(|repository| repository.name == "alias")
            .unwrap();

        let (available, skipped) = app.available_sync_indices(vec![canonical_index, alias_index]);

        assert_eq!(available.len(), 1);
        assert_eq!(skipped, 1);
        let run_id = app.reserve_sync_batch(available);

        let (available, skipped) = app.available_sync_indices(vec![canonical_index, alias_index]);

        assert!(available.is_empty());
        assert_eq!(skipped, 2);

        std::fs::remove_file(&alias_path).unwrap();
        std::fs::create_dir(&replacement_path).unwrap();
        symlink(&replacement_path, &alias_path).unwrap();
        app.release_sync_batch(run_id);

        assert!(app.active_repositories.is_empty());
    }

    #[test]
    fn start_sync_accepts_a_new_repository_while_another_batch_runs() {
        let mut app = app_with_repositories(&["one", "two"]);
        app.table_state.select(Some(0));
        app.start_sync(false);
        app.table_state.select(Some(1));
        app.start_sync(false);

        assert_eq!(app.sync_batches.len(), 2);
        assert_eq!(app.active_repositories.len(), 2);

        app.table_state.select(Some(0));
        app.start_sync(false);
        assert_eq!(app.sync_batches.len(), 2);
        assert_eq!(app.status, "Selected repositories are already syncing");
    }

    #[test]
    fn quit_waits_for_every_sync_batch() {
        let mut app = app_with_repositories(&["one", "two"]);
        let first = app.reserve_sync_batch(vec![0]);
        let second = app.reserve_sync_batch(vec![1]);
        app.request_quit();

        app.sender
            .send(UiMessage::SyncComplete {
                run_id: first,
                results: vec![sync_result(&app.repositories[0], SyncKind::Skipped)],
            })
            .unwrap();
        app.drain_messages();
        assert!(!app.quit);

        app.sender
            .send(UiMessage::SyncComplete {
                run_id: second,
                results: vec![sync_result(&app.repositories[1], SyncKind::Skipped)],
            })
            .unwrap();
        app.drain_messages();
        assert!(app.quit);
        assert_eq!(app.status, "Sync complete: 0 updated, 2 skipped, 0 failed");
    }

    #[test]
    fn failed_batch_stays_visible_while_another_batch_runs() {
        let mut app = app_with_repositories(&["one", "two"]);
        let first = app.reserve_sync_batch(vec![0]);
        let second = app.reserve_sync_batch(vec![1]);
        app.sender
            .send(UiMessage::OperationFailed {
                run_id: first,
                operation: "Sync",
                detail: "worker failed".to_owned(),
            })
            .unwrap();
        app.drain_messages();

        assert!(matches!(
            &app.states[0],
            RowState::Finished(result)
                if result.kind == SyncKind::Failed && result.detail == "worker failed"
        ));
        assert!(app.status.starts_with("sync 1/2:"));

        app.sender
            .send(UiMessage::SyncComplete {
                run_id: second,
                results: vec![sync_result(&app.repositories[1], SyncKind::Skipped)],
            })
            .unwrap();
        app.drain_messages();
        assert_eq!(app.status, "Sync complete: 0 updated, 1 skipped, 1 failed");
    }

    #[test]
    fn renders_empty_and_narrow_terminals_without_panicking() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        for (width, height) in [(120, 40), (80, 24), (40, 10)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            if width == 40 {
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(rendered.contains("q"));
            }
        }
    }

    #[test]
    fn completion_is_visible_only_after_every_batch_finishes() {
        let mut app = app_with_repositories(&["one", "two"]);
        let first = app.reserve_sync_batch(vec![0]);
        let second = app.reserve_sync_batch(vec![1]);
        app.sender
            .send(UiMessage::SyncComplete {
                run_id: first,
                results: vec![sync_result(&app.repositories[0], SyncKind::Updated)],
            })
            .unwrap();
        app.drain_messages();
        assert!(app.completion.is_none());
        app.sender
            .send(UiMessage::SyncComplete {
                run_id: second,
                results: vec![sync_result(&app.repositories[1], SyncKind::Failed)],
            })
            .unwrap();
        app.drain_messages();
        assert!(app.completion.as_ref().unwrap().failed);

        for color in [true, false] {
            app.color_enabled = color;
            for (width, height) in [(120, 24), (40, 12)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let lines: Vec<String> = terminal
                    .backend()
                    .buffer()
                    .content()
                    .chunks(usize::from(width))
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect();
                let text = lines.join("\n");
                assert!(text.contains("SYNC ENDED WITH ERRORS"));
                assert!(
                    text.split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .contains("2/2 repos")
                );
                assert!(text.contains("q quit"));
                if !color && width == 40 {
                    println!("{text}");
                }
            }
        }
        app.reserve_sync_batch(vec![0]);
        assert!(app.completion.is_none());
    }

    #[test]
    fn overview_merges_counts_and_uses_dark_text_for_errors() {
        let mut app = app_with_repositories(&["one", "two"]);
        app.color_enabled = true;
        app.reserve_sync_batch(vec![0, 1]);
        app.states[0] = RowState::Planning;
        app.update_sync_status();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = buffer.content()[240..360]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(line.contains("FETCHING  0/2 (0%)"));
        assert!(line.contains("1 waiting, 1 fetching, 0 queued, 0 pulling"));
        assert!(!line.contains("sync 0/2"));
        assert_eq!(buffer[(80, 2)].bg, Color::Cyan);
        assert_eq!(buffer[(1, 3)].bg, Color::Reset);
        app.sync_batches.clear();
        app.sync_totals.finished = 2;
        app.sync_totals.failed = 2;
        app.finish_sync();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = buffer.content()[240..360]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(line.contains("SYNC ENDED WITH ERRORS") && line.contains("2 failed"));
        assert_eq!(buffer[(1, 2)].fg, Color::Black);
        assert_eq!(buffer[(1, 2)].bg, Color::LightRed);
    }

    #[test]
    fn overview_bar_distinguishes_stages_and_keeps_progress_without_color() {
        fn bar(app: &mut App) -> (String, Color, Modifier) {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| render(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            let text = buffer.content()[160..240]
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            (text, buffer[(1, 2)].bg, buffer[(1, 2)].modifier)
        }
        let mut app = app_with_repositories(&["one", "two"]);
        app.color_enabled = true;
        assert_eq!(bar(&mut app).1, Color::DarkGray);
        app.reserve_sync_batch(vec![0, 1]);
        app.sync_totals.finished = 1;
        for (state, label, color) in [
            (RowState::Waiting, "WAITING", Color::DarkGray),
            (RowState::Planning, "FETCHING", Color::Cyan),
            (RowState::Prepared(String::new()), "QUEUED", Color::Blue),
            (RowState::Updating(String::new()), "SYNCING", Color::Magenta),
        ] {
            app.states[0] = state;
            let (text, background, _) = bar(&mut app);
            assert!(text.contains(label) && text.contains("1/2 (50%)"));
            assert_eq!(background, color);
        }
        app.color_enabled = false;
        let (text, background, modifiers) = bar(&mut app);
        assert!(text.contains("SYNCING  1/2 (50%)"));
        assert_eq!(background, Color::Reset);
        assert!(modifiers.contains(Modifier::REVERSED));
        app.color_enabled = true;
        app.sync_totals.finished = 2;
        assert!(bar(&mut app).0.contains("FINISHING"));
        app.sync_batches.clear();
        app.finish_sync();
        assert_eq!(bar(&mut app).1, Color::Green);
        app.sync_totals.failed = 1;
        app.finish_sync();
        assert_eq!(bar(&mut app).1, Color::LightRed);
        app.completion = None;
        app.refresh_run = Some(3);
        assert!(bar(&mut app).0.contains("REFRESHING"));
        app.quit_pending = true;
        assert_eq!(bar(&mut app).1, Color::Yellow);
    }

    #[test]
    fn tall_terminal_keeps_progress_and_details_next_to_repositories() {
        let mut app = app_with_repositories(&["alpha", "beta", "gamma"]);
        app.reserve_sync_batch(vec![0, 1, 2]);
        app.update_sync_status();
        let mut terminal = Terminal::new(TestBackend::new(120, 80)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let progress = lines
            .iter()
            .position(|line| line.contains("WAITING  0/3"))
            .unwrap();
        let last_repo = lines
            .iter()
            .position(|line| line.contains("gamma"))
            .unwrap();
        let details = lines
            .iter()
            .position(|line| line.contains("details"))
            .unwrap();
        assert!(progress < 5);
        assert!(details > last_repo && details <= last_repo + 2);
        assert!(
            lines
                .iter()
                .position(|line| line.contains("q quit"))
                .unwrap()
                < 15
        );
        println!("{}", lines[..13].join("\n"));
    }

    #[test]
    fn long_repository_list_scrolls_beneath_the_overview() {
        let names: Vec<_> = (0..60).map(|index| format!("repo-{index:02}")).collect();
        let mut app = app_with_repositories(&names.iter().map(String::as_str).collect::<Vec<_>>());
        app.table_state.select(Some(59));
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(lines[..5].iter().any(|line| line.contains("READY")));
        assert!(lines.iter().any(|line| line.contains("repo-59")));
        assert!(lines.iter().any(|line| line.contains("details")));
        assert!(lines.iter().any(|line| line.contains("q quit")));
    }

    #[test]
    fn main_view_has_no_panel_borders() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!rendered.contains(['┌', '┐', '└', '┘', '│', '─']));
    }

    #[test]
    fn renders_unicode_repository_fields() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        store
            .save(&[Repository {
                name: "项目-很长的名字".to_owned(),
                path: PathBuf::from("/tmp/代码 仓库"),
                strategy: Strategy::Merge,
                submodules: true,
            }])
            .unwrap();
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        app.states[0] = RowState::Ready(Inspection {
            branch: Some("功能/测试".to_owned()),
            updates: Some(2),
            ..Inspection::default()
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn quit_during_work_is_deferred_until_completion() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::at(directory.path().join("repos.tsv"));
        let mut app = App::new(
            store,
            Settings {
                jobs: 1,
                fetch_attempts: 1,
                terminal_prompt: false,
                command_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let run_id = app.allocate_run_id();
        app.refresh_run = Some(run_id);
        app.request_quit();
        assert!(app.quit_pending);
        assert!(!app.quit);
        app.sender
            .send(UiMessage::InspectionComplete {
                run_id,
                results: Vec::new(),
            })
            .unwrap();
        app.drain_messages();
        assert!(app.quit);
    }
}

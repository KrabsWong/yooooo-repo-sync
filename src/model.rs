use std::fmt;
use std::path::PathBuf;

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Strategy {
    #[default]
    Rebase,
    Merge,
}

impl Strategy {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "rebase" => Ok(Self::Rebase),
            "merge" => Ok(Self::Merge),
            _ => anyhow::bail!("strategy must be rebase or merge"),
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Rebase => Self::Merge,
            Self::Merge => Self::Rebase,
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
    pub strategy: Strategy,
    pub submodules: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inspection {
    pub branch: Option<String>,
    pub compare_ref: Option<String>,
    pub updates: Option<u64>,
    pub dirty: bool,
    pub error: Option<String>,
}

impl Inspection {
    pub fn label(&self) -> &'static str {
        if self.error.is_some() {
            "unknown"
        } else if self.dirty {
            "dirty"
        } else if self.updates.unwrap_or_default() > 0 {
            "yes"
        } else {
            "no"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncKind {
    Updated,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncResult {
    pub repository: Repository,
    pub kind: SyncKind,
    pub dirty: bool,
    pub detail: String,
}

mod cli;
mod config;
mod engine;
mod git;
mod model;
mod tui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command};
use crate::config::ConfigStore;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<bool> {
    let cli = Cli::parse();
    let store = ConfigStore::discover()?;
    match cli.command {
        Some(command) => cli::run(command, &store),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            cli::run(Command::Tui, &store)
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(false)
        }
    }
}

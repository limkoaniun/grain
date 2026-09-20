//! grain: a terminal spaced-repetition app over a markdown vault.
//!
//! `grain --vault <path>` (default `./vault`). M0 is fully offline.

mod app;
mod db;
mod ui;
mod vault;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::app::App;

const USAGE: &str = "usage: grain [--vault <path>]\n\n  --vault <path>   vault directory (default ./vault)\n  -h, --help       show this help";

struct Args {
    vault: PathBuf,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut vault = PathBuf::from("vault");
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--vault" | "-v" => {
                vault = it
                    .next()
                    .map(PathBuf::from)
                    .context("--vault needs a path")?;
            }
            "-h" | "--help" => return Ok(None),
            other if other.starts_with("--vault=") => {
                vault = PathBuf::from(&other["--vault=".len()..]);
            }
            other => bail!("unknown argument `{other}`\n{USAGE}"),
        }
    }
    Ok(Some(Args { vault }))
}

fn main() -> Result<()> {
    let Some(args) = parse_args(std::env::args().skip(1))? else {
        println!("{USAGE}");
        return Ok(());
    };
    if !args.vault.is_dir() {
        bail!("vault `{}` is not a directory", args.vault.display());
    }
    let today = chrono::Local::now().date_naive();
    let mut app = App::open(&args.vault, today)
        .with_context(|| format!("opening vault {}", args.vault.display()))?;
    for (path, reason) in &app.refresh.skipped {
        eprintln!("grain: skipped {path}: {reason}");
    }
    ratatui::run(|terminal| run_loop(terminal, &mut app))
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                app.handle_key(key.code)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Args>> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn default_vault_is_dot_vault() {
        assert_eq!(parse(&[]).unwrap().unwrap().vault, PathBuf::from("vault"));
    }

    #[test]
    fn vault_flag_in_both_forms() {
        assert_eq!(parse(&["--vault", "x/y"]).unwrap().unwrap().vault, PathBuf::from("x/y"));
        assert_eq!(parse(&["--vault=z"]).unwrap().unwrap().vault, PathBuf::from("z"));
    }

    #[test]
    fn help_and_bad_args() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--vault"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}

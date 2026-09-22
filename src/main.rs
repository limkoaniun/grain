//! grain: a terminal spaced-repetition app over a markdown vault.
//!
//! `grain --vault <path>` (default `./vault`). With `GRAIN_SM_API_KEY` set, grades are
//! synced to the SuperMemo API from a worker thread; without it grain is fully offline.

mod app;
mod db;
mod sync;
mod ui;
mod vault;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::app::App;
use crate::sync::api::{Scheduler, UreqScheduler, API_KEY_ENV};

const USAGE: &str = "usage: grain [--vault <path>] [--auth-check]\n\n  --vault <path>   vault directory (default ./vault)\n  --auth-check     check GRAIN_SM_API_KEY against the SuperMemo API and exit\n  -h, --help       show this help";

/// How long the event loop waits for a key before running `App::tick`.
const TICK: Duration = Duration::from_millis(100);

struct Args {
    vault: PathBuf,
    auth_check: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut vault = PathBuf::from("vault");
    let mut auth_check = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--vault" | "-v" => {
                vault = it
                    .next()
                    .map(PathBuf::from)
                    .context("--vault needs a path")?;
            }
            "--auth-check" => auth_check = true,
            "-h" | "--help" => return Ok(None),
            other if other.starts_with("--vault=") => {
                vault = PathBuf::from(&other["--vault=".len()..]);
            }
            other => bail!("unknown argument `{other}`\n{USAGE}"),
        }
    }
    Ok(Some(Args { vault, auth_check }))
}

/// The API key from the environment, or `None` for offline mode.
fn api_key() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn main() -> Result<()> {
    let Some(args) = parse_args(std::env::args().skip(1))? else {
        println!("{USAGE}");
        return Ok(());
    };
    if !args.vault.is_dir() {
        bail!("vault `{}` is not a directory", args.vault.display());
    }
    if args.auth_check {
        return auth_check(&args.vault);
    }
    let today = chrono::Local::now().date_naive();
    let mut app = match api_key() {
        Some(key) => App::open_with_scheduler(&args.vault, today, Box::new(UreqScheduler::new(&key))),
        None => App::open(&args.vault, today),
    }
    .with_context(|| format!("opening vault {}", args.vault.display()))?;
    for (path, reason) in &app.refresh.skipped {
        eprintln!("grain: skipped {path}: {reason}");
    }
    ratatui::run(|terminal| run_loop(terminal, &mut app))?;
    // Terminal is restored; anything still waiting on the network gets a few seconds.
    let pending = app.finish()?;
    for line in &app.sync_log {
        eprintln!("grain: {line}");
    }
    if pending > 0 {
        eprintln!("grain: {pending} grades pending sync");
    }
    Ok(())
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app))?;
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key_with(key.code, key.modifiers)?;
                }
            }
        }
        app.tick(Instant::now(), chrono::Local::now().date_naive())?;
    }
    Ok(())
}

/// `grain --auth-check`: call `/auth/me` with the configured key and report, no TUI.
fn auth_check(vault: &std::path::Path) -> Result<()> {
    let Some(key) = api_key() else {
        bail!("{API_KEY_ENV} is not set");
    };
    let db = db::Db::open(&vault.join(".grain").join("grain.db"))?;
    let learner = db.meta_i64("sm_learner_id", 1)?;
    let collection = db.meta_i64("sm_collection_id", 1)?;
    let me = UreqScheduler::new(&key)
        .whoami()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("auth check failed")?;
    println!(
        "project {} · key {} · learner {learner} · collection {collection}",
        me.project_id, me.api_key_id
    );
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
        let args = parse(&[]).unwrap().unwrap();
        assert_eq!(args.vault, PathBuf::from("vault"));
        assert!(!args.auth_check);
    }

    #[test]
    fn vault_flag_in_both_forms() {
        assert_eq!(parse(&["--vault", "x/y"]).unwrap().unwrap().vault, PathBuf::from("x/y"));
        assert_eq!(parse(&["--vault=z"]).unwrap().unwrap().vault, PathBuf::from("z"));
    }

    #[test]
    fn auth_check_flag() {
        let args = parse(&["--auth-check", "--vault", "v"]).unwrap().unwrap();
        assert!(args.auth_check);
        assert_eq!(args.vault, PathBuf::from("v"));
    }

    #[test]
    fn help_and_bad_args() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--vault"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}

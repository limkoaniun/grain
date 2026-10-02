//! User config (M10): `~/.config/grain/config`, `key = value` lines, SuperMemo's ini in spirit.
//!
//! One line per setting, `#` comments and blank lines allowed, unknown keys kept and
//! ignored. A bad value for a known key fails the whole load naming the line, because the
//! file is short and hand-edited: silently ignoring a typo would hide it. Everything here
//! is pure except `load`, `save` and the two env-reading wrappers `path` and `vault_path`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::postpone::{Postpone, DEFAULT_KEEP};

/// What the end of the main pass does with the cards that failed: ask, always drill, never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalDrill {
    Ask,
    On,
    Off,
}

impl FinalDrill {
    /// The next mode in the settings-screen cycle `ask → on → off → ask`.
    pub fn next(self) -> FinalDrill {
        match self {
            FinalDrill::Ask => FinalDrill::On,
            FinalDrill::On => FinalDrill::Off,
            FinalDrill::Off => FinalDrill::Ask,
        }
    }

    /// The value as it is written in the file and shown on the settings screen.
    pub fn as_str(self) -> &'static str {
        match self {
            FinalDrill::Ask => "ask",
            FinalDrill::On => "on",
            FinalDrill::Off => "off",
        }
    }

    /// Parses a file or prompt value; `None` when it is not one of the three words.
    fn parse(text: &str) -> Option<FinalDrill> {
        match text {
            "ask" => Some(FinalDrill::Ask),
            "on" => Some(FinalDrill::On),
            "off" => Some(FinalDrill::Off),
            _ => None,
        }
    }
}

/// A setting, in the order the file appends and the settings screen lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Vault,
    Postpone,
    FinalDrill,
    Collection,
}

impl Key {
    /// Every key, in file and screen order.
    pub const ALL: [Key; 4] = [Key::Vault, Key::Postpone, Key::FinalDrill, Key::Collection];

    /// The key as it is spelled in the file.
    pub fn name(self) -> &'static str {
        match self {
            Key::Vault => "vault",
            Key::Postpone => "postpone",
            Key::FinalDrill => "final_drill",
            Key::Collection => "collection",
        }
    }

    /// The key as it is spelled on screen and in a prompt label.
    pub fn label(self) -> &'static str {
        match self {
            Key::Vault => "vault",
            Key::Postpone => "postpone",
            Key::FinalDrill => "final drill",
            Key::Collection => "collection",
        }
    }
}

/// The known key a file line names, if any. Anything else is an unknown key, kept as it is.
fn key_named(name: &str) -> Option<Key> {
    Key::ALL.into_iter().find(|key| key.name() == name)
}

/// The four settings, already validated. `vault` stays the text as typed (`~/notes`);
/// `vault_path` expands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub vault: String,
    pub postpone: Postpone,
    pub final_drill: FinalDrill,
    pub collection: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            vault: "vault".to_string(),
            postpone: Postpone::Keep(DEFAULT_KEEP),
            final_drill: FinalDrill::Ask,
            collection: "all".to_string(),
        }
    }
}

impl Config {
    /// Reads `path`; a missing file is the defaults, a bad value is an error naming the line.
    ///
    /// The first line of a known key wins; later duplicates are ignored, as on save.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("read config {}", path.display()));
            }
        };
        let mut config = Config::default();
        let mut seen: Vec<Key> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            match parse_line(line) {
                Line::Blank | Line::Comment => {}
                Line::Bad => bail!("config {}: line {n}: expected key = value", path.display()),
                Line::Pair { key, value } => {
                    let Some(key) = key_named(key) else { continue };
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.push(key);
                    config
                        .set(key, value)
                        .map_err(|e| anyhow!("config {}: line {n}: {e}", path.display()))?;
                }
            }
        }
        Ok(config)
    }

    /// Writes the four settings into `path`, keeping every other line where it is.
    ///
    /// The first line of each known key is rewritten, later duplicates are dropped, keys
    /// with no line are appended in `Key::ALL` order, and the file ends with one newline.
    pub fn save(&self, path: &Path) -> Result<()> {
        let existing = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("read config {}", path.display()));
            }
        };
        let mut out: Vec<String> = Vec::new();
        let mut written: Vec<Key> = Vec::new();
        for line in existing.lines() {
            match parse_line(line) {
                Line::Blank | Line::Comment | Line::Bad => out.push(line.to_string()),
                Line::Pair { key, .. } => match key_named(key) {
                    None => out.push(line.to_string()),
                    Some(key) if written.contains(&key) => {}
                    Some(key) => {
                        written.push(key);
                        out.push(self.line_for(key));
                    }
                },
            }
        }
        for key in Key::ALL {
            if !written.contains(&key) {
                out.push(self.line_for(key));
            }
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        let mut text = out.join("\n");
        text.push('\n');
        fs::write(path, text).with_context(|| format!("write config {}", path.display()))
    }

    /// The file line for one key: `{name} = {value}`.
    fn line_for(&self, key: Key) -> String {
        format!("{} = {}", key.name(), self.get(key))
    }

    /// Validates `text` and stores it. The error is the whole reason, with no line prefix:
    /// `load` adds the line, the settings screen shows it as `config · {reason}`.
    pub fn set(&mut self, key: Key, text: &str) -> Result<()> {
        let text = text.trim();
        match key {
            Key::Vault => {
                if text.is_empty() {
                    bail!("vault must not be empty");
                }
                self.vault = text.to_string();
            }
            Key::Postpone => {
                self.postpone = if text == "off" {
                    Postpone::Off
                } else {
                    match text.parse::<usize>() {
                        Ok(n) => Postpone::Keep(n),
                        Err(_) => bail!("postpone expects a count or off"),
                    }
                };
            }
            Key::FinalDrill => {
                let Some(mode) = FinalDrill::parse(text) else {
                    bail!("final drill expects ask, on or off");
                };
                self.final_drill = mode;
            }
            Key::Collection => {
                let len = text.chars().count();
                if !(1..=40).contains(&len) || text.contains('\n') {
                    bail!("collection must be 1 to 40 characters");
                }
                self.collection = text.to_string();
            }
        }
        Ok(())
    }

    /// One setting as text, for the file, a prefilled prompt and the settings row.
    pub fn get(&self, key: Key) -> String {
        match key {
            Key::Vault => self.vault.clone(),
            Key::Postpone => match self.postpone {
                Postpone::Off => "off".to_string(),
                Postpone::Keep(n) => n.to_string(),
            },
            Key::FinalDrill => self.final_drill.as_str().to_string(),
            Key::Collection => self.collection.clone(),
        }
    }

    /// The vault directory to open, with a leading `~/` expanded through `HOME`.
    pub fn vault_path(&self) -> PathBuf {
        self.vault_path_with(std::env::var("HOME").ok().as_deref())
    }

    /// `vault_path` with `HOME` supplied: `~/x` under `home`, anything else as typed.
    pub fn vault_path_with(&self, home: Option<&str>) -> PathBuf {
        match (self.vault.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => PathBuf::from(&self.vault),
        }
    }
}

/// Where the config file lives: `GRAIN_CONFIG`, else `XDG_CONFIG_HOME`, else `HOME`.
pub fn path() -> Option<PathBuf> {
    let grain_config = std::env::var("GRAIN_CONFIG").ok();
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    path_from(grain_config.as_deref(), xdg.as_deref(), home.as_deref())
}

/// `path` with the three variables supplied; an unset variable and an empty one are the same.
pub fn path_from(
    grain_config: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let set = |value: Option<&str>| value.filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(explicit) = set(grain_config) {
        return Some(explicit);
    }
    if let Some(xdg) = set(xdg) {
        return Some(xdg.join("grain").join("config"));
    }
    Some(set(home)?.join(".config").join("grain").join("config"))
}

/// One line of the file, as read.
enum Line<'a> {
    Blank,
    Comment,
    Pair { key: &'a str, value: &'a str },
    Bad,
}

/// Classifies one line: blank, `#` comment, `key = value` (both trimmed), or neither.
fn parse_line(line: &str) -> Line<'_> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Blank;
    }
    if trimmed.starts_with('#') {
        return Line::Comment;
    }
    match trimmed.split_once('=') {
        Some((key, value)) => Line::Pair {
            key: key.trim(),
            value: value.trim(),
        },
        None => Line::Bad,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Writes `text` as a config file in `dir` and returns its path.
    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The Display of the error `load` gives for `text`.
    fn load_err(dir: &Path, name: &str, text: &str) -> String {
        let path = write(dir, name, text);
        format!("{}", Config::load(&path).unwrap_err())
    }

    #[test]
    fn missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("config")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.vault, "vault");
        assert_eq!(config.postpone, Postpone::Keep(50));
        assert_eq!(config.final_drill, FinalDrill::Ask);
        assert_eq!(config.collection, "all");
    }

    #[test]
    fn round_trips_the_four_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut config = Config::default();
        config.set(Key::Vault, "~/notes").unwrap();
        config.set(Key::Postpone, "off").unwrap();
        config.set(Key::FinalDrill, "on").unwrap();
        config.set(Key::Collection, "citrus").unwrap();
        config.save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "vault = ~/notes\npostpone = off\nfinal_drill = on\ncollection = citrus\n"
        );
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn save_keeps_unknown_lines_comments_and_blanks_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config", "# my config\n\ntheme = dark\ncollection = old\n");
        let mut config = Config::load(&path).unwrap();
        assert_eq!(config.collection, "old");
        config.set(Key::Collection, "new").unwrap();
        config.save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# my config\n\ntheme = dark\ncollection = new\nvault = vault\npostpone = 50\nfinal_drill = ask\n"
        );
    }

    #[test]
    fn duplicate_known_key_lines_collapse_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config", "collection = a\ncollection = b\n");
        let mut config = Config::load(&path).unwrap();
        assert_eq!(config.collection, "a");
        config.set(Key::Collection, "c").unwrap();
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("collection")).collect::<Vec<_>>(),
            ["collection = c"]
        );
        assert_eq!(text, "collection = c\nvault = vault\npostpone = 50\nfinal_drill = ask\n");
    }

    #[test]
    fn bad_values_name_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let postpone = load_err(dir, "a", "# a comment\npostpone = fifty\n");
        assert!(postpone.starts_with("config "), "{postpone}");
        assert!(postpone.contains("line 2: postpone expects a count or off"), "{postpone}");
        let drill = load_err(dir, "b", "final_drill = maybe\n");
        assert!(drill.contains("line 1: final drill expects ask, on or off"), "{drill}");
        let collection = load_err(dir, "c", "collection = \n");
        assert!(collection.contains("line 1: collection must be 1 to 40 characters"), "{collection}");
        let vault = load_err(dir, "d", "vault = \n");
        assert!(vault.contains("line 1: vault must not be empty"), "{vault}");
        let nonsense = load_err(dir, "e", "vault = v\nnonsense\n");
        assert!(nonsense.contains("line 2: expected key = value"), "{nonsense}");
    }

    #[test]
    fn vault_path_expands_a_leading_tilde() {
        let mut config = Config::default();
        config.set(Key::Vault, "~/notes").unwrap();
        assert_eq!(config.vault_path_with(Some("/home/u")), PathBuf::from("/home/u/notes"));
        assert_eq!(config.vault_path_with(None), PathBuf::from("~/notes"));
        config.set(Key::Vault, "/abs").unwrap();
        assert_eq!(config.vault_path_with(Some("/home/u")), PathBuf::from("/abs"));
        config.set(Key::Vault, "vault").unwrap();
        assert_eq!(config.vault_path_with(Some("/home/u")), PathBuf::from("vault"));
    }

    #[test]
    fn path_from_precedence() {
        assert_eq!(
            path_from(Some("/c"), Some("/x"), Some("/h")),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            path_from(None, Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/grain/config"))
        );
        assert_eq!(
            path_from(Some(""), None, Some("/h")),
            Some(PathBuf::from("/h/.config/grain/config"))
        );
        assert_eq!(path_from(None, None, None), None);
    }

    #[test]
    fn final_drill_cycles() {
        assert_eq!(FinalDrill::Ask.next(), FinalDrill::On);
        assert_eq!(FinalDrill::On.next(), FinalDrill::Off);
        assert_eq!(FinalDrill::Off.next(), FinalDrill::Ask);
        assert_eq!(FinalDrill::Ask.as_str(), "ask");
        assert_eq!(FinalDrill::On.as_str(), "on");
        assert_eq!(FinalDrill::Off.as_str(), "off");
    }

    #[test]
    fn get_formats_each_key() {
        let mut config = Config::default();
        assert_eq!(config.get(Key::Postpone), "50");
        config.set(Key::Vault, "~/notes").unwrap();
        config.set(Key::Postpone, "off").unwrap();
        config.set(Key::FinalDrill, "on").unwrap();
        config.set(Key::Collection, "citrus").unwrap();
        assert_eq!(config.get(Key::Vault), "~/notes");
        assert_eq!(config.get(Key::Postpone), "off");
        assert_eq!(config.get(Key::FinalDrill), "on");
        assert_eq!(config.get(Key::Collection), "citrus");
    }
}

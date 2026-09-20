//! Keep the SQLite index in step with the vault.
//!
//! Refresh = scan, skip files whose path+mtime already match, upsert the rest
//! (allocating and writing back missing sm_ids), then drop rows for files that
//! no longer exist. A rename shows up as a known sm_id at a new path.
//!
//! This module is the only writer of vault files, and it writes two things:
//! a missing `sm_id` on first index, and `due`/`interval` after a sync.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;

use crate::db::{Db, ItemRow};
use crate::vault::card::{parse_card_body, CardBody};
use crate::vault::frontmatter::{Document, ItemMeta, ItemType};
use crate::vault::scan::{mtime_of, scan_vault, ScannedFile};

/// What a refresh did, for the status line and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub unchanged: usize,
    pub indexed: usize,
    pub allocated: usize,
    pub removed: usize,
    /// Files grain could not index, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// A fully parsed card ready for review.
#[derive(Debug, Clone)]
pub struct LoadedCard {
    pub meta: ItemMeta,
    pub body: CardBody,
}

/// Bring the index in step with the files under `root`.
pub fn refresh(root: &Path, db: &Db) -> Result<RefreshReport> {
    let files = scan_vault(root)?;
    let known = db.path_index()?;
    let mut report = RefreshReport::default();
    let mut present: Vec<String> = Vec::with_capacity(files.len());
    // sm_ids seen this scan, so a duplicated id cannot flip-flop between two paths.
    let mut seen_ids: HashSet<i64> = known.values().map(|(id, _)| *id).collect();
    let mut claimed_ids: HashSet<i64> = HashSet::new();

    for file in &files {
        if let Some((sm_id, mtime)) = known.get(&file.rel_path) {
            if *mtime == file.mtime {
                report.unchanged += 1;
                present.push(file.rel_path.clone());
                claimed_ids.insert(*sm_id);
                continue;
            }
        }
        match index_file(db, file, &mut seen_ids, &mut claimed_ids) {
            Ok(allocated) => {
                report.indexed += 1;
                if allocated {
                    report.allocated += 1;
                }
                present.push(file.rel_path.clone());
            }
            Err(e) => report.skipped.push((file.rel_path.clone(), format!("{e:#}"))),
        }
    }

    report.removed = db.delete_missing(&present)?;
    Ok(report)
}

/// Parse one file and upsert it. Returns whether an sm_id was allocated.
fn index_file(
    db: &Db,
    file: &ScannedFile,
    seen_ids: &mut HashSet<i64>,
    claimed_ids: &mut HashSet<i64>,
) -> Result<bool> {
    let text = std::fs::read_to_string(&file.abs_path)
        .with_context(|| format!("reading {}", file.rel_path))?;
    let mut doc = Document::parse(&text)?;
    let mut meta = doc.meta()?;
    let mut mtime = file.mtime;
    let mut allocated = false;

    let sm_id = match meta.sm_id {
        Some(id) => {
            if claimed_ids.contains(&id) {
                bail!("sm_id {id} is already used by another file in this vault");
            }
            id
        }
        None => {
            let id = allocate_unseen(db, seen_ids)?;
            doc.set_sm_id(id);
            let out = doc.serialize()?;
            std::fs::write(&file.abs_path, out)
                .with_context(|| format!("writing sm_id back to {}", file.rel_path))?;
            mtime = mtime_of(&file.abs_path)?;
            meta.sm_id = Some(id);
            allocated = true;
            id
        }
    };
    seen_ids.insert(sm_id);
    claimed_ids.insert(sm_id);

    let title = match meta.kind {
        ItemType::Card => parse_card_body(&doc.body)?.title(),
        ItemType::Article => article_title(&doc.body),
    }
    .or_else(|| Some(file_stem(&file.rel_path)));

    db.upsert_item(&ItemRow {
        sm_id,
        path: file.rel_path.clone(),
        kind: meta.kind,
        due: meta.due,
        interval: meta.interval,
        prio: meta.prio,
        read_pos: meta.read_pos,
        tags: meta.tags.join(","),
        mtime,
        title,
    })?;
    Ok(allocated)
}

fn allocate_unseen(db: &Db, seen_ids: &HashSet<i64>) -> Result<i64> {
    loop {
        let id = db.allocate_sm_id()?;
        if !seen_ids.contains(&id) {
            return Ok(id);
        }
    }
}

/// Write `due` and `interval` into a card's frontmatter after a sync.
/// Goes through [`Document::serialize`] so unknown keys survive. Returns the new mtime.
pub fn write_schedule(root: &Path, rel_path: &str, due: NaiveDate, interval: i64) -> Result<i64> {
    let abs = root.join(rel_path);
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {rel_path}"))?;
    let mut doc = Document::parse(&text).with_context(|| format!("parsing {rel_path}"))?;
    doc.set_schedule(due, interval);
    let out = doc.serialize().with_context(|| format!("serializing {rel_path}"))?;
    std::fs::write(&abs, out).with_context(|| format!("writing schedule to {rel_path}"))?;
    mtime_of(&abs)
}

/// Read and parse a card by its vault-relative path.
pub fn load_card(root: &Path, rel_path: &str) -> Result<LoadedCard> {
    let abs = root.join(rel_path);
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {rel_path}"))?;
    let doc = Document::parse(&text).with_context(|| format!("parsing {rel_path}"))?;
    let meta = doc.meta()?;
    if meta.kind != ItemType::Card {
        bail!("{rel_path} is not a card");
    }
    let body = parse_card_body(&doc.body).with_context(|| format!("parsing {rel_path}"))?;
    Ok(LoadedCard { meta, body })
}

/// First `# heading` of an article body, else its first non-empty line.
fn article_title(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines
        .iter()
        .find_map(|l| l.strip_prefix('#').map(|h| h.trim_start_matches('#').trim().to_string()))
        .filter(|t| !t.is_empty())
        .or_else(|| lines.first().map(|l| (*l).to_string()))
}

fn file_stem(rel_path: &str) -> String {
    Path::new(rel_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel_path.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::{Db, Schedule};
    use std::fs;
    use std::path::Path;

    fn write(root: &Path, name: &str, text: &str) {
        fs::write(root.join(name), text).unwrap();
    }

    fn bump_mtime(root: &Path, name: &str) {
        // Push the mtime forward by two seconds so a same-second rewrite is still a change.
        let f = fs::File::options().write(true).open(root.join(name)).unwrap();
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        f.set_modified(t).unwrap();
    }

    fn setup() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "---\ntype: card\nsm_id: 10\nprio: 30\n---\nQ: alpha?\nA: a\n");
        write(root, "b.md", "---\ntype: card\n---\nQ: beta?\nA: b\n");
        write(root, "art.md", "---\ntype: article\nprio: 20\n---\n# Article title\nbody\n");
        write(root, "bad.md", "---\ntype: recipe\n---\nnot ours\n");
        let db = Db::open(&root.join(".grain/grain.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn first_refresh_indexes_everything_and_allocates_sm_ids() {
        let (dir, db) = setup();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.indexed, 3);
        assert_eq!(report.allocated, 2);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].0 == "bad.md");

        let items = db.queue().unwrap();
        let ids: Vec<i64> = items.iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [1, 10, 2], "art prio 20, a prio 30, b prio 50");
        let art = &items[0];
        assert_eq!(art.title.as_deref(), Some("Article title"));
        let a = &items[1];
        assert_eq!(a.title.as_deref(), Some("alpha?"));

        // Written back into the files.
        let b = fs::read_to_string(dir.path().join("b.md")).unwrap();
        assert!(b.starts_with("---\ntype: card\nsm_id: 2\n---\n"), "{b}");
        assert_eq!(db.meta("next_sm_id").unwrap().as_deref(), Some("3"));
    }

    #[test]
    fn second_refresh_skips_unchanged_files_and_allocates_nothing() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        let before = fs::read_to_string(dir.path().join("b.md")).unwrap();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.indexed, 0);
        assert_eq!(report.unchanged, 3);
        assert_eq!(report.allocated, 0);
        assert_eq!(fs::read_to_string(dir.path().join("b.md")).unwrap(), before);
    }

    #[test]
    fn modified_file_is_reindexed() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        write(dir.path(), "a.md", "---\ntype: card\nsm_id: 10\nprio: 5\n---\nQ: alpha?\nA: a\n");
        bump_mtime(dir.path(), "a.md");
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(db.item(10).unwrap().unwrap().prio, 5);
    }

    #[test]
    fn rename_keeps_sm_id_and_journal() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        db.insert_grade(10, 4, "2026-09-20T10:00:00Z").unwrap();
        fs::rename(dir.path().join("a.md"), dir.path().join("renamed.md")).unwrap();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.removed, 0);
        let item = db.item(10).unwrap().unwrap();
        assert_eq!(item.path, "renamed.md");
        assert_eq!(db.journal_count(10).unwrap(), 1);
        assert_eq!(db.queue().unwrap().len(), 3);
    }

    #[test]
    fn deleted_file_row_is_removed_but_journal_stays() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        db.insert_grade(10, 4, "2026-09-20T10:00:00Z").unwrap();
        fs::remove_file(dir.path().join("a.md")).unwrap();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.removed, 1);
        assert!(db.item(10).unwrap().is_none());
        assert_eq!(db.journal_count(10).unwrap(), 1);
    }

    #[test]
    fn rebuilding_from_scratch_reproduces_the_queue() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        let before = db.queue().unwrap();
        drop(db);
        fs::remove_dir_all(dir.path().join(".grain")).unwrap();
        let db = Db::open(&dir.path().join(".grain/grain.db")).unwrap();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.allocated, 0);
        assert_eq!(db.queue().unwrap(), before);
    }

    #[test]
    fn duplicate_sm_id_across_files_is_skipped_not_flip_flopped() {
        let (dir, db) = setup();
        write(dir.path(), "dup.md", "---\ntype: card\nsm_id: 10\n---\nQ: dup?\nA: d\n");
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.skipped.len(), 2);
        assert_eq!(db.queue().unwrap().len(), 3);
    }

    #[test]
    fn write_schedule_rewrites_file_keeps_unknown_keys_and_returns_new_mtime() {
        let (dir, db) = setup();
        write(
            dir.path(),
            "a.md",
            "---\ntype: card\nsm_id: 10\nprio: 30\ncustom: keep\n---\nQ: alpha?\n\nA: a\n",
        );
        refresh(dir.path(), &db).unwrap();
        let before = db.item(10).unwrap().unwrap();
        let due = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let mtime = write_schedule(dir.path(), "a.md", due, 12).unwrap();
        let text = fs::read_to_string(dir.path().join("a.md")).unwrap();
        assert_eq!(
            text,
            "---\ntype: card\nsm_id: 10\ndue: 2026-10-02\ninterval: 12\nprio: 30\ncustom: keep\n---\nQ: alpha?\n\nA: a\n"
        );
        assert_eq!(mtime, mtime_of(&dir.path().join("a.md")).unwrap());
        assert!(mtime >= before.mtime);

        // Recording the new mtime in the index makes the next refresh skip the file.
        db.apply_sync(
            db.insert_grade(10, 4, "2026-09-20T10:00:00Z").unwrap(),
            12,
            Some(&Schedule { sm_id: 10, due, interval: 12, mtime }),
        )
        .unwrap();
        let report = refresh(dir.path(), &db).unwrap();
        assert_eq!(report.indexed, 0, "{report:?}");
        assert_eq!(report.unchanged, 3);
        assert_eq!(db.item(10).unwrap().unwrap().due, Some(due));
    }

    #[test]
    fn write_schedule_on_missing_file_names_the_path() {
        let (dir, _db) = setup();
        let due = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let err = write_schedule(dir.path(), "nope.md", due, 1).unwrap_err().to_string();
        assert!(err.contains("nope.md"), "{err}");
    }

    #[test]
    fn load_card_reads_document_and_body() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        let card = load_card(dir.path(), "a.md").unwrap();
        assert_eq!(card.meta.sm_id, Some(10));
        assert_eq!(card.body.title().as_deref(), Some("alpha?"));
    }
}

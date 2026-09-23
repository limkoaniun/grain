//! Keep the SQLite index in step with the vault.
//!
//! Refresh = scan, skip files whose path+mtime already match, upsert the rest
//! (allocating and writing back missing sm_ids), then drop rows for files that
//! no longer exist. A rename shows up as a known sm_id at a new path.
//!
//! This module is the only writer of vault files. It writes a missing `sm_id` on
//! first index (M0), `due`/`interval` after a sync (M1), and, from M2, an
//! article's session keys (`read_pos`, `due`, `interval`, `done`, `prio`) and new
//! child files under a folder named after their parent. From M5 it also writes
//! new top-level items (`a`/`i`) and copies a parent's `url`/`imported` onto
//! extract and cloze children.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;

use crate::db::{Db, ItemRow};
use crate::vault::article::Span;
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
        a_factor: meta.a_factor,
        done: meta.done,
        source: meta.source.as_deref().and_then(link_target),
        range: meta
            .range
            .as_deref()
            .and_then(Span::parse_range)
            .map(|r| (r.start as i64, r.end as i64)),
        url: meta.url.clone(),
        imported: meta.imported,
    })?;
    Ok(allocated)
}

/// The path a `source` wikilink points at: `[[citrus-vocab|alias]]` → `citrus-vocab`,
/// `[[citrus-vocab/1.md]]` → `citrus-vocab/1`. `None` when empty.
pub fn link_target(source: &str) -> Option<String> {
    let inner = source.trim().trim_start_matches("[[").trim_end_matches("]]");
    let name = inner.split('|').next().unwrap_or_default().trim();
    let name = name.strip_suffix(".md").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
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

/// Rewrite one file's frontmatter through `Document` and return the new mtime.
fn rewrite(root: &Path, rel_path: &str, edit: impl FnOnce(&mut Document)) -> Result<i64> {
    let abs = root.join(rel_path);
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {rel_path}"))?;
    let mut doc = Document::parse(&text).with_context(|| format!("parsing {rel_path}"))?;
    edit(&mut doc);
    let out = doc.serialize().with_context(|| format!("serializing {rel_path}"))?;
    std::fs::write(&abs, out).with_context(|| format!("writing {rel_path}"))?;
    mtime_of(&abs)
}

/// After a reading session: `read_pos`, `due` and `interval` in one rewrite.
pub fn write_article_session(root: &Path, rel_path: &str, read_pos: i64, due: NaiveDate, interval: i64) -> Result<i64> {
    rewrite(root, rel_path, |doc| {
        doc.set_read_pos(read_pos);
        doc.set_schedule(due, interval);
    })
}

pub fn write_read_pos(root: &Path, rel_path: &str, read_pos: i64) -> Result<i64> {
    rewrite(root, rel_path, |doc| doc.set_read_pos(read_pos))
}

pub fn write_prio(root: &Path, rel_path: &str, prio: i64) -> Result<i64> {
    rewrite(root, rel_path, |doc| doc.set_prio(prio))
}

pub fn write_done(root: &Path, rel_path: &str, done: NaiveDate) -> Result<i64> {
    rewrite(root, rel_path, |doc| doc.set_done(done))
}

/// Create a child of `parent` (an extract when `kind` is an article, a cloze when it
/// is a card) at `<parent stem>/<n>.md`, index it, and return its row. The parent
/// file is not touched. `body` is the whole body text; `range` the span of the
/// parent body it came from. `prio` is inherited; an extract also inherits an
/// explicit `a_factor`.
pub fn create_child(root: &Path, db: &Db, parent: &ItemRow, kind: ItemType, body: &str, range: Span) -> Result<ItemRow> {
    let stem = parent.path.strip_suffix(".md").unwrap_or(&parent.path);
    let dir = root.join(stem);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {stem}/"))?;
    let n = next_child_number(&dir)?;
    let rel_path = format!("{stem}/{n}.md");
    let sm_id = db.allocate_sm_id()?;

    let mut doc = Document {
        front: serde_yaml::Mapping::new(),
        body: if body.ends_with('\n') { body.to_string() } else { format!("{body}\n") },
    };
    doc.front.insert(serde_yaml::Value::from("type"), serde_yaml::Value::from(kind.as_str()));
    doc.set_sm_id(sm_id);
    doc.set_prio(parent.prio);
    if kind == ItemType::Article {
        if let Some(a) = parent.a_factor {
            doc.set_a_factor(a);
        }
    }
    doc.set_source_range(stem, &range.range_string());
    if let Some(u) = &parent.url {
        doc.set_url(u);
    }
    if let Some(d) = parent.imported {
        doc.set_imported(d);
    }
    let out = doc.serialize().with_context(|| format!("serializing {rel_path}"))?;
    std::fs::write(root.join(&rel_path), out).with_context(|| format!("writing {rel_path}"))?;
    index_path(root, db, &rel_path)
}

/// A new top-level item (M5): `a` for a card, `i` for an imported article.
pub struct NewItem<'a> {
    pub kind: ItemType,
    /// Text to slug for the filename; falls back to the kind name when the slug is empty.
    pub slug_base: &'a str,
    /// The whole body text (a trailing `\n` is added if missing).
    pub body: &'a str,
    /// The address it was imported from, when it was.
    pub url: Option<&'a str>,
    /// The date it was imported, when it was.
    pub imported: Option<NaiveDate>,
}

/// Write a new top-level file at the first free `<slug>.md`, `<slug>-2.md`, …
/// (`<kind>.md` when `slug_base` slugs to nothing), index it, and return its row.
pub fn create_item(root: &Path, db: &Db, item: NewItem<'_>) -> Result<ItemRow> {
    let slug = crate::import::slug(item.slug_base);
    let base = if slug.is_empty() { item.kind.as_str().to_string() } else { slug };
    let rel_path = first_free_name(root, &base);
    let sm_id = db.allocate_sm_id()?;

    let mut doc = Document {
        front: serde_yaml::Mapping::new(),
        body: if item.body.ends_with('\n') { item.body.to_string() } else { format!("{}\n", item.body) },
    };
    doc.front.insert(serde_yaml::Value::from("type"), serde_yaml::Value::from(item.kind.as_str()));
    doc.set_sm_id(sm_id);
    if let Some(u) = item.url {
        doc.set_url(u);
    }
    if let Some(d) = item.imported {
        doc.set_imported(d);
    }
    let out = doc.serialize().with_context(|| format!("serializing {rel_path}"))?;
    std::fs::write(root.join(&rel_path), out).with_context(|| format!("writing {rel_path}"))?;
    index_path(root, db, &rel_path)
}

/// The first of `<base>.md`, `<base>-2.md`, `<base>-3.md`, … that does not exist yet.
fn first_free_name(root: &Path, base: &str) -> String {
    let candidate = format!("{base}.md");
    if !root.join(&candidate).exists() {
        return candidate;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}.md");
        if !root.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Highest `<n>.md` in `dir` plus one; 1 for an empty folder. Other names are ignored.
fn next_child_number(dir: &Path) -> Result<u32> {
    let mut max = 0u32;
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name.strip_suffix(".md").and_then(|s| s.parse::<u32>().ok()) {
            max = max.max(n);
        }
    }
    Ok(max + 1)
}

/// Index one file grain just wrote, and return its row.
fn index_path(root: &Path, db: &Db, rel_path: &str) -> Result<ItemRow> {
    let abs_path = root.join(rel_path);
    let file = ScannedFile {
        rel_path: rel_path.to_string(),
        mtime: mtime_of(&abs_path)?,
        abs_path,
    };
    let known = db.path_index()?;
    let mut seen_ids: HashSet<i64> = known.values().map(|(id, _)| *id).collect();
    let mut claimed_ids = HashSet::new();
    index_file(db, &file, &mut seen_ids, &mut claimed_ids).with_context(|| format!("indexing {rel_path}"))?;
    let sm_id = *db
        .path_index()?
        .get(rel_path)
        .map(|(id, _)| id)
        .with_context(|| format!("{rel_path} was not indexed"))?;
    db.item(sm_id)?.with_context(|| format!("{rel_path} vanished from the index"))
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

    // ---- M2 writers ----

    use crate::vault::article::Span;

    fn article_setup() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "---\ntype: article\nsm_id: 10\nprio: 30\na_factor: 2\ncustom: keep\n---\n# Title\n\nPomelo is the largest citrus fruit.\nKumquat is small.\n",
        );
        write(root, "b.md", "---\ntype: article\nprio: 40\n---\nplain\n");
        let db = Db::open(&root.join(".grain/grain.db")).unwrap();
        refresh(root, &db).unwrap();
        (dir, db)
    }

    fn d(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn article_writers_rewrite_the_file_keep_unknown_keys_and_return_the_new_mtime() {
        let (dir, db) = article_setup();
        let root = dir.path();
        let m1 = write_article_session(root, "a.md", 10, d("2026-09-29"), 7).unwrap();
        let text = fs::read_to_string(root.join("a.md")).unwrap();
        assert!(text.starts_with("---\ntype: article\nsm_id: 10\ndue: 2026-09-29\ninterval: 7\nprio: 30\nread_pos: 10\na_factor: 2\ncustom: keep\n---\n# Title\n"), "{text}");
        assert_eq!(m1, mtime_of(&root.join("a.md")).unwrap());
        let _ = write_read_pos(root, "a.md", 36).unwrap();
        assert!(fs::read_to_string(root.join("a.md")).unwrap().contains("\nread_pos: 36\n"));
        let _ = write_prio(root, "a.md", 25).unwrap();
        assert!(fs::read_to_string(root.join("a.md")).unwrap().contains("\nprio: 25\nread_pos: 36\n"));
        let m2 = write_done(root, "a.md", d("2026-09-30")).unwrap();
        let text = fs::read_to_string(root.join("a.md")).unwrap();
        assert!(text.contains("\na_factor: 2\ndone: 2026-09-30\ncustom: keep\n"), "{text}");
        assert!(text.ends_with("Kumquat is small.\n"), "body intact");
        // Recording the mtime in the index makes the next refresh skip the file.
        db.set_done(10, d("2026-09-30"), m2).unwrap();
        let report = refresh(root, &db).unwrap();
        assert_eq!(report.indexed, 0, "{report:?}");
        assert!(write_prio(root, "nope.md", 1).unwrap_err().to_string().contains("nope.md"));
    }

    #[test]
    fn create_child_makes_a_numbered_file_in_a_folder_named_after_the_parent() {
        let (dir, db) = article_setup();
        let root = dir.path();
        let parent = db.item(10).unwrap().unwrap();
        let body = "Pomelo is the largest citrus fruit.\n";
        let child = create_child(root, &db, &parent, ItemType::Article, body, Span { start: 9, end: 44 }).unwrap();
        assert_eq!(child.path, "a/1.md");
        assert_eq!(child.kind, ItemType::Article);
        assert_eq!(child.prio, 30);
        assert_eq!(child.a_factor, Some(2.0));
        assert_eq!(child.source.as_deref(), Some("a"));
        assert_eq!(child.range, Some((9, 44)));
        assert!(child.sm_id > 0 && child.sm_id != 10);
        let text = fs::read_to_string(root.join("a/1.md")).unwrap();
        assert_eq!(
            text,
            format!("---\ntype: article\nsm_id: {}\nprio: 30\na_factor: 2\nsource: '[[a]]'\nrange: 9-44\n---\nPomelo is the largest citrus fruit.\n", child.sm_id)
        );
        assert_eq!(db.item(child.sm_id).unwrap().unwrap(), child, "indexed at once");
        assert_eq!(db.children_of("a").unwrap().len(), 1);
        assert_eq!(fs::read_to_string(root.join("a.md")).unwrap().matches("Pomelo").count(), 1, "parent untouched");

        // Numbering skips non-numeric names and continues from the highest number.
        write(root, "a/notes.md", "---\ntype: article\n---\nx\n");
        let second = create_child(root, &db, &parent, ItemType::Article, "Kumquat is small.\n", Span { start: 45, end: 62 }).unwrap();
        assert_eq!(second.path, "a/2.md");

        // A cloze child is a card, without a_factor, and is due now.
        let q = "Q: [...] is the largest citrus fruit.\n\nA: Pomelo\n";
        let cloze = create_child(root, &db, &parent, ItemType::Card, q, Span { start: 9, end: 15 }).unwrap();
        assert_eq!(cloze.path, "a/3.md");
        assert_eq!(cloze.kind, ItemType::Card);
        assert_eq!(cloze.a_factor, None);
        assert_eq!(cloze.due, None);
        assert_eq!(cloze.title.as_deref(), Some("[...] is the largest citrus fruit."));
        let text = fs::read_to_string(root.join("a/3.md")).unwrap();
        assert!(text.starts_with(&format!("---\ntype: card\nsm_id: {}\nprio: 30\nsource: '[[a]]'\nrange: 9-15\n---\nQ: [...]", cloze.sm_id)), "{text}");

        // Nested: a child of a child goes one folder deeper, with a path-qualified link.
        let grandchild = create_child(root, &db, &child, ItemType::Article, "Pomelo\n", Span { start: 0, end: 6 }).unwrap();
        assert_eq!(grandchild.path, "a/1/1.md");
        assert_eq!(grandchild.source.as_deref(), Some("a/1"));
        assert!(fs::read_to_string(root.join("a/1/1.md")).unwrap().contains("source: '[[a/1]]'"));

        // A parent without an explicit a_factor passes none on.
        let b = db.queue().unwrap().into_iter().find(|i| i.path == "b.md").unwrap();
        let bc = create_child(root, &db, &b, ItemType::Article, "plain\n", Span { start: 0, end: 5 }).unwrap();
        assert_eq!(bc.path, "b/1.md");
        assert_eq!(bc.a_factor, None);

        // The next refresh sees every child as unchanged and allocates nothing.
        let report = refresh(root, &db).unwrap();
        assert_eq!(report.indexed, 1, "only notes.md was new to the index: {report:?}");
        assert_eq!(report.allocated, 1, "notes.md had no sm_id; every child already had one");
        assert_eq!(report.unchanged, 7, "children are skipped by mtime: {report:?}");
        assert_eq!(db.queue().unwrap().len(), 8);
    }

    #[test]
    fn load_card_reads_document_and_body() {
        let (dir, db) = setup();
        refresh(dir.path(), &db).unwrap();
        let card = load_card(dir.path(), "a.md").unwrap();
        assert_eq!(card.meta.sm_id, Some(10));
        assert_eq!(card.body.title().as_deref(), Some("alpha?"));
    }

    // ---- M5 writers ----

    #[test]
    fn create_item_writes_card_and_indexes_it() {
        let (dir, db) = setup();
        let root = dir.path();
        let row = create_item(
            root,
            &db,
            NewItem {
                kind: ItemType::Card,
                slug_base: "Large citrus fruit?",
                body: "Q: Large citrus fruit?\n\nA: pomelo\n",
                url: None,
                imported: None,
            },
        )
        .unwrap();
        assert_eq!(row.path, "large-citrus-fruit.md");
        assert_eq!(row.kind, ItemType::Card);
        assert_eq!(row.title.as_deref(), Some("Large citrus fruit?"));
        assert_eq!(row.due, None);
        assert_eq!(row.prio, 50);
        let text = fs::read_to_string(root.join("large-citrus-fruit.md")).unwrap();
        assert_eq!(
            text,
            format!("---\ntype: card\nsm_id: {}\n---\nQ: Large citrus fruit?\n\nA: pomelo\n", row.sm_id)
        );
    }

    #[test]
    fn create_item_writes_article_with_reference() {
        let (dir, db) = setup();
        let root = dir.path();
        let row = create_item(
            root,
            &db,
            NewItem {
                kind: ItemType::Article,
                slug_base: "Pomelo",
                body: "# Pomelo\n\nBig.\n",
                url: Some("https://en.wikipedia.org/wiki/Pomelo"),
                imported: Some(d("2026-09-20")),
            },
        )
        .unwrap();
        assert_eq!(row.path, "pomelo.md");
        let text = fs::read_to_string(root.join("pomelo.md")).unwrap();
        assert_eq!(
            text,
            format!(
                "---\ntype: article\nsm_id: {}\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n---\n# Pomelo\n\nBig.\n",
                row.sm_id
            )
        );
        assert_eq!(row.url.as_deref(), Some("https://en.wikipedia.org/wiki/Pomelo"));
        assert_eq!(row.imported, Some(d("2026-09-20")));
    }

    #[test]
    fn create_item_suffixes_on_collision() {
        let (dir, db) = setup();
        let root = dir.path();
        let make = |body: &'static str| NewItem {
            kind: ItemType::Card,
            slug_base: "Pomelo",
            body,
            url: None,
            imported: None,
        };
        let first = create_item(root, &db, make("Q: a\n\nA: b\n")).unwrap();
        assert_eq!(first.path, "pomelo.md");
        let second = create_item(root, &db, make("Q: c\n\nA: d\n")).unwrap();
        assert_eq!(second.path, "pomelo-2.md");
        let third = create_item(root, &db, make("Q: e\n\nA: f\n")).unwrap();
        assert_eq!(third.path, "pomelo-3.md");
    }

    #[test]
    fn create_item_falls_back_to_kind_name() {
        let (dir, db) = setup();
        let root = dir.path();
        let card = create_item(
            root,
            &db,
            NewItem {
                kind: ItemType::Card,
                slug_base: "柚子",
                body: "Q: a\n\nA: b\n",
                url: None,
                imported: None,
            },
        )
        .unwrap();
        assert_eq!(card.path, "card.md");
        let article = create_item(
            root,
            &db,
            NewItem {
                kind: ItemType::Article,
                slug_base: "柚子",
                body: "# T\n",
                url: None,
                imported: None,
            },
        )
        .unwrap();
        assert_eq!(article.path, "article.md");
    }

    #[test]
    fn create_child_copies_url_and_imported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "---\ntype: article\nsm_id: 10\nprio: 30\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n---\n# Title\n\nPomelo is citrus.\n",
        );
        let db = Db::open(&root.join(".grain/grain.db")).unwrap();
        refresh(root, &db).unwrap();
        let parent = db.item(10).unwrap().unwrap();

        let article_child =
            create_child(root, &db, &parent, ItemType::Article, "Pomelo is citrus.\n", Span { start: 9, end: 25 }).unwrap();
        let text = fs::read_to_string(root.join("a/1.md")).unwrap();
        assert!(
            text.contains("\nrange: 9-25\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n"),
            "{text}"
        );
        assert_eq!(article_child.url.as_deref(), Some("https://en.wikipedia.org/wiki/Pomelo"));
        assert_eq!(article_child.imported, Some(d("2026-09-20")));

        let card_child = create_child(root, &db, &parent, ItemType::Card, "Q: a\n\nA: b\n", Span { start: 0, end: 6 }).unwrap();
        let text2 = fs::read_to_string(root.join("a/2.md")).unwrap();
        assert!(
            text2.contains("\nrange: 0-6\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n"),
            "{text2}"
        );
        assert_eq!(card_child.url.as_deref(), Some("https://en.wikipedia.org/wiki/Pomelo"));
        assert_eq!(card_child.imported, Some(d("2026-09-20")));
    }

    #[test]
    fn refresh_indexes_url_and_imported() {
        let (dir, db) = setup();
        write(
            dir.path(),
            "imported.md",
            "---\ntype: article\nsm_id: 40\nprio: 20\nurl: https://x.org/a\nimported: 2026-09-20\n---\n# Imported\nbody\n",
        );
        refresh(dir.path(), &db).unwrap();
        let row = db.item(40).unwrap().unwrap();
        assert_eq!(row.url.as_deref(), Some("https://x.org/a"));
        assert_eq!(row.imported, NaiveDate::from_ymd_opt(2026, 9, 20));
        assert_eq!(db.item(10).unwrap().unwrap().url, None, "a card without url stays NULL");
    }
}

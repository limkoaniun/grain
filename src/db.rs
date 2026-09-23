//! SQLite sidecar at `<vault>/.grain/grain.db`.
//!
//! A rebuildable cache of the vault plus the grade journal. All writes go
//! through one [`Db`] on one thread. Schema is versioned via `meta.schema_version`:
//! 1 is M0/M1, 2 (M2) adds article scheduling and the child link, 3 (M5) adds
//! `items.url` and `items.imported`. Each step runs once, in its own transaction.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};

use crate::vault::frontmatter::ItemType;

pub const SCHEMA_VERSION: i64 = 3;

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS items (
  sm_id    INTEGER PRIMARY KEY,
  path     TEXT NOT NULL UNIQUE,
  type     TEXT NOT NULL CHECK (type IN ('card','article')),
  due      TEXT,
  interval INTEGER,
  prio     INTEGER NOT NULL DEFAULT 50,
  read_pos INTEGER,
  tags     TEXT NOT NULL DEFAULT '',
  mtime    INTEGER NOT NULL,
  title    TEXT
);
CREATE INDEX IF NOT EXISTS idx_due  ON items(due) WHERE type = 'card';
CREATE INDEX IF NOT EXISTS idx_prio ON items(prio);

CREATE TABLE IF NOT EXISTS journal (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  sm_id     INTEGER NOT NULL,
  grade     INTEGER NOT NULL CHECK (grade BETWEEN 0 AND 5),
  graded_at TEXT NOT NULL,
  synced    INTEGER NOT NULL DEFAULT 0,
  interval_returned INTEGER
);
CREATE INDEX IF NOT EXISTS idx_pending ON journal(synced) WHERE synced = 0;

CREATE TABLE IF NOT EXISTS media (hash TEXT PRIMARY KEY, path TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta  (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Version 1 → 2 (M2): article scheduling, `done`, and the child link. The `mtime`
/// reset makes the next refresh re-index every file once so the new columns fill.
const MIGRATE_V2: &str = "
ALTER TABLE items ADD COLUMN a_factor    REAL;
ALTER TABLE items ADD COLUMN done        TEXT;
ALTER TABLE items ADD COLUMN source      TEXT;
ALTER TABLE items ADD COLUMN range_start INTEGER;
ALTER TABLE items ADD COLUMN range_end   INTEGER;
DROP INDEX IF EXISTS idx_due;
CREATE INDEX idx_due    ON items(due);
CREATE INDEX idx_source ON items(source);
UPDATE items SET mtime = 0;
UPDATE meta SET value = '2' WHERE key = 'schema_version';
";

/// Version 2 → 3 (M5): where an imported item came from and when. The `mtime`
/// reset makes the next refresh re-index every file once so the new columns fill.
const MIGRATE_V3: &str = "
ALTER TABLE items ADD COLUMN url      TEXT;
ALTER TABLE items ADD COLUMN imported TEXT;
CREATE INDEX idx_url ON items(url);
UPDATE items SET mtime = 0;
UPDATE meta SET value = '3' WHERE key = 'schema_version';
";

/// One row of `items`.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub sm_id: i64,
    pub path: String,
    pub kind: ItemType,
    pub due: Option<NaiveDate>,
    pub interval: Option<i64>,
    pub prio: i64,
    pub read_pos: Option<i64>,
    pub tags: String,
    pub mtime: i64,
    pub title: Option<String>,
    /// M2: article interval multiplier as written in the file; `None` means the default.
    pub a_factor: Option<f64>,
    /// M2: date the article was marked done; `None` means active.
    pub done: Option<NaiveDate>,
    /// M2: link target of `source` without brackets or `.md` (`citrus-vocab`, `citrus-vocab/1`).
    pub source: Option<String>,
    /// M2: `range` as `(start, end)` character offsets into the parent body.
    pub range: Option<(i64, i64)>,
    /// M5: the address an imported article came from; `None` when it was not imported.
    pub url: Option<String>,
    /// M5: the date the item was imported; `None` when it was not imported.
    pub imported: Option<NaiveDate>,
}

/// One unsynced journal row, ready to be sent to the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGrade {
    pub journal_id: i64,
    pub sm_id: i64,
    pub grade: u8,
    /// ISO 8601 UTC, as journaled.
    pub graded_at: String,
}

/// The schedule a sync landed on a card: what goes into `items` alongside the journal update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub sm_id: i64,
    pub due: NaiveDate,
    pub interval: i64,
    /// The card file's mtime after `due`/`interval` were written into it.
    pub mtime: i64,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the sidecar database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL")?;
        Self::from_connection(conn)
    }

    /// An in-memory database, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA_V1).context("creating schema")?;
        let mut version = match self.meta("schema_version")? {
            None => {
                // A fresh database starts at 1 and takes the same steps as an old one.
                self.set_meta("schema_version", "1")?;
                1
            }
            Some(v) => v.parse::<i64>().context("meta.schema_version is not an integer")?,
        };
        if version > SCHEMA_VERSION {
            bail!("grain.db schema version {version} is newer than this build supports ({SCHEMA_VERSION})");
        }
        if version == 1 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATE_V2).context("migrating grain.db to schema version 2")?;
            tx.commit()?;
            version = 2;
        }
        if version == 2 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATE_V3).context("migrating grain.db to schema version 3")?;
            tx.commit()?;
            version = 3;
        }
        debug_assert_eq!(version, SCHEMA_VERSION);
        if self.meta("next_sm_id")?.is_none() {
            self.set_meta("next_sm_id", "1")?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// An integer `meta` value, or `default` when the key is absent.
    pub fn meta_i64(&self, key: &str, default: i64) -> Result<i64> {
        match self.meta(key)? {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse::<i64>()
                .with_context(|| format!("meta.{key} `{v}` is not an integer")),
        }
    }

    /// Hand out the next unused sm_id and persist the counter, atomically.
    /// Ids already present in `items` are skipped so a hand-numbered vault never collides.
    pub fn allocate_sm_id(&self) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let mut next: i64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'next_sm_id'", [], |r| {
                r.get::<_, String>(0)
            })?
            .parse()
            .context("meta.next_sm_id is not an integer")?;
        loop {
            let taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE sm_id = ?1)",
                [next],
                |r| r.get(0),
            )?;
            if !taken {
                break;
            }
            next += 1;
        }
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'next_sm_id'",
            [(next + 1).to_string()],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Insert or replace an item by sm_id. A changed path (rename) updates the row in place.
    pub fn upsert_item(&self, item: &ItemRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO items (sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title,
                                a_factor, done, source, range_start, range_end, url, imported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(sm_id) DO UPDATE SET
               path = excluded.path, type = excluded.type, due = excluded.due,
               interval = excluded.interval, prio = excluded.prio, read_pos = excluded.read_pos,
               tags = excluded.tags, mtime = excluded.mtime, title = excluded.title,
               a_factor = excluded.a_factor, done = excluded.done, source = excluded.source,
               range_start = excluded.range_start, range_end = excluded.range_end,
               url = excluded.url, imported = excluded.imported",
            params![
                item.sm_id,
                item.path,
                item.kind.as_str(),
                item.due.map(|d| d.to_string()),
                item.interval,
                item.prio,
                item.read_pos,
                item.tags,
                item.mtime,
                item.title,
                item.a_factor,
                item.done.map(|d| d.to_string()),
                item.source,
                item.range.map(|r| r.0),
                item.range.map(|r| r.1),
                item.url,
                item.imported.map(|d| d.to_string()),
            ],
        )
        .with_context(|| format!("indexing {}", item.path))?;
        Ok(())
    }

    /// `path -> (sm_id, mtime)` for every indexed item; used for mtime-diffing.
    pub fn path_index(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT path, sm_id, mtime FROM items")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Delete item rows whose path is not in `present`. Journal rows are kept.
    pub fn delete_missing(&self, present: &[String]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch("CREATE TEMP TABLE present (path TEXT PRIMARY KEY)")?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO present (path) VALUES (?1)")?;
            for p in present {
                ins.execute([p])?;
            }
        }
        let n = tx.execute("DELETE FROM items WHERE path NOT IN (SELECT path FROM present)", [])?;
        tx.execute_batch("DROP TABLE present")?;
        tx.commit()?;
        Ok(n)
    }

    /// Every active item, cards and articles mixed, ordered `prio ASC, due ASC` (NULL due first).
    /// Items marked `done` are left out.
    pub fn queue(&self) -> Result<Vec<ItemRow>> {
        self.select_items("WHERE done IS NULL ORDER BY prio ASC, due ASC, sm_id ASC", &[])
    }

    /// Items of both types with `due IS NULL OR due <= today`, `done` excluded, in queue order.
    pub fn due_items(&self, today: NaiveDate) -> Result<Vec<ItemRow>> {
        self.select_items(
            "WHERE done IS NULL AND (due IS NULL OR due <= ?1)
             ORDER BY prio ASC, due ASC, sm_id ASC",
            &[&today.to_string()],
        )
    }

    /// Items whose `source` is `target` (a path without `.md`), by position in the parent.
    pub fn children_of(&self, target: &str) -> Result<Vec<ItemRow>> {
        self.select_items("WHERE source = ?1 ORDER BY range_start ASC, sm_id ASC", &[&target])
    }

    /// After a reading session: the next date, interval and read-point, plus the file's new mtime.
    pub fn set_article_session(&self, sm_id: i64, due: NaiveDate, interval: i64, read_pos: i64, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET due = ?2, interval = ?3, read_pos = ?4, mtime = ?5 WHERE sm_id = ?1",
            params![sm_id, due.to_string(), interval, read_pos, mtime],
        )
        .with_context(|| format!("scheduling article sm_id {sm_id}"))?;
        Ok(())
    }

    pub fn set_read_pos(&self, sm_id: i64, read_pos: i64, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET read_pos = ?2, mtime = ?3 WHERE sm_id = ?1",
            params![sm_id, read_pos, mtime],
        )
        .with_context(|| format!("saving read_pos for sm_id {sm_id}"))?;
        Ok(())
    }

    pub fn set_prio(&self, sm_id: i64, prio: i64, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET prio = ?2, mtime = ?3 WHERE sm_id = ?1",
            params![sm_id, prio, mtime],
        )
        .with_context(|| format!("setting prio for sm_id {sm_id}"))?;
        Ok(())
    }

    pub fn set_done(&self, sm_id: i64, done: NaiveDate, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET done = ?2, mtime = ?3 WHERE sm_id = ?1",
            params![sm_id, done.to_string(), mtime],
        )
        .with_context(|| format!("marking sm_id {sm_id} done"))?;
        Ok(())
    }

    pub fn item(&self, sm_id: i64) -> Result<Option<ItemRow>> {
        Ok(self.select_items("WHERE sm_id = ?1", &[&sm_id])?.into_iter().next())
    }

    /// M5: the item imported from `url`, if the vault already holds one.
    pub fn item_by_url(&self, url: &str) -> Result<Option<ItemRow>> {
        Ok(self.select_items("WHERE url = ?1 LIMIT 1", &[&url])?.into_iter().next())
    }

    fn select_items(&self, tail: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<ItemRow>> {
        let sql = format!(
            "SELECT sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title,
                    a_factor, done, source, range_start, range_end, url, imported
             FROM items {tail}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, |r| {
            Ok(RawItem {
                sm_id: r.get(0)?,
                path: r.get(1)?,
                kind: r.get(2)?,
                due: r.get(3)?,
                interval: r.get(4)?,
                prio: r.get(5)?,
                read_pos: r.get(6)?,
                tags: r.get(7)?,
                mtime: r.get(8)?,
                title: r.get(9)?,
                a_factor: r.get(10)?,
                done: r.get(11)?,
                source: r.get(12)?,
                range_start: r.get(13)?,
                range_end: r.get(14)?,
                url: r.get(15)?,
                imported: r.get(16)?,
            })
        })?;
        rows.map(|r| r?.into_row()).collect()
    }

    /// Journal one grade. Commits before returning; the UI advances only after this.
    pub fn insert_grade(&self, sm_id: i64, grade: u8, graded_at: &str) -> Result<i64> {
        if grade > 5 {
            bail!("grade {grade} out of range 0-5");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO journal (sm_id, grade, graded_at, synced) VALUES (?1, ?2, ?3, 0)",
            params![sm_id, grade, graded_at],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Delete one journal row if it is still unsynced. Returns whether a row was removed.
    /// Rows with `synced = 1` are never deleted.
    pub fn delete_unsynced_grade(&self, journal_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM journal WHERE id = ?1 AND synced = 0",
            [journal_id],
        )?;
        Ok(n == 1)
    }

    /// Persist the outbox's daily request counter (`sync_requests_day`, `sync_requests_today`).
    pub fn set_sync_counter(&self, day: NaiveDate, count: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (key, value) in [("sync_requests_day", day.to_string()), ("sync_requests_today", count.to_string())] {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Journal rows with `synced = 0`, oldest first (`id ASC`).
    pub fn pending_grades(&self) -> Result<Vec<PendingGrade>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, sm_id, grade, graded_at FROM journal WHERE synced = 0 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingGrade {
                journal_id: r.get(0)?,
                sm_id: r.get(1)?,
                grade: r.get::<_, i64>(2)?.clamp(0, 5) as u8,
                graded_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record that the scheduler accepted a grade: `synced = 1` plus the returned interval,
    /// and, when the card still exists, its new `due`/`interval`/`mtime`. One transaction.
    pub fn apply_sync(&self, journal_id: i64, interval: i64, schedule: Option<&Schedule>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE journal SET synced = 1, interval_returned = ?2 WHERE id = ?1",
            params![journal_id, interval],
        )
        .with_context(|| format!("marking journal row {journal_id} synced"))?;
        if let Some(s) = schedule {
            tx.execute(
                "UPDATE items SET due = ?2, interval = ?3, mtime = ?4 WHERE sm_id = ?1",
                params![s.sm_id, s.due.to_string(), s.interval, s.mtime],
            )
            .with_context(|| format!("scheduling sm_id {}", s.sm_id))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `(synced, interval_returned)` of one journal row, for tests.
    #[cfg(test)]
    pub fn journal_row_sync(&self, journal_id: i64) -> Result<(i64, Option<i64>)> {
        Ok(self.conn.query_row(
            "SELECT synced, interval_returned FROM journal WHERE id = ?1",
            [journal_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    #[cfg(test)]
    pub fn journal_count(&self, sm_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM journal WHERE sm_id = ?1",
            [sm_id],
            |r| r.get(0),
        )?)
    }
}

/// Column-typed intermediate so type/date validation errors carry context.
struct RawItem {
    sm_id: i64,
    path: String,
    kind: String,
    due: Option<String>,
    interval: Option<i64>,
    prio: i64,
    read_pos: Option<i64>,
    tags: String,
    mtime: i64,
    title: Option<String>,
    a_factor: Option<f64>,
    done: Option<String>,
    source: Option<String>,
    range_start: Option<i64>,
    range_end: Option<i64>,
    url: Option<String>,
    imported: Option<String>,
}

impl RawItem {
    fn into_row(self) -> Result<ItemRow> {
        let date = |key: &str, s: Option<String>| -> Result<Option<NaiveDate>> {
            match s {
                None => Ok(None),
                Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                    .map(Some)
                    .with_context(|| format!("bad {key} date `{s}` for sm_id {}", self.sm_id)),
            }
        };
        let due = date("due", self.due)?;
        let done = date("done", self.done)?;
        let imported = date("imported", self.imported)?;
        let range = match (self.range_start, self.range_end) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        Ok(ItemRow {
            sm_id: self.sm_id,
            path: self.path,
            kind: ItemType::parse(&self.kind)?,
            due,
            interval: self.interval,
            prio: self.prio,
            read_pos: self.read_pos,
            tags: self.tags,
            mtime: self.mtime,
            title: self.title,
            a_factor: self.a_factor,
            done,
            source: self.source,
            range,
            url: self.url,
            imported,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use chrono::NaiveDate;
    use std::path::Path;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn card(sm_id: i64, path: &str, prio: i64, due: Option<&str>) -> ItemRow {
        ItemRow {
            sm_id,
            path: path.to_string(),
            kind: ItemType::Card,
            due: due.map(d),
            interval: None,
            prio,
            read_pos: None,
            tags: String::new(),
            mtime: 100,
            title: Some(format!("title {sm_id}")),
            a_factor: None,
            done: None,
            source: None,
            range: None,
            url: None,
            imported: None,
        }
    }

    #[test]
    fn fresh_db_has_schema_version_and_next_sm_id() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("3"));
        assert_eq!(db.meta("next_sm_id").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn open_on_disk_enables_wal_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".grain").join("grain.db");
        {
            let db = Db::open(&path).unwrap();
            let mode: String = db
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            db.upsert_item(&card(1, "a.md", 50, None)).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.queue().unwrap().len(), 1);
    }

    #[test]
    fn allocate_sm_id_hands_out_consecutive_ids() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.allocate_sm_id().unwrap(), 1);
        assert_eq!(db.allocate_sm_id().unwrap(), 2);
        assert_eq!(db.meta("next_sm_id").unwrap().as_deref(), Some("3"));
    }

    #[test]
    fn allocate_sm_id_skips_ids_already_in_items() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "taken.md", 50, None)).unwrap();
        assert_eq!(db.allocate_sm_id().unwrap(), 2);
    }

    #[test]
    fn upsert_by_sm_id_moves_path_on_rename() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(5, "old.md", 50, None)).unwrap();
        let mut renamed = card(5, "new.md", 50, None);
        renamed.mtime = 200;
        db.upsert_item(&renamed).unwrap();
        let index = db.path_index().unwrap();
        assert_eq!(index.get("new.md"), Some(&(5, 200)));
        assert!(!index.contains_key("old.md"));
    }

    #[test]
    fn queue_orders_by_prio_then_due_with_null_due_first() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 50, Some("2026-09-25"))).unwrap();
        db.upsert_item(&card(2, "b.md", 20, Some("2026-09-30"))).unwrap();
        db.upsert_item(&card(3, "c.md", 20, None)).unwrap();
        db.upsert_item(&card(4, "d.md", 20, Some("2026-09-01"))).unwrap();
        let ids: Vec<i64> = db.queue().unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [3, 4, 2, 1]);
    }

    #[test]
    fn due_items_mixes_types_in_queue_order_and_skips_future() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 10, Some("2026-09-25"))).unwrap();
        db.upsert_item(&card(2, "b.md", 20, Some("2026-09-20"))).unwrap();
        db.upsert_item(&card(3, "c.md", 30, None)).unwrap();
        let mut art = card(4, "art.md", 5, None);
        art.kind = ItemType::Article;
        db.upsert_item(&art).unwrap();
        let ids: Vec<i64> = db.due_items(d("2026-09-20")).unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [4, 2, 3], "cards and articles both due, future card excluded, prio ASC then due ASC");
    }

    #[test]
    fn delete_missing_removes_rows_but_keeps_journal() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "keep.md", 50, None)).unwrap();
        db.upsert_item(&card(2, "gone.md", 50, None)).unwrap();
        db.insert_grade(2, 4, "2026-09-20T10:00:00Z").unwrap();
        db.delete_missing(&["keep.md".to_string()]).unwrap();
        let ids: Vec<i64> = db.queue().unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [1]);
        assert_eq!(db.journal_count(2).unwrap(), 1);
    }

    #[test]
    fn journal_insert_then_undo_removes_newest_unsynced_only() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 50, None)).unwrap();
        let first = db.insert_grade(1, 3, "2026-09-20T10:00:00Z").unwrap();
        let second = db.insert_grade(1, 5, "2026-09-20T10:00:01Z").unwrap();
        assert!(second > first);
        assert_eq!(db.journal_count(1).unwrap(), 2);
        assert!(db.delete_unsynced_grade(second).unwrap());
        assert_eq!(db.journal_count(1).unwrap(), 1);
        assert!(!db.delete_unsynced_grade(second).unwrap());
        assert!(db.delete_unsynced_grade(first).unwrap());
        assert_eq!(db.journal_count(1).unwrap(), 0);
    }

    #[test]
    fn synced_journal_rows_are_never_deleted() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_grade(1, 3, "2026-09-20T10:00:00Z").unwrap();
        db.conn
            .execute("UPDATE journal SET synced = 1 WHERE id = ?1", [id])
            .unwrap();
        assert!(!db.delete_unsynced_grade(id).unwrap());
        assert_eq!(db.journal_count(1).unwrap(), 1);
    }

    #[test]
    fn insert_grade_rejects_out_of_range() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.insert_grade(1, 6, "2026-09-20T10:00:00Z").is_err());
    }

    #[test]
    fn pending_grades_are_unsynced_rows_oldest_first() {
        let db = Db::open_in_memory().unwrap();
        let a = db.insert_grade(7, 3, "2026-09-20T10:00:00Z").unwrap();
        let b = db.insert_grade(8, 5, "2026-09-20T10:00:01Z").unwrap();
        let c = db.insert_grade(7, 4, "2026-09-20T10:00:02Z").unwrap();
        db.apply_sync(b, 12, None).unwrap();
        let pending = db.pending_grades().unwrap();
        let ids: Vec<i64> = pending.iter().map(|p| p.journal_id).collect();
        assert_eq!(ids, [a, c]);
        assert_eq!(pending[0].sm_id, 7);
        assert_eq!(pending[0].grade, 3);
        assert_eq!(pending[0].graded_at, "2026-09-20T10:00:00Z");
    }

    #[test]
    fn apply_sync_marks_row_and_updates_item_schedule_together() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 50, None)).unwrap();
        let id = db.insert_grade(1, 4, "2026-09-20T10:00:00Z").unwrap();
        let schedule = Schedule {
            sm_id: 1,
            due: d("2026-10-02"),
            interval: 12,
            mtime: 555,
        };
        db.apply_sync(id, 12, Some(&schedule)).unwrap();
        assert!(db.pending_grades().unwrap().is_empty());
        assert!(!db.delete_unsynced_grade(id).unwrap(), "synced row is protected");
        let (synced, returned): (i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT synced, interval_returned FROM journal WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((synced, returned), (1, Some(12)));
        let item = db.item(1).unwrap().unwrap();
        assert_eq!(item.due, Some(d("2026-10-02")));
        assert_eq!(item.interval, Some(12));
        assert_eq!(item.mtime, 555);
        assert_eq!(db.path_index().unwrap().get("a.md"), Some(&(1, 555)));
    }

    #[test]
    fn apply_sync_without_schedule_leaves_item_alone() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 50, Some("2026-09-01"))).unwrap();
        let id = db.insert_grade(1, 4, "2026-09-20T10:00:00Z").unwrap();
        db.apply_sync(id, 3, None).unwrap();
        let item = db.item(1).unwrap().unwrap();
        assert_eq!(item.due, Some(d("2026-09-01")));
        assert_eq!(item.mtime, 100);
    }

    /// A schema-version-1 database with two rows, as M0/M1 would have left it.
    fn v1_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute("INSERT INTO meta (key, value) VALUES ('schema_version', '1'), ('next_sm_id', '3')", []).unwrap();
        conn.execute(
            "INSERT INTO items (sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title)
             VALUES (1, 'a.md', 'card', '2026-09-25', 6, 28, NULL, '', 100, 'a'),
                    (2, 'art.md', 'article', NULL, NULL, 20, 2210, '', 100, 'Article')",
            [],
        ).unwrap();
        conn.execute("INSERT INTO journal (sm_id, grade, graded_at, synced) VALUES (1, 4, '2026-09-20T10:00:00Z', 1)", []).unwrap();
    }

    #[test]
    fn v1_database_migrates_to_current_in_place_keeping_rows_and_forcing_a_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grain.db");
        v1_database(&path);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("3"));
        assert_eq!(SCHEMA_VERSION, 3);
        let items = db.queue().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.mtime == 0), "mtime zeroed so the next refresh re-indexes: {items:?}");
        assert!(items.iter().all(|i| i.a_factor.is_none() && i.done.is_none() && i.source.is_none() && i.range.is_none()));
        assert_eq!(db.journal_count(1).unwrap(), 1);
        assert_eq!(db.meta("next_sm_id").unwrap().as_deref(), Some("3"));
        drop(db);
        let again = Db::open(&path).unwrap();
        assert_eq!(again.meta("schema_version").unwrap().as_deref(), Some("3"), "idempotent");
        let cols = item_columns(&again);
        for c in ["a_factor", "done", "source", "range_start", "range_end"] {
            assert!(cols.iter().any(|x| x == c), "missing column {c}: {cols:?}");
        }
    }

    fn item_columns(db: &Db) -> Vec<String> {
        db.conn
            .prepare("PRAGMA table_info(items)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    /// A schema-version-2 database with one row, as M2–M4 would have left it.
    fn v2_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1'), ('next_sm_id', '3')",
            [],
        )
        .unwrap();
        conn.execute_batch(MIGRATE_V2).unwrap();
        conn.execute(
            "INSERT INTO items (sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title)
             VALUES (1, 'a.md', 'card', '2026-09-25', 6, 28, NULL, '', 5, 'a')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn v2_database_migrates_to_3_and_zeroes_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grain.db");
        v2_database(&path);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("3"));
        let cols = item_columns(&db);
        for c in ["url", "imported"] {
            assert!(cols.iter().any(|x| x == c), "missing column {c}: {cols:?}");
        }
        let item = db.item(1).unwrap().unwrap();
        assert_eq!(item.mtime, 0, "mtime zeroed so the next refresh re-indexes: {item:?}");
        assert_eq!(item.url, None);
        assert_eq!(item.imported, None);
        assert_eq!(item.due, Some(d("2026-09-25")), "the row survives");
        let indexes: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'idx_url'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexes, 1, "idx_url created");
        drop(db);
        let again = Db::open(&path).unwrap();
        assert_eq!(again.meta("schema_version").unwrap().as_deref(), Some("3"), "idempotent");
    }

    #[test]
    fn newer_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grain.db");
        v1_database(&path);
        Connection::open(&path).unwrap().execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", []).unwrap();
        assert!(Db::open(&path).is_err());
    }

    fn article(sm_id: i64, path: &str, prio: i64, due: Option<&str>) -> ItemRow {
        let mut row = card(sm_id, path, prio, due);
        row.kind = ItemType::Article;
        row
    }

    #[test]
    fn done_items_are_hidden_from_the_queue_and_from_due_items() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&article(1, "a.md", 20, None)).unwrap();
        db.upsert_item(&article(2, "b.md", 20, Some("2026-09-25"))).unwrap();
        let mut done = article(3, "c.md", 5, None);
        done.done = Some(d("2026-09-21"));
        db.upsert_item(&done).unwrap();
        db.upsert_item(&card(4, "k.md", 10, None)).unwrap();
        let ids: Vec<i64> = db.queue().unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [4, 1, 2], "done hidden; prio then due");
        let due_items: Vec<i64> = db.due_items(d("2026-09-20")).unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(due_items, [4, 1], "due_items mixes types and skips done");
    }

    #[test]
    fn source_and_range_round_trip_and_children_of_orders_by_range() {
        let db = Db::open_in_memory().unwrap();
        let mut c1 = card(10, "citrus-vocab/2.md", 20, None);
        c1.source = Some("citrus-vocab".to_string());
        c1.range = Some((153, 205));
        let mut c2 = card(11, "citrus-vocab/1.md", 20, None);
        c2.source = Some("citrus-vocab".to_string());
        c2.range = Some((83, 89));
        let mut other = card(12, "x.md", 20, None);
        other.source = Some("elsewhere".to_string());
        for r in [&c1, &c2, &other] {
            db.upsert_item(r).unwrap();
        }
        let kids = db.children_of("citrus-vocab").unwrap();
        assert_eq!(kids.iter().map(|k| k.sm_id).collect::<Vec<_>>(), [11, 10]);
        assert_eq!(kids[0].range, Some((83, 89)));
        assert_eq!(db.item(12).unwrap().unwrap().source.as_deref(), Some("elsewhere"));
    }

    #[test]
    fn article_setters_update_the_row_and_its_mtime() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&article(1, "a.md", 20, None)).unwrap();
        db.set_article_session(1, d("2026-09-29"), 7, 118, 501).unwrap();
        let row = db.item(1).unwrap().unwrap();
        assert_eq!((row.due, row.interval, row.read_pos, row.mtime), (Some(d("2026-09-29")), Some(7), Some(118), 501));
        db.set_read_pos(1, 153, 502).unwrap();
        let row = db.item(1).unwrap().unwrap();
        assert_eq!((row.read_pos, row.mtime), (Some(153), 502));
        db.set_prio(1, 30, 503).unwrap();
        let row = db.item(1).unwrap().unwrap();
        assert_eq!((row.prio, row.mtime), (30, 503));
        db.set_done(1, d("2026-09-30"), 504).unwrap();
        let row = db.item(1).unwrap().unwrap();
        assert_eq!((row.done, row.mtime), (Some(d("2026-09-30")), 504));
        assert!(db.queue().unwrap().is_empty());
    }

    #[test]
    fn meta_i64_defaults_and_parses_and_rejects_garbage() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta_i64("sync_daily_cap", 50).unwrap(), 50);
        db.set_meta("sync_daily_cap", "1").unwrap();
        assert_eq!(db.meta_i64("sync_daily_cap", 50).unwrap(), 1);
        db.set_meta("sync_daily_cap", "lots").unwrap();
        let err = db.meta_i64("sync_daily_cap", 50).unwrap_err().to_string();
        assert!(err.contains("sync_daily_cap"), "{err}");
    }

    #[test]
    fn upsert_and_item_round_trip_url_imported() {
        let db = Db::open_in_memory().unwrap();
        let mut art = article(1, "imported/a.md", 20, None);
        art.url = Some("https://x.org/a".to_string());
        art.imported = Some(d("2026-09-20"));
        db.upsert_item(&art).unwrap();
        db.upsert_item(&card(2, "b.md", 50, None)).unwrap();

        let row = db.item(1).unwrap().unwrap();
        assert_eq!(row.url.as_deref(), Some("https://x.org/a"));
        assert_eq!(row.imported, Some(d("2026-09-20")));
        assert_eq!(db.item(2).unwrap().unwrap().url, None);

        let found = db.item_by_url("https://x.org/a").unwrap().unwrap();
        assert_eq!(found.sm_id, 1);
        assert!(db.item_by_url("https://x.org/none").unwrap().is_none());
    }
}

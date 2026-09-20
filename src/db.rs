//! SQLite sidecar at `<vault>/.grain/grain.db`.
//!
//! A rebuildable cache of the vault plus the grade journal. All writes go
//! through one [`Db`] on one thread. Schema is versioned via `meta.schema_version`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};

use crate::vault::frontmatter::ItemType;

pub const SCHEMA_VERSION: i64 = 1;

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

/// One row of `items`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let version = match self.meta("schema_version")? {
            None => {
                self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
                SCHEMA_VERSION
            }
            Some(v) => v.parse::<i64>().context("meta.schema_version is not an integer")?,
        };
        if version > SCHEMA_VERSION {
            bail!("grain.db schema version {version} is newer than this build supports ({SCHEMA_VERSION})");
        }
        // Future migrations go here, stepping `version` up to SCHEMA_VERSION.
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
            "INSERT INTO items (sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(sm_id) DO UPDATE SET
               path = excluded.path, type = excluded.type, due = excluded.due,
               interval = excluded.interval, prio = excluded.prio, read_pos = excluded.read_pos,
               tags = excluded.tags, mtime = excluded.mtime, title = excluded.title",
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

    /// All items, cards and articles mixed, ordered `prio ASC, due ASC` (NULL due first).
    pub fn queue(&self) -> Result<Vec<ItemRow>> {
        self.select_items("ORDER BY prio ASC, due ASC, sm_id ASC", &[])
    }

    /// Cards with `due IS NULL OR due <= today`, in queue order.
    pub fn due_cards(&self, today: NaiveDate) -> Result<Vec<ItemRow>> {
        self.select_items(
            "WHERE type = 'card' AND (due IS NULL OR due <= ?1) ORDER BY prio ASC, due ASC, sm_id ASC",
            &[&today.to_string()],
        )
    }

    pub fn item(&self, sm_id: i64) -> Result<Option<ItemRow>> {
        Ok(self.select_items("WHERE sm_id = ?1", &[&sm_id])?.into_iter().next())
    }

    fn select_items(&self, tail: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<ItemRow>> {
        let sql = format!(
            "SELECT sm_id, path, type, due, interval, prio, read_pos, tags, mtime, title FROM items {tail}"
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
}

impl RawItem {
    fn into_row(self) -> Result<ItemRow> {
        let due = match self.due {
            None => None,
            Some(s) => Some(
                NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                    .with_context(|| format!("bad due date `{s}` for sm_id {}", self.sm_id))?,
            ),
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
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use chrono::NaiveDate;

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
        }
    }

    #[test]
    fn fresh_db_has_schema_version_and_next_sm_id() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("1"));
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
    fn due_cards_excludes_articles_and_future_cards() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_item(&card(1, "a.md", 10, Some("2026-09-25"))).unwrap();
        db.upsert_item(&card(2, "b.md", 20, Some("2026-09-20"))).unwrap();
        db.upsert_item(&card(3, "c.md", 30, None)).unwrap();
        let mut art = card(4, "art.md", 5, None);
        art.kind = ItemType::Article;
        db.upsert_item(&art).unwrap();
        let ids: Vec<i64> = db.due_cards(d("2026-09-20")).unwrap().iter().map(|i| i.sm_id).collect();
        assert_eq!(ids, [2, 3]);
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
}

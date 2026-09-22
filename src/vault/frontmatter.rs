//! Frontmatter parsing and serialization.
//!
//! A [`Document`] keeps the raw YAML mapping so unknown keys survive a
//! rewrite untouched; [`ItemMeta`] is the typed view grain actually uses.

use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDate;
use serde_yaml::{Mapping, Value};

const FENCE: &str = "---";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    Card,
    Article,
}

impl ItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemType::Card => "card",
            ItemType::Article => "article",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "card" => Ok(ItemType::Card),
            "article" => Ok(ItemType::Article),
            other => bail!("unknown item type `{other}` (expected card or article)"),
        }
    }
}

/// Typed view of the frontmatter keys grain understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemMeta {
    pub kind: ItemType,
    pub sm_id: Option<i64>,
    pub due: Option<NaiveDate>,
    pub interval: Option<i64>,
    pub prio: i64,
    pub read_pos: Option<i64>,
    pub source: Option<String>,
    pub range: Option<String>,
    pub tags: Vec<String>,
}

/// A markdown file split into its YAML frontmatter and body.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub front: Mapping,
    pub body: String,
}

impl Document {
    /// Parse a file's full text. Frontmatter is required and must start on line 1.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.replace("\r\n", "\n");
        let rest = text
            .strip_prefix(FENCE)
            .and_then(|r| r.strip_prefix('\n'))
            .ok_or_else(|| anyhow!("missing frontmatter: file must start with `---`"))?;
        let (yaml, body) = split_at_closing_fence(rest)
            .ok_or_else(|| anyhow!("missing frontmatter: no closing `---`"))?;
        let front = match serde_yaml::from_str::<Value>(yaml).context("invalid frontmatter YAML")? {
            Value::Mapping(m) => m,
            Value::Null => Mapping::new(),
            _ => bail!("frontmatter must be a YAML mapping"),
        };
        let doc = Document {
            front,
            body: body.to_string(),
        };
        doc.meta()?;
        Ok(doc)
    }

    /// Serialize back to file text. Key order and unknown keys are preserved.
    pub fn serialize(&self) -> Result<String> {
        let yaml = if self.front.is_empty() {
            String::new()
        } else {
            serde_yaml::to_string(&self.front).context("serializing frontmatter")?
        };
        Ok(format!("{FENCE}\n{yaml}{FENCE}\n{}", self.body))
    }

    /// The typed view of the keys grain understands.
    pub fn meta(&self) -> Result<ItemMeta> {
        let kind = match self.get("type") {
            Some(Value::String(s)) => ItemType::parse(s)?,
            Some(_) => bail!("frontmatter `type` must be a string"),
            None => bail!("frontmatter is missing `type`"),
        };
        Ok(ItemMeta {
            kind,
            sm_id: self.int("sm_id")?,
            due: self.date("due")?,
            interval: self.int("interval")?,
            prio: self.int("prio")?.unwrap_or(50),
            read_pos: self.int("read_pos")?,
            source: self.string("source")?,
            range: self.string("range")?,
            tags: self.tags()?,
        })
    }

    /// Set `sm_id`, placing it right after `type` for a new key so files stay tidy.
    pub fn set_sm_id(&mut self, sm_id: i64) {
        self.set_after("sm_id", Value::from(sm_id), &["type"]);
    }

    /// Set `due` and `interval` after a sync. New keys go right after `sm_id`
    /// (or `type` when there is no `sm_id`); existing keys keep their position.
    pub fn set_schedule(&mut self, due: NaiveDate, interval: i64) {
        self.set_after("due", Value::from(due.to_string()), &["sm_id", "type"]);
        self.set_after("interval", Value::from(interval), &["due", "sm_id", "type"]);
    }

    /// Overwrite `key` in place, or insert it after the first of `anchors` that
    /// exists (appending when none does). Unknown keys and their order survive.
    fn set_after(&mut self, key: &str, value: Value, anchors: &[&str]) {
        let key = Value::from(key);
        if self.front.contains_key(&key) {
            self.front.insert(key, value);
            return;
        }
        let anchor = anchors
            .iter()
            .find(|a| self.front.contains_key(Value::from(**a)))
            .copied();
        let mut rebuilt = Mapping::with_capacity(self.front.len() + 1);
        let mut inserted = false;
        for (k, v) in self.front.iter() {
            rebuilt.insert(k.clone(), v.clone());
            if !inserted && k.as_str() == anchor {
                rebuilt.insert(key.clone(), value.clone());
                inserted = true;
            }
        }
        if !inserted {
            rebuilt.insert(key, value);
        }
        self.front = rebuilt;
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.front.get(Value::from(key))
    }

    fn int(&self, key: &str) -> Result<Option<i64>> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| anyhow!("frontmatter `{key}` must be an integer")).map(Some),
            Some(Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map(Some)
                .with_context(|| format!("frontmatter `{key}` must be an integer")),
            Some(_) => bail!("frontmatter `{key}` must be an integer"),
        }
    }

    fn string(&self, key: &str) -> Result<Option<String>> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(Value::Bool(b)) => Ok(Some(b.to_string())),
            Some(_) => bail!("frontmatter `{key}` must be a scalar"),
        }
    }

    fn date(&self, key: &str) -> Result<Option<NaiveDate>> {
        match self.string(key)? {
            None => Ok(None),
            Some(s) => NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .map(Some)
                .with_context(|| format!("frontmatter `{key}` must be a YYYY-MM-DD date")),
        }
    }

    fn tags(&self) -> Result<Vec<String>> {
        match self.get("tags") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Sequence(seq)) => seq
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    Value::Number(n) => Ok(n.to_string()),
                    _ => bail!("frontmatter `tags` entries must be strings"),
                })
                .collect(),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()),
            Some(_) => bail!("frontmatter `tags` must be a list or string"),
        }
    }
}

/// Split text after the opening fence into (yaml, body) at the first line that is exactly `---`.
fn split_at_closing_fence(rest: &str) -> Option<(&str, &str)> {
    if let Some(body) = rest.strip_prefix("---\n") {
        return Some(("", body));
    }
    if rest == FENCE {
        return Some(("", ""));
    }
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches('\n') == FENCE {
            let body = &rest[offset + line.len()..];
            return Some((&rest[..offset], body));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const CARD: &str = "---\ntype: card\nsm_id: 1042\ndue: 2026-09-25\ninterval: 6\nprio: 28\nsource: \"[[citrus-vocab]]\"\nrange: 2210-2380\ncustom_key: keep me\n---\nQ: question?\n\nA: answer\n";

    #[test]
    fn parses_typed_fields() {
        let doc = Document::parse(CARD).unwrap();
        let meta = doc.meta().unwrap();
        assert_eq!(meta.kind, ItemType::Card);
        assert_eq!(meta.sm_id, Some(1042));
        assert_eq!(meta.due.map(|d| d.to_string()), Some("2026-09-25".to_string()));
        assert_eq!(meta.interval, Some(6));
        assert_eq!(meta.prio, 28);
        assert_eq!(meta.source.as_deref(), Some("[[citrus-vocab]]"));
        assert_eq!(meta.range.as_deref(), Some("2210-2380"));
        assert_eq!(doc.body, "Q: question?\n\nA: answer\n");
    }

    #[test]
    fn defaults_when_fields_absent() {
        let doc = Document::parse("---\ntype: card\n---\nQ: q\nA: a\n").unwrap();
        let meta = doc.meta().unwrap();
        assert_eq!(meta.sm_id, None);
        assert_eq!(meta.due, None);
        assert_eq!(meta.interval, None);
        assert_eq!(meta.prio, 50);
        assert!(meta.tags.is_empty());
    }

    #[test]
    fn round_trip_preserves_unknown_keys_and_order() {
        let doc = Document::parse(CARD).unwrap();
        let out = doc.serialize().unwrap();
        let again = Document::parse(&out).unwrap();
        assert_eq!(again.front, doc.front);
        assert_eq!(again.body, doc.body);
        let keys: Vec<String> = again
            .front
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            keys,
            ["type", "sm_id", "due", "interval", "prio", "source", "range", "custom_key"]
        );
        assert!(out.contains("custom_key: keep me\n"));
        assert!(out.ends_with("---\nQ: question?\n\nA: answer\n"));
    }

    #[test]
    fn set_sm_id_inserts_after_type_and_survives_round_trip() {
        let mut doc = Document::parse("---\ntype: card\nprio: 40\n---\nQ: q\nA: a\n").unwrap();
        doc.set_sm_id(7);
        let out = doc.serialize().unwrap();
        assert!(out.starts_with("---\ntype: card\nsm_id: 7\nprio: 40\n---\n"), "{out}");
        assert_eq!(Document::parse(&out).unwrap().meta().unwrap().sm_id, Some(7));
    }

    #[test]
    fn set_schedule_inserts_after_sm_id_when_absent_and_keeps_position_otherwise() {
        let mut doc =
            Document::parse("---\ntype: card\nsm_id: 7\nprio: 40\nextra: x\n---\nQ: q\nA: a\n").unwrap();
        doc.set_schedule(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(), 12);
        let out = doc.serialize().unwrap();
        assert!(
            out.starts_with("---\ntype: card\nsm_id: 7\ndue: 2026-10-02\ninterval: 12\nprio: 40\nextra: x\n---\n"),
            "{out}"
        );
        let meta = Document::parse(&out).unwrap().meta().unwrap();
        assert_eq!(meta.due, NaiveDate::from_ymd_opt(2026, 10, 2));
        assert_eq!(meta.interval, Some(12));

        let mut doc = Document::parse(CARD).unwrap();
        doc.set_schedule(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(), 12);
        let out = doc.serialize().unwrap();
        let keys: Vec<String> = Document::parse(&out)
            .unwrap()
            .front
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
        assert_eq!(keys, ["type", "sm_id", "due", "interval", "prio", "source", "range", "custom_key"]);
        assert!(out.contains("due: 2026-10-02\ninterval: 12\n"), "{out}");
        assert!(out.contains("custom_key: keep me\n"), "{out}");
    }

    #[test]
    fn set_schedule_without_sm_id_appends_after_type() {
        let mut doc = Document::parse("---\ntype: card\nprio: 40\n---\nQ: q\nA: a\n").unwrap();
        doc.set_schedule(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(), 1);
        let out = doc.serialize().unwrap();
        assert!(out.starts_with("---\ntype: card\ndue: 2026-10-02\ninterval: 1\nprio: 40\n---\n"), "{out}");
    }

    #[test]
    fn article_with_read_pos_and_tags_list() {
        let doc =
            Document::parse("---\ntype: article\nprio: 20\nread_pos: 2210\ntags: [citrus, a b]\n---\n# T\n")
                .unwrap();
        let meta = doc.meta().unwrap();
        assert_eq!(meta.kind, ItemType::Article);
        assert_eq!(meta.read_pos, Some(2210));
        assert_eq!(meta.tags, vec!["citrus".to_string(), "a b".to_string()]);
    }

    #[test]
    fn rejects_missing_frontmatter_or_unknown_type() {
        assert!(Document::parse("no front\n").is_err());
        assert!(Document::parse("---\ntype: recipe\n---\nx\n").is_err());
        assert!(Document::parse("---\nprio: 1\n---\nx\n").is_err());
    }

    #[test]
    fn tolerates_crlf_and_missing_trailing_newline() {
        let doc = Document::parse("---\r\ntype: card\r\n---\r\nQ: q\r\nA: a").unwrap();
        assert_eq!(doc.meta().unwrap().kind, ItemType::Card);
        assert_eq!(doc.body, "Q: q\nA: a");
    }
}

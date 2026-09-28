//! Stats screen maths (M8): SuperMemo's Statistics fields grain can compute, from plain item and journal rows.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate};

use crate::db::{GradeRow, ItemRow};
use crate::vault::frontmatter::ItemType;

/// Stands in for a value there is no data for. U+2014.
const DASH: &str = "—";

/// The local calendar date of an ISO 8601 UTC `graded_at`: the `review_date` the API gets.
pub fn local_date(graded_at: &str) -> Result<NaiveDate> {
    let utc = DateTime::parse_from_rfc3339(graded_at)
        .with_context(|| format!("graded_at `{graded_at}` is not RFC 3339"))?;
    Ok(utc.with_timezone(&Local).date_naive())
}

/// One snapshot of the collection: every number the stats screen shows, computed once.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub total: usize,
    pub memorized: usize,
    pub pending: usize,
    pub dismissed: usize,
    /// Memorized cards only: the denominator of the repetitions and lapses averages.
    pub memorized_cards: usize,
    pub due_cards: usize,
    pub due_articles: usize,
    /// Cards waiting in the session's final drill; `+N` on `outstanding` when non-zero.
    pub drill: usize,
    pub burden_cards: f64,
    pub burden_articles: f64,
    pub interval_cards: Option<f64>,
    pub interval_articles: Option<f64>,
    pub first_day: Option<NaiveDate>,
    pub period_days: Option<i64>,
    pub grades_total: usize,
    pub grades_today: usize,
    pub lapses_total: usize,
    pub lapses_today: usize,
    /// Journal rows per local date, every date.
    pub graded_by_day: BTreeMap<NaiveDate, usize>,
    /// Active items per `due` date (only dates after today are read by `calendar`).
    pub due_by_day: BTreeMap<NaiveDate, usize>,
}

/// One calendar row: past days count grades, today counts what is due, later days count `due`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayRow {
    pub date: NaiveDate,
    pub count: usize,
    pub is_today: bool,
}

/// Everything the stats screen shows, from the item rows and the whole journal.
///
/// Fails only when a `graded_at` is not RFC 3339; the item rows cannot fail.
pub fn compute(items: &[ItemRow], grades: &[GradeRow], today: NaiveDate, drill: usize) -> Result<Stats> {
    let graded_ids: BTreeSet<i64> = grades.iter().map(|g| g.sm_id).collect();
    let mut stats = Stats {
        total: items.len(),
        memorized: 0,
        pending: 0,
        dismissed: 0,
        memorized_cards: 0,
        due_cards: 0,
        due_articles: 0,
        drill,
        burden_cards: 0.0,
        burden_articles: 0.0,
        interval_cards: None,
        interval_articles: None,
        first_day: None,
        period_days: None,
        grades_total: 0,
        grades_today: 0,
        lapses_total: 0,
        lapses_today: 0,
        graded_by_day: BTreeMap::new(),
        due_by_day: BTreeMap::new(),
    };
    // Interval sums and counts behind the two means.
    let (mut card_sum, mut card_count) = (0i64, 0usize);
    let (mut article_sum, mut article_count) = (0i64, 0usize);
    for row in items {
        if row.done.is_some() {
            stats.dismissed += 1;
            continue;
        }
        let card = row.kind == ItemType::Card;
        if row.interval.is_some() || graded_ids.contains(&row.sm_id) {
            stats.memorized += 1;
            if card {
                stats.memorized_cards += 1;
            }
        } else {
            stats.pending += 1;
        }
        if row.due.is_none_or(|d| d <= today) {
            if card {
                stats.due_cards += 1;
            } else {
                stats.due_articles += 1;
            }
        }
        if let Some(interval) = row.interval {
            // An interval of zero or less is kept out of the burden, as SuperMemo's
            // `Σ 1/interval` has nothing to add for it.
            let load = if interval > 0 { 1.0 / interval as f64 } else { 0.0 };
            if card {
                card_sum += interval;
                card_count += 1;
                stats.burden_cards += load;
            } else {
                article_sum += interval;
                article_count += 1;
                stats.burden_articles += load;
            }
        }
        if let Some(due) = row.due {
            *stats.due_by_day.entry(due).or_insert(0) += 1;
        }
    }
    stats.interval_cards = mean(card_sum, card_count);
    stats.interval_articles = mean(article_sum, article_count);
    for row in grades {
        let day = local_date(&row.graded_at)
            .with_context(|| format!("journal row for sm_id {}", row.sm_id))?;
        *stats.graded_by_day.entry(day).or_insert(0) += 1;
        stats.grades_total += 1;
        let lapse = row.grade < 3;
        if lapse {
            stats.lapses_total += 1;
        }
        if day == today {
            stats.grades_today += 1;
            if lapse {
                stats.lapses_today += 1;
            }
        }
        stats.first_day = Some(stats.first_day.map_or(day, |first| first.min(day)));
    }
    stats.period_days = stats.first_day.map(|first| (today - first).num_days());
    Ok(stats)
}

impl Stats {
    /// The eight `(label, value)` pairs, left column rows 0–3 then right column rows 0–3.
    pub fn fields(&self) -> [(&'static str, String); 8] {
        [
            ("first day", self.first_day_field()),
            (
                "memorized",
                format!(
                    "{} · pending {} · dismissed {}",
                    self.memorized, self.pending, self.dismissed
                ),
            ),
            (
                "repetitions",
                match self.journal_avg(self.grades_total) {
                    Some(avg) => format!("{avg:.1} avg · {} total", self.grades_total),
                    None => format!("{DASH} · {} total", self.grades_total),
                },
            ),
            (
                "lapses",
                match self.journal_avg(self.lapses_total) {
                    Some(avg) => format!("{avg:.2} avg · {} today", self.lapses_today),
                    None => format!("{DASH} · {} today", self.lapses_today),
                },
            ),
            ("outstanding", self.outstanding_field()),
            (
                "burden",
                format!("{:.2} + {:.2} /day", self.burden_cards, self.burden_articles),
            ),
            ("measured FI", self.forgetting_index_field()),
            (
                "interval",
                format!(
                    "{} (I) · {} (T)",
                    days_field(self.interval_cards),
                    days_field(self.interval_articles)
                ),
            ),
        ]
    }

    /// One row per day from `today - past` to `today + future`, oldest first.
    pub fn calendar(&self, today: NaiveDate, past: usize, future: usize) -> Vec<DayRow> {
        // A date overflow is impossible with a terminal-sized window; if it ever happens
        // the range starts (or stops) early rather than panicking.
        let (mut day, steps) = match today.checked_sub_days(Days::new(past as u64)) {
            Some(start) => (start, past.saturating_add(future)),
            None => (today, future),
        };
        let mut rows = Vec::with_capacity(steps.saturating_add(1));
        for _ in 0..=steps {
            let count = if day < today {
                self.graded_by_day.get(&day).copied().unwrap_or(0)
            } else if day == today {
                self.due_cards + self.due_articles
            } else {
                self.due_by_day.get(&day).copied().unwrap_or(0)
            };
            rows.push(DayRow {
                date: day,
                count,
                is_today: day == today,
            });
            let Some(next) = day.checked_add_days(Days::new(1)) else {
                break;
            };
            day = next;
        }
        rows
    }

    /// A journal average over memorized cards: `None` — the dash — with an empty journal
    /// or no memorized card to divide by.
    fn journal_avg(&self, part: usize) -> Option<f64> {
        if self.grades_total == 0 {
            return None;
        }
        mean(part as i64, self.memorized_cards)
    }

    fn first_day_field(&self) -> String {
        match (self.first_day, self.period_days) {
            (Some(day), Some(days)) => format!("{day} · {days} d"),
            _ => DASH.to_string(),
        }
    }

    fn outstanding_field(&self) -> String {
        let mut value = format!("{}+{}", self.due_cards, self.due_articles);
        if self.drill > 0 {
            value.push_str(&format!("+{}", self.drill));
        }
        value
    }

    /// `lapses × 100 / rows` over the whole journal, then over today's rows.
    fn forgetting_index_field(&self) -> String {
        let Some(all) = percent(self.lapses_total, self.grades_total) else {
            return DASH.to_string();
        };
        let today = match percent(self.lapses_today, self.grades_today) {
            Some(pct) => format!("{pct:.1} %"),
            None => DASH.to_string(),
        };
        format!("{all:.1} % ({today})")
    }
}

/// The mean, or `None` when there is nothing to divide by.
fn mean(sum: i64, count: usize) -> Option<f64> {
    (count > 0).then(|| sum as f64 / count as f64)
}

/// `part` as a percentage of `whole`, or `None` when `whole` is zero.
fn percent(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 * 100.0 / whole as f64)
}

/// A mean interval as `6.0 d`, or the dash when that side has no item with an interval.
fn days_field(days: Option<f64>) -> String {
    match days {
        Some(d) => format!("{d:.1} d"),
        None => DASH.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::{GradeRow, ItemRow};
    use crate::vault::frontmatter::ItemType;
    use chrono::{DateTime, Local, NaiveDate};

    fn today() -> NaiveDate {
        date("2026-09-20")
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn item(
        sm_id: i64,
        kind: ItemType,
        due: Option<&str>,
        interval: Option<i64>,
        done: Option<&str>,
    ) -> ItemRow {
        ItemRow {
            sm_id,
            path: format!("{sm_id}.md"),
            kind,
            due: due.map(date),
            interval,
            prio: 50,
            read_pos: None,
            tags: String::new(),
            mtime: 0,
            title: None,
            a_factor: None,
            done: done.map(date),
            source: None,
            range: None,
            url: None,
            imported: None,
        }
    }

    fn grade(sm_id: i64, grade: u8, graded_at: &str) -> GradeRow {
        GradeRow {
            sm_id,
            grade,
            graded_at: graded_at.to_string(),
        }
    }

    /// The eight fixture-vault rows at `2026-09-20`, as after `App::open`.
    fn fixture_items() -> Vec<ItemRow> {
        vec![
            item(1, ItemType::Card, Some("2026-09-01"), Some(1), None),
            item(2, ItemType::Card, Some("2026-09-30"), Some(14), None),
            item(3, ItemType::Card, Some("2026-09-10"), Some(3), None),
            item(4, ItemType::Card, Some("2026-09-25"), Some(6), None),
            item(5, ItemType::Card, None, None, None),
            item(6, ItemType::Card, None, None, None),
            item(7, ItemType::Article, None, None, None),
            item(8, ItemType::Article, None, None, None),
        ]
    }

    #[test]
    fn fixture_shape_gives_the_spec_values() {
        let stats = compute(&fixture_items(), &[], today(), 0).unwrap();
        assert_eq!(stats.total, 8);
        assert_eq!(
            stats.fields(),
            [
                ("first day", "—".to_string()),
                ("memorized", "4 · pending 4 · dismissed 0".to_string()),
                ("repetitions", "— · 0 total".to_string()),
                ("lapses", "— · 0 today".to_string()),
                ("outstanding", "4+2".to_string()),
                ("burden", "1.57 + 0.00 /day".to_string()),
                ("measured FI", "—".to_string()),
                ("interval", "6.0 d (I) · — (T)".to_string()),
            ]
        );
    }

    #[test]
    fn drill_count_joins_outstanding() {
        let stats = compute(&fixture_items(), &[], today(), 1).unwrap();
        assert_eq!(stats.drill, 1);
        assert_eq!(stats.fields()[4], ("outstanding", "4+2+1".to_string()));
    }

    #[test]
    fn done_items_are_dismissed_and_never_due() {
        let items = vec![item(1, ItemType::Card, None, None, Some("2026-09-01"))];
        let stats = compute(&items, &[], today(), 0).unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.dismissed, 1);
        assert_eq!(stats.memorized, 0);
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.due_cards, 0);
        assert_eq!(stats.due_articles, 0);
    }

    #[test]
    fn a_graded_card_without_interval_is_memorized() {
        let items = vec![item(1, ItemType::Card, None, None, None)];
        let grades = vec![grade(1, 4, "2026-09-19T12:00:00Z")];
        let stats = compute(&items, &grades, today(), 0).unwrap();
        assert_eq!(stats.memorized, 1);
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.memorized_cards, 1);
    }

    #[test]
    fn fi_lapses_and_repetitions_from_grades() {
        let items = vec![
            item(1, ItemType::Card, None, Some(2), None),
            item(2, ItemType::Card, None, Some(4), None),
        ];
        // T12:00:00Z so the local date is the same in any zone from UTC−10 to UTC+12.
        let grades = vec![
            grade(1, 4, "2026-09-19T12:00:00Z"),
            grade(1, 1, "2026-09-20T12:00:00Z"),
            grade(2, 5, "2026-09-20T12:00:00Z"),
        ];
        let stats = compute(&items, &grades, today(), 0).unwrap();
        let fields = stats.fields();
        assert_eq!(fields[0], ("first day", "2026-09-19 · 1 d".to_string()));
        assert_eq!(fields[2], ("repetitions", "1.5 avg · 3 total".to_string()));
        assert_eq!(fields[3], ("lapses", "0.50 avg · 1 today".to_string()));
        assert_eq!(fields[5], ("burden", "0.75 + 0.00 /day".to_string()));
        assert_eq!(fields[6], ("measured FI", "33.3 % (50.0 %)".to_string()));
        assert_eq!(fields[7], ("interval", "3.0 d (I) · — (T)".to_string()));
    }

    #[test]
    fn fi_today_is_a_dash_without_a_grade_today() {
        let items = vec![item(1, ItemType::Card, None, Some(2), None)];
        let grades = vec![grade(1, 5, "2026-09-18T12:00:00Z")];
        let stats = compute(&items, &grades, today(), 0).unwrap();
        assert_eq!(stats.fields()[6], ("measured FI", "0.0 % (—)".to_string()));
    }

    #[test]
    fn article_side_of_burden_and_interval() {
        let items = vec![item(1, ItemType::Article, None, Some(7), None)];
        let stats = compute(&items, &[], today(), 0).unwrap();
        let fields = stats.fields();
        assert_eq!(
            fields[1],
            ("memorized", "1 · pending 0 · dismissed 0".to_string())
        );
        assert_eq!(fields[2], ("repetitions", "— · 0 total".to_string()));
        assert_eq!(fields[5], ("burden", "0.00 + 0.14 /day".to_string()));
        assert_eq!(fields[7], ("interval", "— (I) · 7.0 d (T)".to_string()));
    }

    #[test]
    fn calendar_buckets_past_grades_and_future_due() {
        let items = vec![
            item(1, ItemType::Card, Some("2026-09-10"), None, None),
            item(2, ItemType::Card, None, None, None),
            item(3, ItemType::Card, Some("2026-09-21"), None, None),
            item(4, ItemType::Card, Some("2026-09-30"), None, None),
            item(5, ItemType::Card, Some("2026-09-21"), None, Some("2026-09-19")),
        ];
        let grades = vec![
            grade(1, 4, "2026-09-19T12:00:00Z"),
            grade(1, 4, "2026-09-20T12:00:00Z"),
            grade(2, 2, "2026-09-20T12:00:00Z"),
        ];
        let stats = compute(&items, &grades, today(), 0).unwrap();
        let rows = stats.calendar(today(), 2, 2);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows.iter().map(|r| r.date).collect::<Vec<_>>(),
            vec![
                date("2026-09-18"),
                date("2026-09-19"),
                date("2026-09-20"),
                date("2026-09-21"),
                date("2026-09-22"),
            ]
        );
        // Today counts the two due items (the overdue card and the undated one), not
        // today's two grades; the done card never counts on 09-21.
        assert_eq!(
            rows.iter().map(|r| r.count).collect::<Vec<_>>(),
            vec![0, 1, 2, 1, 0]
        );
        assert_eq!(
            rows.iter().map(|r| r.is_today).collect::<Vec<_>>(),
            vec![false, false, true, false, false]
        );
    }

    #[test]
    fn calendar_with_zero_rows_each_way_is_just_today() {
        let stats = compute(&fixture_items(), &[], today(), 0).unwrap();
        let rows = stats.calendar(today(), 0, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date, today());
        assert!(rows[0].is_today);
    }

    #[test]
    fn empty_input_is_all_dashes_and_no_panic() {
        let stats = compute(&[], &[], today(), 0).unwrap();
        assert_eq!(stats.total, 0);
        assert_eq!(
            stats.fields(),
            [
                ("first day", "—".to_string()),
                ("memorized", "0 · pending 0 · dismissed 0".to_string()),
                ("repetitions", "— · 0 total".to_string()),
                ("lapses", "— · 0 today".to_string()),
                ("outstanding", "0+0".to_string()),
                ("burden", "0.00 + 0.00 /day".to_string()),
                ("measured FI", "—".to_string()),
                ("interval", "— (I) · — (T)".to_string()),
            ]
        );
    }

    #[test]
    fn local_date_converts_from_utc() {
        let raw = "2026-09-19T12:00:00Z";
        let expected = DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Local)
            .date_naive();
        assert_eq!(local_date(raw).unwrap(), expected);
        let err = local_date("nope").unwrap_err();
        assert!(
            format!("{err:#}").contains("not RFC 3339"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn a_bad_graded_at_names_the_sm_id() {
        let items = vec![item(1, ItemType::Card, None, Some(2), None)];
        let grades = vec![grade(7, 4, "nope")];
        let err = compute(&items, &grades, today(), 0).unwrap_err();
        assert!(
            format!("{err:#}").contains("journal row for sm_id 7"),
            "unexpected error: {err:#}"
        );
    }
}

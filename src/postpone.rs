//! Auto-postpone (M9): SuperMemo's rule for shrinking the backlog, as pure maths.
//!
//! No `Db`, no filesystem, no clock: `plan` takes the overdue rows in queue order and
//! `today`, and says which items move and to which date. `app.rs` does the writing.

use chrono::{Days, NaiveDate};

use crate::db::ItemRow;
use crate::vault::frontmatter::ItemType;

/// Whether a launch postpones the backlog, and how many overdue items it keeps due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Postpone {
    Off,
    Keep(usize),
}

/// The keep count of `--postpone` when the flag is absent.
pub const DEFAULT_KEEP: usize = 50;

/// One item to postpone: its row identity, its file and the date to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub sm_id: i64,
    pub path: String,
    pub due: NaiveDate,
}

/// What a launch should do: the items to move, and how many stayed due.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub moves: Vec<Move>,
    pub kept: usize,
}

/// Plan the postpone for `overdue` (already in queue order: `prio ASC, due ASC, sm_id ASC`).
///
/// Protected rows are skipped and never count toward the keep count; of what is left, the
/// first `keep` stay due and the rest move to `today + delay_days(interval)`.
pub fn plan(overdue: &[ItemRow], today: NaiveDate, postpone: Postpone) -> Plan {
    let Postpone::Keep(keep) = postpone else {
        return Plan::default();
    };
    let mut candidates = overdue.iter().filter(|row| !protected(row));
    let kept = candidates.by_ref().take(keep).count();
    let moves = candidates
        .filter_map(|row| {
            // Only an absurd `today` can overflow; such a row is left alone rather than clamped.
            let due = today.checked_add_days(Days::new(delay_days(row.interval)))?;
            Some(Move {
                sm_id: row.sm_id,
                path: row.path.clone(),
                due,
            })
        })
        .collect();
    Plan { moves, kept }
}

/// Days to push a postponed item out: a tenth of its interval, rounded half away from
/// zero, clamped to `1..=30`. A missing interval counts as 1, so the delay is one day.
fn delay_days(interval: Option<i64>) -> u64 {
    let tenth = (interval.unwrap_or(1) as f64 * 0.1).round();
    // Clamped while still a float, so the cast is always an exact 1..=30.
    tenth.clamp(1.0, 30.0) as u64
}

/// SuperMemo's A-Factor 1.01: an article marked never to be postponed.
fn protected(row: &ItemRow) -> bool {
    row.kind == ItemType::Article && row.a_factor.is_some_and(|a| a <= 1.01)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::ItemRow;
    use crate::vault::frontmatter::ItemType;
    use chrono::NaiveDate;

    fn today() -> NaiveDate {
        date("2026-09-20")
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// An overdue row, as `plan` sees it: only the fields the maths reads carry a value.
    fn row(sm_id: i64, kind: ItemType, prio: i64, interval: Option<i64>, a_factor: Option<f64>) -> ItemRow {
        ItemRow {
            sm_id,
            path: format!("{sm_id}.md"),
            kind,
            due: Some(date("2026-09-01")),
            interval,
            prio,
            read_pos: None,
            tags: String::new(),
            mtime: 0,
            title: None,
            a_factor,
            done: None,
            source: None,
            range: None,
            url: None,
            imported: None,
        }
    }

    fn two_cards() -> Vec<ItemRow> {
        vec![
            row(1, ItemType::Card, 35, Some(3), None),
            row(2, ItemType::Card, 60, Some(1), None),
        ]
    }

    #[test]
    fn keep_one_moves_the_rest() {
        let plan = plan(&two_cards(), today(), Postpone::Keep(1));

        assert_eq!(plan.kept, 1);
        assert_eq!(
            plan.moves,
            vec![Move {
                sm_id: 2,
                path: "2.md".to_string(),
                due: date("2026-09-21"),
            }]
        );
    }

    #[test]
    fn keep_at_least_len_moves_nothing() {
        for keep in [2, 50] {
            let plan = plan(&two_cards(), today(), Postpone::Keep(keep));

            assert!(plan.moves.is_empty(), "keep {keep} moved {:?}", plan.moves);
            assert_eq!(plan.kept, 2, "keep {keep}");
        }
    }

    #[test]
    fn keep_zero_moves_all() {
        let plan = plan(&two_cards(), today(), Postpone::Keep(0));

        assert_eq!(plan.kept, 0);
        assert_eq!(plan.moves.iter().map(|m| m.sm_id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn off_plans_nothing() {
        let plan = plan(&two_cards(), today(), Postpone::Off);

        assert!(plan.moves.is_empty());
        assert_eq!(plan.kept, 0);
    }

    #[test]
    fn delay_is_a_tenth_of_the_interval_clamped() {
        let rows: Vec<ItemRow> = [Some(1), Some(3), Some(45), Some(400), None]
            .into_iter()
            .enumerate()
            .map(|(i, interval)| row(i as i64 + 1, ItemType::Card, 50, interval, None))
            .collect();

        let plan = plan(&rows, today(), Postpone::Keep(0));

        assert_eq!(
            plan.moves.iter().map(|m| m.due).collect::<Vec<_>>(),
            vec![
                date("2026-09-21"),
                date("2026-09-21"),
                date("2026-09-25"),
                date("2026-10-20"),
                date("2026-09-21"),
            ]
        );
    }

    #[test]
    fn low_a_factor_articles_are_protected_and_not_counted() {
        let rows = vec![
            row(1, ItemType::Article, 10, Some(7), Some(1.01)),
            row(2, ItemType::Card, 20, Some(3), None),
            row(3, ItemType::Card, 30, Some(3), None),
        ];

        let plan = plan(&rows, today(), Postpone::Keep(1));

        assert_eq!(plan.moves.iter().map(|m| m.sm_id).collect::<Vec<_>>(), vec![3]);
        assert_eq!(plan.kept, 1);

        // An article at the default a_factor is an ordinary candidate.
        let ordinary = vec![
            row(4, ItemType::Article, 10, Some(7), None),
            row(5, ItemType::Article, 20, Some(7), Some(1.5)),
        ];
        let default_factor = super::plan(&ordinary, today(), Postpone::Keep(0));

        assert_eq!(
            default_factor.moves.iter().map(|m| m.sm_id).collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert_eq!(default_factor.kept, 0);
    }

    #[test]
    fn a_date_that_would_overflow_skips_the_row() {
        let rows = vec![row(1, ItemType::Card, 35, Some(3), None)];

        let plan = plan(&rows, NaiveDate::MAX, Postpone::Keep(0));

        assert!(plan.moves.is_empty());
        assert_eq!(plan.kept, 0);
    }

    #[test]
    fn empty_input() {
        let plan = plan(&[], today(), Postpone::Keep(5));

        assert!(plan.moves.is_empty());
        assert_eq!(plan.kept, 0);
    }
}

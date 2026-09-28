//! Stats screen: the eight SuperMemo numbers grain can compute, in two columns
//! of four rows, then one blank row and a workload calendar, one row per day
//! with today in the middle. A pure function of `App::stats`.

use chrono::NaiveDate;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::AMBER;
use crate::app::App;
use crate::stats::{DayRow, Stats};

pub const HINTS: &[(&str, &str)] = &[("s/esc", "back"), ("q", "quit")];

/// Rows the number block takes in two columns, when the content area has room.
const NUMBER_ROWS: u16 = 4;

/// Rows it takes stacked into one column: one field each.
const STACKED_ROWS: u16 = 8;

/// The narrowest width that fits two columns. A half of 40 cells is exactly what
/// the widest left value (`memorized …`, 13 + 27) needs, so below 80 the halves
/// would clip it with no ellipsis and no wrap. The fields stack instead.
const TWO_COLUMN_WIDTH: u16 = 80;

/// Cells the dim label takes before its value.
const LABEL_WIDTH: usize = 13;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(stats) = app.stats.as_ref() else {
        return;
    };
    let block = block_rows(area.width);
    number_rows(frame, area, stats, block);
    calendar(frame, area, stats, app.today(), block);
}

/// Rows the number block claims before the blank row under it.
fn block_rows(width: u16) -> u16 {
    if width >= TWO_COLUMN_WIDTH {
        NUMBER_ROWS
    } else {
        STACKED_ROWS
    }
}

/// The eight fields: `fields()[0..4]` down the left half and `[4..8]` down the
/// right at 80 columns or more, otherwise all eight in `fields()` order, one row
/// each. A short area simply loses its lower rows.
fn number_rows(frame: &mut Frame, area: Rect, stats: &Stats, block: u16) {
    let height = area.height.min(block);
    let fields = stats.fields();
    if block == STACKED_ROWS {
        for r in 0..height {
            let (label, value) = &fields[r as usize];
            let row = Rect { y: area.y + r, height: 1, ..area };
            frame.render_widget(field_line(label, value, false), row);
        }
        return;
    }
    let columns = Rect { height, ..area };
    let [left, right] =
        columns.layout(&Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]));
    for r in 0..height {
        let pair = [
            (left, &fields[r as usize], false),
            (right, &fields[4 + r as usize], true),
        ];
        for (column, (label, value), pad) in pair {
            let row = Rect {
                y: column.y + r,
                height: 1,
                ..column
            };
            frame.render_widget(field_line(label, value, pad), row);
        }
    }
}

/// `memorized    4 · pending 4 · dismissed 0`: a dim label padded to `LABEL_WIDTH`,
/// then the value. `pad` adds one leading cell, which the right column takes so the
/// two halves keep a gap even when the left value fills its own half exactly.
fn field_line(label: &str, value: &str, pad: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(3);
    if pad {
        spans.push(Span::from(" "));
    }
    spans.push(Span::from(format!("{label:<width$}", width = LABEL_WIDTH)).dim());
    spans.push(Span::from(value.to_string()));
    Line::from(spans)
}

/// The calendar fills the rows left after the number block and one blank row,
/// today in the middle: an odd remainder gives the extra day to the future.
fn calendar(frame: &mut Frame, area: Rect, stats: &Stats, today: NaiveDate, block: u16) {
    let offset = block + 1;
    let left = area.height.saturating_sub(offset);
    if left == 0 {
        return;
    }
    let past = (left - 1) / 2;
    let future = left - 1 - past;
    let bottom = area.y.saturating_add(area.height);
    for (i, day) in stats.calendar(today, past as usize, future as usize).iter().enumerate() {
        let y = area.y + offset + i as u16;
        if y >= bottom {
            break;
        }
        let row = Rect { y, height: 1, ..area };
        frame.render_widget(calendar_line(day, today, area.width), row);
    }
}

/// `▎ 09-20 Sun  ▮▮▮▮▮▮ 6`: a two-cell gutter, amber on today; the date, dim in
/// the past, plain in the future, amber on today; then one `▮` per counted item,
/// cut to the cells left on the row, and the count. A zero day shows a dim `0`
/// and no bar, so the row reads as empty at a glance.
fn calendar_line(day: &DayRow, today: NaiveDate, width: u16) -> Line<'static> {
    let date = day.date.format("%m-%d %a").to_string();
    let date_cells = date.chars().count();
    let date = Span::from(date);
    let (gutter, date) = if day.is_today {
        (Span::from("▎ ").fg(AMBER), date.fg(AMBER))
    } else if day.date < today {
        (Span::from("  "), date.dim())
    } else {
        (Span::from("  "), date)
    };
    let mut spans = vec![gutter, date, Span::from("  ")];
    if day.count == 0 {
        spans.push(Span::from("0").dim());
        return Line::from(spans);
    }
    let count = day.count.to_string();
    // The bar gets what the gutter, the date, the two spaces before it, the space
    // after it and the count itself leave on the row.
    let room = (width as usize).saturating_sub(2 + date_cells + 2 + 1 + count.chars().count());
    spans.push(Span::from("▮".repeat(day.count.min(room))));
    spans.push(Span::from(" "));
    spans.push(Span::from(count));
    Line::from(spans)
}

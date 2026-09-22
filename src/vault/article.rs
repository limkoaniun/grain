//! The read screen's view of an article body: paragraphs, words and character
//! offsets. Pure functions over `&str`; offsets count Unicode scalar values from
//! the start of the body, matching `read_pos` and `range` in the file format.

/// A half-open span of the body in character offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A paragraph is a span: a maximal run of non-blank lines.
pub type Paragraph = Span;

impl Span {
    /// The text this span covers. Out-of-range offsets clamp to the body.
    pub fn text<'a>(&self, body: &'a str) -> &'a str {
        let (b0, b1) = (byte_at(body, self.start), byte_at(body, self.end.max(self.start)));
        &body[b0..b1]
    }

    /// `start-end`, the `range` frontmatter form.
    pub fn range_string(&self) -> String {
        format!("{}-{}", self.start, self.end)
    }

    /// Parse the `range` frontmatter form. `None` for anything else, including `end < start`.
    pub fn parse_range(s: &str) -> Option<Span> {
        let (a, b) = s.trim().split_once('-')?;
        let start: usize = a.trim().parse().ok()?;
        let end: usize = b.trim().parse().ok()?;
        (end >= start).then_some(Span { start, end })
    }
}

/// Byte index of character offset `off`, clamped to the end of `body`.
fn byte_at(body: &str, off: usize) -> usize {
    body.char_indices().nth(off).map_or(body.len(), |(b, _)| b)
}

/// Paragraphs: maximal runs of lines that are not blank after trimming.
/// The span ends before the last line's newline.
pub fn split_paragraphs(body: &str) -> Vec<Paragraph> {
    let mut out = Vec::new();
    let mut current: Option<Paragraph> = None;
    let mut off = 0usize;
    for line in body.split('\n') {
        let len = line.chars().count();
        if line.trim().is_empty() {
            if let Some(p) = current.take() {
                out.push(p);
            }
        } else {
            match &mut current {
                Some(p) => p.end = off + len,
                None => current = Some(Span { start: off, end: off + len }),
            }
        }
        off += len + 1;
    }
    if let Some(p) = current {
        out.push(p);
    }
    out
}

/// Index of the paragraph containing `off` (a blank line belongs to the next one).
/// Beyond the body, or with no paragraphs, the first paragraph.
pub fn paragraph_at(paragraphs: &[Paragraph], off: usize) -> usize {
    paragraphs.iter().position(|p| off <= p.end).unwrap_or(0)
}

/// Whitespace-separated words inside a paragraph, as spans of the body.
pub fn words(body: &str, paragraph: &Paragraph) -> Vec<Span> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in body.chars().enumerate().skip(paragraph.start).take(paragraph.end - paragraph.start) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push(Span { start: s, end: i });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(Span { start: s, end: paragraph.end });
    }
    out
}

/// `(question, answer)` for a cloze: the paragraph with `hidden` replaced by `[...]`, and the hidden text.
pub fn cloze_text(body: &str, paragraph: &Paragraph, hidden: Span) -> (String, String) {
    let before = Span { start: paragraph.start, end: hidden.start }.text(body);
    let after = Span { start: hidden.end, end: paragraph.end }.text(body);
    (format!("{before}[...]{after}"), hidden.text(body).to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const BODY: &str = "# Citrus vocabulary\n\nA long-form note on citrus species and their culinary names.\n\nPomelo is the largest citrus fruit and an ancestor of the grapefruit.\nKumquat, unusually, has a sweet peel and sour flesh.\n";

    #[test]
    fn paragraphs_are_runs_of_non_blank_lines_with_char_offsets() {
        let ps = split_paragraphs(BODY);
        assert_eq!(ps.len(), 3);
        assert_eq!((ps[0].start, ps[0].end), (0, 19));
        assert_eq!(ps[0].text(BODY), "# Citrus vocabulary");
        assert_eq!((ps[1].start, ps[1].end), (21, 81));
        assert_eq!((ps[2].start, ps[2].end), (83, 205));
        assert!(ps[2].text(BODY).starts_with("Pomelo") && ps[2].text(BODY).ends_with("flesh."));
    }

    #[test]
    fn blank_lines_with_spaces_separate_and_leading_trailing_blanks_are_ignored() {
        let ps = split_paragraphs("\n\n  \nfirst\n   \nsecond line a\nsecond line b\n\n\n");
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].text("\n\n  \nfirst\n   \nsecond line a\nsecond line b\n\n\n"), "first");
        assert_eq!(ps[1].start, 15);
        assert!(split_paragraphs("").is_empty());
        assert!(split_paragraphs("\n \n").is_empty());
    }

    #[test]
    fn offsets_count_chars_not_bytes() {
        let body = "柚子 yuzu\n\nsecond";
        let ps = split_paragraphs(body);
        assert_eq!((ps[0].start, ps[0].end), (0, 7));
        assert_eq!((ps[1].start, ps[1].end), (9, 15));
        assert_eq!(ps[1].text(body), "second");
    }

    #[test]
    fn paragraph_at_maps_an_offset_to_its_paragraph_or_the_first() {
        let ps = split_paragraphs(BODY);
        assert_eq!(paragraph_at(&ps, 0), 0);
        assert_eq!(paragraph_at(&ps, 19), 0);
        assert_eq!(paragraph_at(&ps, 20), 1, "a blank line belongs to the next paragraph");
        assert_eq!(paragraph_at(&ps, 83), 2);
        assert_eq!(paragraph_at(&ps, 150), 2);
        assert_eq!(paragraph_at(&ps, 9999), 0, "beyond the body falls back to the first");
        assert_eq!(paragraph_at(&[], 5), 0);
    }

    #[test]
    fn words_are_whitespace_separated_with_offsets_inside_the_body() {
        let ps = split_paragraphs(BODY);
        let ws = words(BODY, &ps[2]);
        assert_eq!(ws[0].text(BODY), "Pomelo");
        assert_eq!((ws[0].start, ws[0].end), (83, 89));
        assert_eq!(ws[11].text(BODY), "grapefruit.");
        assert_eq!((ws[11].start, ws[11].end), (141, 152));
        assert_eq!(ws[12].text(BODY), "Kumquat,", "words continue across the paragraph's lines");
        assert_eq!(ws.len(), 21);
        let ws = words("a  b\n c", &split_paragraphs("a  b\n c")[0]);
        assert_eq!(ws.iter().map(|w| w.text("a  b\n c")).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn cloze_text_replaces_a_span_inside_the_paragraph() {
        let ps = split_paragraphs(BODY);
        let p = &ps[2];
        let (q, a) = cloze_text(BODY, p, Span { start: 83, end: 89 });
        assert_eq!(a, "Pomelo");
        assert!(q.starts_with("[...] is the largest citrus fruit"), "{q}");
        assert!(q.ends_with("sour flesh."), "{q}");
        let (q, a) = cloze_text(BODY, p, Span { start: 141, end: 152 });
        assert_eq!(a, "grapefruit.");
        assert!(q.contains("ancestor of the [...]\nKumquat"), "{q}");
        let (q, a) = cloze_text(BODY, p, Span { start: 199, end: 204 });
        assert_eq!(a, "flesh");
        assert!(q.ends_with("sour [...]."), "{q}");
    }

    #[test]
    fn span_text_and_range_string() {
        let s = Span { start: 83, end: 89 };
        assert_eq!(s.text(BODY), "Pomelo");
        assert_eq!(s.range_string(), "83-89");
        assert_eq!(Span::parse_range("83-89"), Some(Span { start: 83, end: 89 }));
        assert_eq!(Span::parse_range("2210-2380"), Some(Span { start: 2210, end: 2380 }));
        assert_eq!(Span::parse_range("x"), None);
        assert_eq!(Span::parse_range("9-3"), None, "end before start");
    }
}

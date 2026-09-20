//! Card body parsing: `Q:` / `A:` extraction and embed lines.
//!
//! Embeds are kept in place in the data model (question side vs answer side)
//! so later milestones can render them; M0 shows a placeholder line.

use anyhow::{bail, Result};

/// One embedded file reference (`![[x]]` or `![](x)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embed {
    pub target: String,
}

impl Embed {
    /// Placeholder line shown in place of the media in M0.
    pub fn placeholder(&self) -> String {
        let ext = self
            .target
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let kind = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "image",
            "mp3" | "wav" | "ogg" | "m4a" | "flac" | "opus" => "audio",
            _ => "embed",
        };
        format!("[{kind}: {}]", self.target)
    }
}

/// One line of a card side: plain text or an embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Embed(Embed),
}

/// A card body split into its two sides. Markers are stripped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CardBody {
    pub question: Vec<Segment>,
    pub answer: Vec<Segment>,
}

impl CardBody {
    /// First non-empty text line of the question, for listings.
    pub fn title(&self) -> Option<String> {
        self.question.iter().find_map(|s| match s {
            Segment::Text(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            _ => None,
        })
    }
}

const Q_MARK: &str = "Q:";
const A_MARK: &str = "A:";

/// Parse the body (text after frontmatter) of a card file.
///
/// `Q:` and `A:` are recognized only at line start; the first `Q:` and the
/// first `A:` after it win. Question = between them, answer = `A:` to EOF.
pub fn parse_card_body(body: &str) -> Result<CardBody> {
    let lines: Vec<&str> = body.lines().collect();
    let Some(q_idx) = lines.iter().position(|l| l.starts_with(Q_MARK)) else {
        bail!("card has no `Q:` line");
    };
    let a_idx = lines
        .iter()
        .enumerate()
        .skip(q_idx + 1)
        .find(|(_, l)| l.starts_with(A_MARK))
        .map(|(i, _)| i);

    let q_end = a_idx.unwrap_or(lines.len());
    let mut question = vec![strip_marker(lines[q_idx], Q_MARK)];
    question.extend(lines[q_idx + 1..q_end].iter().map(|l| (*l).to_string()));

    let answer: Vec<String> = match a_idx {
        Some(i) => {
            let mut v = vec![strip_marker(lines[i], A_MARK)];
            v.extend(lines[i + 1..].iter().map(|l| (*l).to_string()));
            v
        }
        None => Vec::new(),
    };

    Ok(CardBody {
        question: to_segments(question),
        answer: to_segments(answer),
    })
}

fn strip_marker(line: &str, marker: &str) -> String {
    line[marker.len()..].trim_start().to_string()
}

/// Convert raw lines to segments, trimming blank lines at both ends of the side.
fn to_segments(lines: Vec<String>) -> Vec<Segment> {
    let first = lines.iter().position(|l| !l.trim().is_empty());
    let last = lines.iter().rposition(|l| !l.trim().is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return Vec::new();
    };
    lines[first..=last]
        .iter()
        .map(|l| match parse_embed(l) {
            Some(embed) => Segment::Embed(embed),
            None => Segment::Text(l.trim_end().to_string()),
        })
        .collect()
}

/// Recognize a line that is exactly one embed: `![[target]]` or `![](target)`.
fn parse_embed(line: &str) -> Option<Embed> {
    let l = line.trim();
    if let Some(inner) = l.strip_prefix("![[").and_then(|r| r.strip_suffix("]]")) {
        let target = inner.split('|').next().unwrap_or(inner).trim();
        return (!target.is_empty()).then(|| Embed { target: target.to_string() });
    }
    if let Some(rest) = l.strip_prefix("![") {
        let (_, after_alt) = rest.split_once("](")?;
        let target = after_alt.strip_suffix(')')?.trim();
        return (!target.is_empty()).then(|| Embed { target: target.to_string() });
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn text(s: &str) -> Segment {
        Segment::Text(s.to_string())
    }

    #[test]
    fn extracts_single_line_question_and_answer() {
        let body = parse_card_body("Q: What is 2+2?\n\nA: 4\n").unwrap();
        assert_eq!(body.question, vec![text("What is 2+2?")]);
        assert_eq!(body.answer, vec![text("4")]);
    }

    #[test]
    fn multi_line_question_and_answer_keep_inner_blank_lines() {
        let src = "Q: First line\nsecond line\n\nA: bergamot\n\nGrown in Calabria.\nThe rind matters.\n";
        let body = parse_card_body(src).unwrap();
        assert_eq!(body.question, vec![text("First line"), text("second line")]);
        assert_eq!(
            body.answer,
            vec![text("bergamot"), text(""), text("Grown in Calabria."), text("The rind matters.")]
        );
    }

    #[test]
    fn only_line_start_markers_count_and_first_occurrence_wins() {
        let src = "intro\nQ: real question\nnot Q: this one\nA: answer mentions Q: inline\nQ: literal in answer\nA: also literal\n";
        let body = parse_card_body(src).unwrap();
        assert_eq!(body.question, vec![text("real question"), text("not Q: this one")]);
        assert_eq!(
            body.answer,
            vec![
                text("answer mentions Q: inline"),
                text("Q: literal in answer"),
                text("A: also literal")
            ]
        );
    }

    #[test]
    fn embeds_are_positioned_by_side() {
        let src = "Q: Large citrus?\n![[pomelo.png]]\n\nA: pomelo\n![[pomelo.mp3]]\n![](media/hand.jpg)\n";
        let body = parse_card_body(src).unwrap();
        assert_eq!(
            body.question,
            vec![text("Large citrus?"), Segment::Embed(Embed { target: "pomelo.png".into() })]
        );
        assert_eq!(
            body.answer,
            vec![
                text("pomelo"),
                Segment::Embed(Embed { target: "pomelo.mp3".into() }),
                Segment::Embed(Embed { target: "media/hand.jpg".into() }),
            ]
        );
    }

    #[test]
    fn embed_placeholder_labels_by_extension() {
        assert_eq!(Embed { target: "pomelo.png".into() }.placeholder(), "[image: pomelo.png]");
        assert_eq!(Embed { target: "pomelo.mp3".into() }.placeholder(), "[audio: pomelo.mp3]");
        assert_eq!(Embed { target: "note".into() }.placeholder(), "[embed: note]");
    }

    #[test]
    fn missing_question_is_an_error_but_missing_answer_is_empty() {
        assert!(parse_card_body("just text\nA: x\n").is_err());
        let body = parse_card_body("Q: lonely\n").unwrap();
        assert_eq!(body.question, vec![text("lonely")]);
        assert!(body.answer.is_empty());
    }

    #[test]
    fn question_title_is_first_text_line() {
        let body = parse_card_body("Q:\n![[a.png]]\n  Actual question  \nA: a\n").unwrap();
        assert_eq!(body.title(), Some("Actual question".to_string()));
    }
}

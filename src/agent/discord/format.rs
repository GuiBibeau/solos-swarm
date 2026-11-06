//! What a long answer becomes in Discord: its Markdown made readable (tables
//! and headings, which Discord does not render) and then cut into messages
//! of at most `budget` characters at paragraph boundaries. A code block is
//! never cut in the middle unless it alone is longer than a message, and then
//! every message that holds a piece of it opens and closes its own fence.
//!
//! Pure: text in, strings out.

use crate::agent::markdown::{self, Dialect, closes, fence_run};

const DISCORD: Dialect = Dialect {
    bold: |text| format!("**{text}**"),
    italic: |text| format!("_{text}_"),
    inline: str::to_owned,
    line: str::to_owned,
    code: str::to_owned,
    fence_info: true,
};

/// Discord's Markdown for what a worker wrote.
pub(super) fn render(text: &str) -> String {
    let text: String = text.chars().take(markdown::MAX_INPUT_CHARS).collect();
    markdown::convert(&text, &DISCORD)
}

pub(super) struct Packed {
    pub parts: Vec<String>,
    /// Text was left over once `max_parts` messages were full.
    pub cut: bool,
}

/// Fill up to `max_parts` messages of `budget` characters each with `text`,
/// in order. Paragraphs (and code blocks) go whole into the message they
/// fit, and one that fits no message is cut on its line breaks, then on a
/// space, then anywhere.
pub(super) fn pack(text: &str, budget: usize, max_parts: usize) -> Packed {
    let mut packer = Packer {
        parts: Vec::new(),
        max_parts,
        cut: false,
        current: String::new(),
        chars: 0,
    };
    for unit in units(text) {
        let size = unit.chars();
        let gap = match (packer.chars, unit.after_blank) {
            (0, _) => 0,
            (_, true) => 2,
            (_, false) => 1,
        };
        if packer.chars + gap + size <= budget {
            packer.append(gap, &unit.text());
        } else if size <= budget {
            packer.flush();
            packer.append(0, &unit.text());
        } else {
            packer.flush();
            let mut pieces = unit.split(budget).into_iter().peekable();
            while let Some(piece) = pieces.next() {
                packer.append(0, &piece);
                if pieces.peek().is_some() {
                    packer.flush();
                }
                if packer.cut {
                    break;
                }
            }
        }
        if packer.cut {
            break;
        }
    }
    packer.flush();
    Packed {
        parts: packer.parts,
        cut: packer.cut,
    }
}

struct Packer {
    parts: Vec<String>,
    max_parts: usize,
    cut: bool,
    current: String,
    chars: usize,
}

impl Packer {
    fn append(&mut self, gap: usize, text: &str) {
        self.current.push_str(&"\n".repeat(gap));
        self.current.push_str(text);
        self.chars += gap + text.chars().count();
    }

    fn flush(&mut self) {
        let part = std::mem::take(&mut self.current);
        self.chars = 0;
        let part = part.trim();
        if part.is_empty() {
            return;
        }
        if self.parts.len() == self.max_parts {
            self.cut = true;
        } else {
            self.parts.push(part.to_owned());
        }
    }
}

/// A paragraph, or one whole code block.
struct Unit<'a> {
    lines: Vec<&'a str>,
    /// The fence that opens it, for a code block.
    fence: Option<String>,
    /// A blank line came before it.
    after_blank: bool,
}

fn units(text: &str) -> Vec<Unit<'_>> {
    fn end_paragraph<'a>(
        paragraph: &mut Vec<&'a str>,
        after_blank: &mut bool,
        units: &mut Vec<Unit<'a>>,
    ) {
        if !paragraph.is_empty() {
            units.push(Unit {
                lines: std::mem::take(paragraph),
                fence: None,
                after_blank: *after_blank,
            });
            *after_blank = false;
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut units = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut after_blank = false;
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        if let Some(run) = fence_run(line) {
            end_paragraph(&mut paragraph, &mut after_blank, &mut units);
            let start = at;
            at += 1;
            while at < lines.len() && !closes(lines[at], &run) {
                at += 1;
            }
            // The closing fence, when there is one.
            let stop = (at + 1).min(lines.len());
            units.push(Unit {
                lines: lines[start..stop].to_vec(),
                fence: Some(run),
                after_blank,
            });
            after_blank = false;
            at = stop;
        } else if line.trim().is_empty() {
            end_paragraph(&mut paragraph, &mut after_blank, &mut units);
            after_blank = true;
            at += 1;
        } else {
            paragraph.push(line);
            at += 1;
        }
    }
    end_paragraph(&mut paragraph, &mut after_blank, &mut units);
    units
}

impl Unit<'_> {
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn chars(&self) -> usize {
        self.lines.iter().map(|l| l.chars().count()).sum::<usize>() + self.lines.len() - 1
    }

    /// Pieces of at most `budget` characters.
    fn split(&self, budget: usize) -> Vec<String> {
        match &self.fence {
            Some(run) => split_code(&self.lines, run, budget),
            None => split_prose(&self.lines, budget),
        }
    }
}

/// A code block cut into blocks of its own: each piece repeats the opening
/// fence, with its info string, and ends with a closing one.
fn split_code(lines: &[&str], run: &str, budget: usize) -> Vec<String> {
    let open = lines[0];
    if open.chars().count() + run.chars().count() >= budget / 2 {
        // Fences that long leave no room for code; they are text.
        return split_prose(lines, budget);
    }
    let body = match lines.last() {
        Some(last) if lines.len() > 1 && closes(last, run) => &lines[1..lines.len() - 1],
        _ => &lines[1..],
    };
    let room = budget - open.chars().count() - run.chars().count() - 2;
    let wrap = |inside: &str| format!("{open}\n{inside}\n{run}");
    let mut pieces = Vec::new();
    let mut inside = String::new();
    let mut chars = 0;
    for line in body {
        for chunk in chunks(line, room) {
            let size = chunk.chars().count();
            if chars > 0 && chars + 1 + size > room {
                pieces.push(wrap(&std::mem::take(&mut inside)));
                chars = 0;
            }
            if chars > 0 {
                inside.push('\n');
                chars += 1;
            }
            inside.push_str(&chunk);
            chars += size;
        }
    }
    if chars > 0 || pieces.is_empty() {
        pieces.push(wrap(&inside));
    }
    pieces
}

/// Lines packed into pieces; a line longer than a piece is cut at a space
/// when one is near the end, and anywhere otherwise.
fn split_prose(lines: &[&str], budget: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut chars = 0;
    for line in lines {
        for chunk in words(line, budget) {
            let size = chunk.chars().count();
            if chars > 0 && chars + 1 + size > budget {
                pieces.push(std::mem::take(&mut piece));
                chars = 0;
            }
            if chars > 0 {
                piece.push('\n');
                chars += 1;
            }
            piece.push_str(&chunk);
            chars += size;
        }
    }
    if chars > 0 {
        pieces.push(piece);
    }
    pieces
}

/// `line` in chunks of at most `size` characters, cut anywhere.
fn chunks(line: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(size.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// `line` in chunks of at most `size` characters, each ending on a space
/// when one falls in its last fifth.
fn words(line: &str, size: usize) -> Vec<String> {
    let size = size.max(1);
    let chars: Vec<char> = line.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    while chars.len() - start > size {
        let window = &chars[start..start + size];
        let at = match window.iter().rposition(|c| c.is_whitespace()) {
            Some(space) if space >= size * 4 / 5 => space,
            _ => size,
        };
        let chunk: String = window[..at].iter().collect();
        if !chunk.trim().is_empty() {
            chunks.push(chunk.trim_end().to_owned());
        }
        start += at;
        while chars.get(start).is_some_and(|c| c.is_whitespace()) {
            start += 1;
        }
    }
    chunks.push(chars[start..].iter().collect());
    chunks
}

#[cfg(test)]
mod tests {
    use super::{pack, render};
    use crate::agent::markdown::close_fences;

    fn lens(parts: &[String]) -> Vec<usize> {
        parts.iter().map(|p| p.chars().count()).collect()
    }

    /// No fence is left open: closing fences adds nothing.
    fn balanced(part: &str) -> bool {
        close_fences(part) == part
    }

    #[test]
    fn a_short_answer_is_one_part_as_written() {
        let packed = pack("One.\n\nTwo\nlines.", 100, 3);
        assert_eq!(packed.parts, ["One.\n\nTwo\nlines."]);
        assert!(!packed.cut);
        assert!(pack("", 100, 3).parts.is_empty());
        assert!(pack("  \n\n ", 100, 3).parts.is_empty());
    }

    #[test]
    fn parts_break_between_paragraphs_and_each_fits() {
        let paragraph = |n: usize| format!("{n}{}", "x".repeat(38));
        let text: Vec<String> = (0..6).map(paragraph).collect();
        let packed = pack(&text.join("\n\n"), 100, 3);
        assert!(!packed.cut);
        assert_eq!(packed.parts.len(), 3);
        assert_eq!(
            packed.parts[0],
            format!("{}\n\n{}", paragraph(0), paragraph(1))
        );
        assert_eq!(
            packed.parts[2],
            format!("{}\n\n{}", paragraph(4), paragraph(5))
        );
        assert!(lens(&packed.parts).iter().all(|n| *n <= 100));
    }

    #[test]
    fn what_does_not_fit_in_the_last_part_is_left_over() {
        let text = vec!["y".repeat(60); 10].join("\n\n");
        let packed = pack(&text, 100, 3);
        assert!(packed.cut);
        assert_eq!(packed.parts.len(), 3);
        assert!(packed.parts.iter().all(|p| p == &"y".repeat(60)));
    }

    #[test]
    fn a_code_block_stays_whole_in_one_part_when_it_can() {
        let code = "```rust\nfn a() {}\n\nfn b() {}\n```";
        let (before, after) = ("p".repeat(60), "q".repeat(60));
        let packed = pack(&format!("{before}\n\n{code}\n\n{after}"), 80, 5);
        assert_eq!(packed.parts, [before.as_str(), code, after.as_str()]);
    }

    #[test]
    fn a_code_block_longer_than_a_part_is_cut_into_blocks_that_each_close() {
        let body: Vec<String> = (0..40)
            .map(|n| format!("line {n:02} of the code"))
            .collect();
        let text = format!("Before.\n\n```py\n{}\n```\n\nAfter.", body.join("\n"));
        let packed = pack(&text, 150, 10);
        assert!(!packed.cut);
        assert!(packed.parts.len() > 4, "{:?}", lens(&packed.parts));
        for part in &packed.parts {
            assert!(part.chars().count() <= 150, "{part}");
            assert!(balanced(part), "{part}");
        }
        assert_eq!(packed.parts[0], "Before.");
        let last = packed.parts.last().unwrap();
        assert!(last.ends_with("```\n\nAfter."), "{last}");
        let code: Vec<&String> = packed.parts[1..].iter().collect();
        assert!(code.iter().all(|p| p.starts_with("```py\n")));
        let kept: Vec<&str> = code
            .iter()
            .flat_map(|p| p.lines().filter(|l| l.starts_with("line ")))
            .collect();
        assert_eq!(kept, body.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn an_open_fence_and_one_enormous_line_are_still_bounded() {
        let packed = pack(&format!("```\n{}", "z".repeat(1_000)), 100, 20);
        assert!(!packed.cut);
        assert!(
            packed
                .parts
                .iter()
                .all(|p| p.chars().count() <= 100 && balanced(p))
        );
        let zs: usize = packed
            .parts
            .iter()
            .map(|p| p.chars().filter(|c| *c == 'z').count())
            .sum();
        assert_eq!(zs, 1_000);
        let words = "word ".repeat(100);
        let packed = pack(words.trim(), 52, 20);
        assert!(
            packed
                .parts
                .iter()
                .all(|p| p.chars().count() <= 52 && p.ends_with("word"))
        );
        let wide = "é".repeat(500);
        assert!(
            pack(&wide, 64, 20)
                .parts
                .iter()
                .all(|p| p.chars().count() <= 64)
        );
    }

    #[test]
    fn a_table_answer_reads_as_text_and_the_code_inside_a_fence_is_left_alone() {
        let answer = "## Merged today\n\n| PR | What | When |\n|---|---|---|\n| #65 | fix(agent): the agent's answers read well in Discord and are not cut off | 17:54 |\n\n```\n| not | a table |\n```";
        let shown = render(answer);
        assert!(
            shown.starts_with("**Merged today**\n\n_PR · What · When_\n**#65** fix(agent)"),
            "{shown}"
        );
        assert!(
            shown.ends_with("· 17:54\n\n```\n| not | a table |\n```"),
            "{shown}"
        );
    }

    #[test]
    fn hostile_text_is_bounded_and_ordinary_fences_stay_balanced() {
        for text in [
            "```\n".repeat(5_000),
            "~~~~\n``` \n~~~\n".repeat(3_000),
            "\u{202e}".repeat(50_000),
            "@everyone <@1> ".repeat(10_000),
            format!("{}{}", "`".repeat(300), "a\n".repeat(20_000)),
            "|".repeat(50_000),
        ] {
            let packed = pack(&render(&text), 200, 3);
            assert!(packed.parts.len() <= 3);
            for part in &packed.parts {
                assert!(part.chars().count() <= 200, "{}", part.chars().count());
            }
        }
        for text in ["```\n".repeat(5_000), "~~~~\n``` \n~~~\n".repeat(3_000)] {
            let packed = pack(&render(&text), 200, 3);
            assert!(packed.parts.iter().all(|part| balanced(part)));
        }
    }
}

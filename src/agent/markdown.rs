//! The Markdown a worker writes, made readable where a channel does not
//! render all of it. Discord and Slack show no table and no heading syntax:
//! a table becomes a monospace block when it is narrow enough to stay one
//! and one line per row when it is not, a heading a bold line. Code blocks,
//! lists and the rest pass through the channel's own [`Dialect`].
//!
//! Pure: text in, text out. Fences are tracked line by line, the way GitHub
//! reads them, so nothing inside a code block is ever rewritten, and every
//! block the text leaves open is closed.

/// The widest monospace table, in characters, that is still a table. Discord
/// does not wrap a code block, and a phone shows about 40 characters of one
/// before it scrolls; wider tables read better as one line per row.
const MONO_MAX_WIDTH: usize = 50;
/// Two spaces between columns.
const GUTTER: usize = 2;
/// The text a channel converts is cut here first, so a hostile report cannot
/// make the conversion cost more than it is worth.
pub(crate) const MAX_INPUT_CHARS: usize = 100_000;
/// The longest language a code block keeps after its fence.
const MAX_INFO_CHARS: usize = 24;

/// How a channel writes what Markdown writes differently.
pub(crate) struct Dialect {
    /// A bold run: `**x**` on Discord, `*x*` on Slack.
    pub bold: fn(&str) -> String,
    /// An italic run, which both write `_x_`.
    pub italic: fn(&str) -> String,
    /// Inline Markdown in a heading or a table cell.
    pub inline: fn(&str) -> String,
    /// One line of prose, whole.
    pub line: fn(&str) -> String,
    /// One line inside a code block.
    pub code: fn(&str) -> String,
    /// Whether the channel shows a fence's info string (` ```rust `). Slack
    /// would show it as the first line of the code.
    pub fence_info: bool,
}

/// Convert `text` for the channel `dialect` describes.
pub(crate) fn convert(text: &str, dialect: &Dialect) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        if let Some(run) = fence_run(line) {
            // The language, the info string's first word.
            let info: String = line.trim_start_matches(' ')[run.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .chars()
                .take(MAX_INFO_CHARS)
                .collect();
            out.push(if dialect.fence_info && !info.is_empty() {
                format!("{run}{info}")
            } else {
                run.clone()
            });
            at += 1;
            while at < lines.len() && !closes(lines[at], &run) {
                out.push((dialect.code)(lines[at]));
                at += 1;
            }
            // The closing fence, or the one the text left out.
            out.push(run);
            at += 1;
        } else if let Some(table) = Table::at(&lines, at) {
            at += table.lines;
            table.render(dialect, &mut out);
        } else if let Some(heading) = heading(line) {
            let text = (dialect.inline)(&heading.replace("**", ""));
            out.push((dialect.bold)(&text));
            at += 1;
        } else {
            out.push((dialect.line)(line));
            at += 1;
        }
    }
    out.join("\n")
}

/// `text` with the code block it ends inside closed, so a cut there leaves
/// nothing open.
pub(crate) fn close_fences(text: &str) -> String {
    let mut open: Option<String> = None;
    for line in text.lines() {
        match (&open, fence_run(line)) {
            (None, Some(run)) => open = Some(run),
            (Some(fence), Some(_)) if closes(line, fence) => open = None,
            _ => {}
        }
    }
    match open {
        Some(fence) => format!("{text}\n{fence}"),
        None => text.to_owned(),
    }
}

/// The run of three or more backticks or tildes that opens a fence on this
/// line. Anything GitHub would not take for a fence is text.
pub(crate) fn fence_run(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let first = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run: String = trimmed.chars().take_while(|c| *c == first).collect();
    let info = &trimmed[run.len()..];
    (run.len() >= 3 && !(first == '`' && info.contains('`'))).then_some(run)
}

/// Whether `line` ends the fence `open` began.
pub(crate) fn closes(line: &str, open: &str) -> bool {
    fence_run(line).is_some_and(|run| {
        run.starts_with(open) && line.trim_start_matches(' ')[run.len()..].trim().is_empty()
    })
}

/// The text of an ATX heading (`## Title`), without its marks.
fn heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marks = trimmed.bytes().take_while(|b| *b == b'#').count();
    let rest = &trimmed[marks..];
    if !(1..=6).contains(&marks) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let text = rest.trim();
    // A closing run of `#` counts only after a space.
    let closed = text.trim_end_matches('#');
    let text = if closed.is_empty() || closed.ends_with([' ', '\t']) {
        closed.trim_end()
    } else {
        text
    };
    (!text.is_empty()).then(|| text.to_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
    Center,
}

/// A GitHub-flavored table: a header, a delimiter row of the same width and
/// the rows after it.
struct Table {
    header: Vec<String>,
    align: Vec<Align>,
    rows: Vec<Vec<String>>,
    /// How many lines of the text it took.
    lines: usize,
}

impl Table {
    fn at(lines: &[&str], at: usize) -> Option<Self> {
        let head = lines.get(at).filter(|line| line.contains('|'))?;
        let header = cells(head);
        let align = delimiter(lines.get(at + 1)?)?;
        if header.len() != align.len() {
            return None;
        }
        let mut rows = Vec::new();
        let mut end = at + 2;
        while let Some(row) = lines.get(end).filter(|row| row.contains('|')) {
            let mut row = cells(row);
            row.resize(header.len(), String::new());
            rows.push(row);
            end += 1;
        }
        Some(Self {
            header,
            align,
            rows,
            lines: end - at,
        })
    }

    fn render(&self, dialect: &Dialect, out: &mut Vec<String>) {
        if self.is_narrow() {
            self.mono(dialect, out);
        } else {
            self.list(dialect, out);
        }
    }

    fn all(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.header).chain(&self.rows).flatten()
    }

    /// A table that can be a monospace block: narrow, and with no link a
    /// code block would show as raw syntax.
    fn is_narrow(&self) -> bool {
        let linked = self.all().any(|cell| {
            cell.contains("](") || cell.contains("http://") || cell.contains("https://")
        });
        let width: usize = self.widths().iter().sum::<usize>() + GUTTER * (self.header.len() - 1);
        !linked && width <= MONO_MAX_WIDTH
    }

    fn widths(&self) -> Vec<usize> {
        (0..self.header.len())
            .map(|col| {
                std::iter::once(&self.header)
                    .chain(&self.rows)
                    .map(|row| plain(&row[col]).chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }

    fn mono(&self, dialect: &Dialect, out: &mut Vec<String>) {
        let widths = self.widths();
        let row = |cells: &[String]| {
            let mut line = String::new();
            for (col, cell) in cells.iter().enumerate() {
                let cell = plain(cell);
                let pad = widths[col] - cell.chars().count();
                let (left, right) = match self.align[col] {
                    Align::Left => (0, pad),
                    Align::Right => (pad, 0),
                    Align::Center => (pad / 2, pad - pad / 2),
                };
                if col > 0 {
                    line.push_str(&" ".repeat(GUTTER));
                }
                line.push_str(&" ".repeat(left));
                line.push_str(&cell);
                line.push_str(&" ".repeat(right));
            }
            (dialect.code)(line.trim_end())
        };
        out.push("```".to_owned());
        out.push(row(&self.header));
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        out.push(row(&rule));
        out.extend(self.rows.iter().map(|cells| row(cells)));
        out.push("```".to_owned());
    }

    /// One line per row: the first cell bold, the others after it. A table
    /// of three columns or more says what they are first.
    fn list(&self, dialect: &Dialect, out: &mut Vec<String>) {
        let names: Vec<&str> = self
            .header
            .iter()
            .map(String::as_str)
            .filter(|name| !name.is_empty())
            .collect();
        if self.header.len() >= 3 && !names.is_empty() {
            let names: Vec<String> = names.iter().map(|name| (dialect.inline)(name)).collect();
            out.push((dialect.italic)(&names.join(" · ")));
        }
        for row in &self.rows {
            let first = &row[0];
            let rest: Vec<String> = row[1..]
                .iter()
                .filter(|cell| !cell.is_empty())
                .map(|cell| (dialect.inline)(cell))
                .collect();
            let rest = rest.join(" · ");
            let first = (!first.is_empty())
                .then(|| (dialect.bold)(&(dialect.inline)(&first.replace("**", ""))));
            match (first, rest.is_empty()) {
                (Some(first), false) => out.push(format!("{first} {rest}")),
                (Some(only), true) => out.push(only),
                (None, false) => out.push(rest),
                (None, true) => {}
            }
        }
    }
}

/// A table row's cells. `\|` is a pipe in a cell, and a leading or trailing
/// pipe only frames the row.
fn cells(line: &str) -> Vec<String> {
    let line = line
        .replace("<br>", " ")
        .replace("<br/>", " ")
        .replace("<br />", " ");
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cell).trim().to_owned()),
            _ => cell.push(c),
        }
    }
    let last = cell.trim();
    if !last.is_empty() {
        out.push(last.to_owned());
    }
    out
}

/// The alignments of a delimiter row (`| --- | :-: |`), or `None` when the
/// line is not one.
fn delimiter(line: &str) -> Option<Vec<Align>> {
    if !line.contains('|') {
        return None;
    }
    let cells = cells(line);
    cells
        .iter()
        .map(|cell| {
            let body = cell.trim_matches(':');
            if body.is_empty() || !body.bytes().all(|b| b == b'-') {
                return None;
            }
            Some(match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect::<Option<Vec<_>>>()
        .filter(|align| !align.is_empty())
}

/// A cell as a code block shows it, where emphasis and code marks are not
/// syntax.
fn plain(cell: &str) -> String {
    cell.replace("**", "").replace("~~", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::{Dialect, close_fences, closes, convert, fence_run, heading};

    const DISCORD: Dialect = Dialect {
        bold: |s| format!("**{s}**"),
        italic: |s| format!("_{s}_"),
        inline: str::to_owned,
        line: str::to_owned,
        code: str::to_owned,
        fence_info: true,
    };

    #[test]
    fn a_narrow_table_is_a_monospace_block() {
        let text = "Status:\n\n| Task | State | Cost |\n|---|:-:|--:|\n| build | `ok` | $1.20 |\n| lint | **failed** | $0.05 |\n\nDone.";
        assert_eq!(
            convert(text, &DISCORD),
            "Status:\n\n```\nTask   State    Cost\n-----  ------  -----\nbuild    ok    $1.20\nlint   failed  $0.05\n```\n\nDone."
        );
    }

    #[test]
    fn a_wide_table_or_one_with_links_is_a_line_per_row_with_its_columns_named() {
        let wide = "| PR | Title | Merged |\n|---|---|---|\n| #65 | fix(agent): answers read well in Discord, and a long answer is not cut | 17:54 |\n| #64 | feat(web): the dashboard | 17:40 |";
        assert_eq!(
            convert(wide, &DISCORD),
            "_PR · Title · Merged_\n\
             **#65** fix(agent): answers read well in Discord, and a long answer is not cut · 17:54\n\
             **#64** feat(web): the dashboard · 17:40"
        );
        let linked = "| Name | Link |\n|---|---|\n| docs | [site](https://example.com/docs) |";
        assert_eq!(
            convert(linked, &DISCORD),
            "**docs** [site](https://example.com/docs)"
        );
    }

    #[test]
    fn a_table_that_is_not_one_is_left_alone() {
        for text in [
            "a | b\njust text",
            "| a | b |\n| --- |\n| 1 | 2 |",
            "| a |\n| x |",
            "no pipes\n---",
        ] {
            let converted = convert(text, &DISCORD);
            assert!(!converted.contains("```"), "{text:?} became {converted:?}");
        }
        assert_eq!(convert("a | b\njust text", &DISCORD), "a | b\njust text");
    }

    #[test]
    fn a_ragged_table_pads_and_trims_its_rows_and_unescapes_pipes() {
        let text = "| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |\n| x\\|y | z |";
        assert_eq!(
            convert(text, &DISCORD),
            "```\na    b\n---  -\n1\n1    2\nx|y  z\n```"
        );
    }

    #[test]
    fn headings_are_bold_lines_and_only_real_headings() {
        assert_eq!(
            convert(
                "# One\n### Two ###\n###### **Six**\n#tag\n####### seven\n    # indented\n#",
                &DISCORD
            ),
            "**One**\n**Two**\n**Six**\n#tag\n####### seven\n    # indented\n#"
        );
        assert_eq!(heading("## a # b"), Some("a # b".to_owned()));
        assert_eq!(heading("## c#"), Some("c#".to_owned()));
    }

    #[test]
    fn code_blocks_pass_through_untouched_and_are_closed() {
        let text = "# Title\n```rust\n# not a heading\n| a | b |\n|---|---|\n```\n~~~\nopen";
        assert_eq!(
            convert(text, &DISCORD),
            "**Title**\n```rust\n# not a heading\n| a | b |\n|---|---|\n```\n~~~\nopen\n~~~"
        );
        assert_eq!(close_fences("a\n```\nb"), "a\n```\nb\n```");
        assert_eq!(close_fences("a\n```\nb\n```\nc"), "a\n```\nb\n```\nc");
        assert!(fence_run("   ```x").is_some() && fence_run("    ```").is_none());
        assert!(closes("````", "```") && !closes("``` x", "```") && !closes("~~~", "```"));
    }

    #[test]
    fn hostile_input_stays_text_and_cannot_blow_up() {
        for text in [
            "|".repeat(10_000),
            format!("{}\n{}", "|a".repeat(5_000), "|-".repeat(5_000)),
            "```\n".repeat(10_000),
            "#".repeat(10_000),
            "| a | b |\n|---|---|\n".to_owned() + &"| x | y |\n".repeat(10_000),
            "\0\u{202e}\r\n\r\n|\r\n|-|\r\n".to_owned(),
        ] {
            let converted = convert(&text, &DISCORD);
            assert!(converted.len() < text.len() * 3 + 64);
        }
        assert_eq!(convert("", &DISCORD), "");
    }
}

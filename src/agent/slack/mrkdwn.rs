//! Slack's mrkdwn for what a worker wrote in Markdown: `*bold*`, `_italic_`,
//! `~strike~`, `<url|text>` links, `•` bullets, headings as bold lines and
//! tables as text (see [`crate::agent::markdown`]).
//!
//! Everything else is escaped (`&`, `<` and `>`), so a worker's `<!channel>`
//! or `<@U1>` is text, never a ping, and a link is made only from Markdown's
//! `[text](url)` with an http, https or mailto address.
//!
//! Pure: text in, text out. Emphasis is matched on one line, within a bounded
//! window and a bounded number of attempts, so no input costs more than a
//! small multiple of its length.

use crate::agent::markdown::{self, Dialect};

const SLACK: Dialect = Dialect {
    bold: |text| format!("*{text}*"),
    italic: |text| format!("_{text}_"),
    inline,
    line,
    code: escape,
    fence_info: false,
};

/// The longest span of emphasis or link text looked for after its opening.
const MAX_SPAN: usize = 2_000;
/// Searches that may fail on one line before the rest of it is left literal.
const MAX_FAILED_SEARCHES: u32 = 200;
/// How deep emphasis nests.
const MAX_DEPTH: u8 = 3;

pub(super) fn render(text: &str) -> String {
    let text: String = text.chars().take(markdown::MAX_INPUT_CHARS).collect();
    markdown::convert(&text, &SLACK)
}

pub(super) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A line of prose: its quote mark and bullet, then its inline Markdown.
fn line(line: &str) -> String {
    let body = line.trim_start();
    let lead = &line[..line.len() - body.len()];
    if let Some(quoted) = body.strip_prefix('>') {
        return format!("{lead}> {}", inline(quoted.trim_start()));
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(item) = body.strip_prefix(bullet) {
            return format!("{lead}• {}", inline(item));
        }
    }
    format!("{lead}{}", inline(body))
}

fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut failed = 0;
    emit(&chars, true, 0, &mut failed, &mut out);
    out
}

fn emit(chars: &[char], links: bool, depth: u8, failed: &mut u32, out: &mut String) {
    let mut at = 0;
    while at < chars.len() {
        let rest = &chars[at..];
        let before = at.checked_sub(1).map(|i| chars[i]);
        let step = match rest[0] {
            '`' => code(rest, failed, out),
            '!' if links && rest.get(1) == Some(&'[') => {
                link(&rest[1..], failed, out).map(|used| used + 1)
            }
            '[' if links => link(rest, failed, out),
            '*' | '_' | '~' if depth < MAX_DEPTH => {
                emphasis(rest, before, links, depth, failed, out)
            }
            _ => None,
        };
        match step {
            Some(used) => at += used,
            None => {
                match rest[0] {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    c => out.push(c),
                }
                at += 1;
            }
        }
    }
}

/// A code span at the start of `rest`: its text escaped, nothing inside it
/// converted. How many characters it took.
fn code(rest: &[char], failed: &mut u32, out: &mut String) -> Option<usize> {
    let run = rest.iter().take_while(|c| **c == '`').count();
    // The span ends at a run of as many backticks as began it, no more.
    let end = find(rest, run, failed, |at| {
        let ticks = rest[at..].iter().take_while(|c| **c == '`').count();
        ticks == run && rest[at - 1] != '`'
    });
    let Some(close) = end else {
        out.extend(&rest[..run]);
        return Some(run);
    };
    let inside: String = rest[run..close].iter().collect();
    // Slack's code span is a single pair of backticks.
    let ticks = if inside.contains('`') { run } else { 1 };
    out.push_str(&"`".repeat(ticks));
    out.push_str(&escape(&inside));
    out.push_str(&"`".repeat(ticks));
    Some(close + run)
}

/// `[text](url)` at the start of `rest`: a Slack link when the address is
/// http, https or mailto, and the text followed by the address in brackets
/// otherwise. How many characters it took, or `None` when it is no link.
fn link(rest: &[char], failed: &mut u32, out: &mut String) -> Option<usize> {
    let close = find(rest, 1, failed, closer(rest, '[', ']'))?;
    if rest.get(close + 1) != Some(&'(') {
        return None;
    }
    let target = &rest[close + 2..];
    let end = find(target, 0, failed, closer(target, '(', ')'))?;
    let inside: String = target[..end].iter().collect();
    let address = inside.split_whitespace().next().unwrap_or_default();
    let address = address
        .strip_prefix('<')
        .and_then(|a| a.strip_suffix('>'))
        .unwrap_or(address);
    let mut label = String::new();
    emit(&rest[1..close], false, 1, failed, &mut label);
    let lower = address.to_ascii_lowercase();
    let web = ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme));
    if address.is_empty() || address.chars().any(char::is_control) {
        out.push_str(&label);
    } else if web {
        let address = address
            .replace('&', "&amp;")
            .replace('<', "%3C")
            .replace('>', "%3E")
            .replace('|', "%7C");
        if label.trim().is_empty() {
            out.push_str(&format!("<{address}>"));
        } else {
            out.push_str(&format!("<{address}|{label}>"));
        }
    } else {
        out.push_str(&format!("{label} ({})", escape(address)));
    }
    Some(close + 2 + end + 1)
}

/// Finds the `close` that ends what `open` began in `text`, where the pairs
/// in between nest. Called with each position in order.
fn closer(text: &[char], open: char, close: char) -> impl FnMut(usize) -> bool + '_ {
    let mut depth = 1;
    move |at| {
        if text[at] == open {
            depth += 1;
        } else if text[at] == close {
            depth -= 1;
        }
        depth == 0
    }
}

/// `**bold**`, `__bold__`, `*italic*` or `~~strike~~` at the start of `rest`;
/// `_italic_` is Slack's too and stays as it is. How many characters it took,
/// or `None` when it is no emphasis and the first one is text.
fn emphasis(
    rest: &[char],
    before: Option<char>,
    links: bool,
    depth: u8,
    failed: &mut u32,
    out: &mut String,
) -> Option<usize> {
    let mark = rest[0];
    let doubled = rest.get(1) == Some(&mark);
    let width = if doubled { 2 } else { 1 };
    let pair = [mark, mark];
    let marks = &pair[..width];
    // What Slack writes where Markdown wrote `marks`.
    let slack = match (mark, doubled) {
        ('*', true) | ('_', true) => '*',
        ('*', false) => '_',
        ('~', true) => '~',
        _ => return None,
    };
    // A mark hugs the text it opens, and `*` or `_` inside a word is text.
    let hugs = rest
        .get(width)
        .is_some_and(|c| !c.is_whitespace() && *c != mark);
    if !hugs || (mark != '~' && before.is_some_and(char::is_alphanumeric)) {
        return None;
    }
    let close = find(rest, width, failed, |at| {
        let after = rest.get(at + width);
        rest[at..].starts_with(marks)
            && !rest[at - 1].is_whitespace()
            && after != Some(&mark)
            && !(mark == '_' && after.is_some_and(|c| c.is_alphanumeric()))
    })?;
    let mut inner = String::new();
    emit(&rest[width..close], links, depth + 1, failed, &mut inner);
    out.push(slack);
    out.push_str(&inner);
    out.push(slack);
    Some(close + width)
}

/// The first position at or after `from` in `rest` where `matches` says yes,
/// within [`MAX_SPAN`] characters, counting a miss against the line's budget.
fn find(
    rest: &[char],
    from: usize,
    failed: &mut u32,
    mut matches: impl FnMut(usize) -> bool,
) -> Option<usize> {
    if *failed >= MAX_FAILED_SEARCHES {
        return None;
    }
    let found = (from..rest.len().min(from + MAX_SPAN)).find(|at| matches(*at));
    if found.is_none() {
        *failed += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn markdown_becomes_mrkdwn() {
        assert_eq!(
            render("**bold** and *italic* and _under_ and ~~gone~~ and `code <b>`"),
            "*bold* and _italic_ and _under_ and ~gone~ and `code &lt;b&gt;`"
        );
        assert_eq!(
            render(
                "See [the docs](https://example.com/a?x=1&y=2) or ![logo](https://example.com/l.png)."
            ),
            "See <https://example.com/a?x=1&amp;y=2|the docs> or <https://example.com/l.png|logo>."
        );
        assert_eq!(
            render("- one\n  - two\n* three\n+ four\n1. five\n> quoted *text*"),
            "• one\n  • two\n• three\n• four\n1. five\n> quoted _text_"
        );
    }

    #[test]
    fn headings_and_tables_become_bold_lines_and_text() {
        assert_eq!(
            render("# Title\n\n| a | b |\n|---|--:|\n| x | 1 |"),
            "*Title*\n\n```\na  b\n-  -\nx  1\n```"
        );
        assert_eq!(
            render(
                "| Repo | Open | Notes |\n|---|---|---|\n| [core](https://example.com/core) | 3 | see **docs** |"
            ),
            "_Repo · Open · Notes_\n*<https://example.com/core|core>* 3 · see *docs*"
        );
    }

    #[test]
    fn code_blocks_are_escaped_and_lose_their_language() {
        assert_eq!(
            render("Run:\n```rust\nlet a = b < c && d;\n# not a heading\n```\nafter **x**"),
            "Run:\n```\nlet a = b &lt; c &amp;&amp; d;\n# not a heading\n```\nafter *x*"
        );
        assert_eq!(render("```\nunclosed *x*"), "```\nunclosed *x*\n```");
    }

    #[test]
    fn mentions_and_hostile_links_are_only_text() {
        assert_eq!(
            render("<!channel> <@U1> <!here|here> & <https://evil.example|x>"),
            "&lt;!channel&gt; &lt;@U1&gt; &lt;!here|here&gt; &amp; &lt;https://evil.example|x&gt;"
        );
        assert_eq!(
            render(
                "[x](javascript:alert(1)) [y](data:text/html,<b>) [z](docs/a.md) [](https://e.example)"
            ),
            "x (javascript:alert(1)) y (data:text/html,&lt;b&gt;) z (docs/a.md) <https://e.example>"
        );
        assert_eq!(
            render("[a|b>](https://e.example/p|q>r) [`<!channel>`](https://e.example)"),
            "<https://e.example/p%7Cq%3Er|a|b&gt;> <https://e.example|`&lt;!channel&gt;`>"
        );
        assert_eq!(
            render("[*x*](https://e.example)"),
            "<https://e.example|_x_>"
        );
    }

    #[test]
    fn marks_that_close_nothing_or_sit_inside_a_word_are_text() {
        assert_eq!(
            render("2 * 3 * 4, snake_case_name, a*b*c, **open, *open, ~~open, `open, [open, [a](b"),
            "2 * 3 * 4, snake_case_name, a*b*c, **open, *open, ~~open, `open, [open, [a](b"
        );
        assert_eq!(render("__init__ and foo__bar__"), "*init* and foo__bar__");
    }

    #[test]
    fn nesting_is_bounded_and_hostile_input_costs_little() {
        let nested = "*a **b ~~c *d *e*** ~~**";
        assert!(!render(nested).is_empty());
        for text in [
            "*a ".repeat(30_000),
            "**a ".repeat(20_000),
            "[a](".repeat(20_000),
            "`a ".repeat(30_000),
            "[".repeat(50_000),
            "![[".repeat(20_000),
            "_*~".repeat(30_000),
            format!("{}{}", "*".repeat(40_000), "x"),
        ] {
            let start = std::time::Instant::now();
            let rendered = render(&text);
            assert!(rendered.len() >= text.len() / 2);
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "{:?}",
                start.elapsed()
            );
        }
    }
}

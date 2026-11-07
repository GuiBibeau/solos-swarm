//! The evidence a repository's pull requests carry (`forge.repos.<repo>.evidence`).
//!
//! Some repositories accept a pull request's checks only with the JSON a
//! command printed, pasted in the description under a heading. The worker's
//! ready report is that description, so the host reads the report before it
//! opens the pull request, and refuses one that CI would fail at once: no
//! heading, no `json` fence under it, JSON that is not an object, or an object
//! that says the checks failed (`ok` is not `true`) or the tree was not clean
//! (`dirty` is `true`).
//!
//! The host checks the shape and does not judge the proof: which commit the
//! object names (`sha`), and every other rule, stay the repository's own CI.
//! The block sits in a code fence, so the description's mention quieting
//! (`@name` outside code) leaves it as it was.

use crate::agent::config::EvidenceConfig;
use serde_json::Value;

/// `Ok` when `text` carries evidence in the shape `terms` asks for; else the
/// words the worker reads, which say what to do next.
pub fn check(text: &str, terms: &EvidenceConfig) -> Result<(), String> {
    let EvidenceConfig { command, heading } = terms;
    let ask = format!(
        "Commit everything, run `{command}` in /work, and report ready again with its JSON, \
         unedited, in a ```json fence under a `{heading}` heading. Your report's text is the pull \
         request's description. If you cannot run it, report blocked and say why."
    );
    let Some(body) = block_under(text, heading) else {
        return Err(format!(
            "Not accepted: this repository's pull requests carry the output of `{command}` under \
             a `{heading}` heading, and your report has no such heading followed by a ```json \
             fence. {ask}"
        ));
    };
    let value: Value = serde_json::from_str(&body).map_err(|why| {
        format!(
            "Not accepted: the JSON under `{heading}` does not parse ({why}). A report is cut at \
             8,000 characters, so a long one loses the end of its block: shorten the prose and \
             paste the output as it was. {ask}"
        )
    })?;
    let Some(object) = value.as_object() else {
        return Err(format!(
            "Not accepted: the JSON under `{heading}` is not the object `{command}` prints. {ask}"
        ));
    };
    if object.get("ok") != Some(&Value::Bool(true)) {
        return Err(format!(
            "Not accepted: the evidence under `{heading}` does not say `\"ok\": true`, so a check \
             failed or was not run. Fix what failed, commit it, and run `{command}` again. {ask}"
        ));
    }
    if object.get("dirty") == Some(&Value::Bool(true)) {
        return Err(format!(
            "Not accepted: the evidence under `{heading}` says the tree was dirty, so it does not \
             prove your commits. {ask}"
        ));
    }
    Ok(())
}

/// The text of the first fence labelled `json` after the line that is
/// `heading`, outside any other fence. `None` when the heading is not there,
/// something other than blank lines comes between it and a fence, or the fence
/// is never closed.
fn block_under(text: &str, heading: &str) -> Option<String> {
    let mut open: Option<Fence> = None;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let fence = Fence::of(line);
        if let Some(current) = &open {
            if fence.is_some_and(|close| close.closes(current)) {
                open = None;
            }
        } else if fence.is_some() {
            open = fence;
        } else if line.trim() == heading {
            let first = lines.by_ref().find(|line| !line.trim().is_empty())?;
            let fence = Fence::of(first).filter(|fence| fence.label == "json")?;
            let mut body = String::new();
            for line in lines {
                if Fence::of(line).is_some_and(|close| close.closes(&fence)) {
                    return Some(body);
                }
                body.push_str(line);
                body.push('\n');
            }
            return None;
        }
    }
    None
}

/// A line that opens or closes a code fence: three or more of one of the
/// characters `` ` `` and `~`, then, for an opening one, a label.
struct Fence {
    mark: char,
    run: usize,
    label: String,
}

impl Fence {
    fn of(line: &str) -> Option<Self> {
        let line = line.trim_start();
        let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let run = line.chars().take_while(|c| *c == mark).count();
        (run >= 3).then(|| Self {
            mark,
            run,
            label: line[run..].trim().to_ascii_lowercase(),
        })
    }

    /// Whether this line, with no label, ends `open`.
    fn closes(&self, open: &Fence) -> bool {
        self.mark == open.mark && self.run >= open.run && self.label.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::agent::config::EvidenceConfig;

    fn terms() -> EvidenceConfig {
        EvidenceConfig {
            command: "bun run solos dev verify --scope unit --json".into(),
            heading: "## Evidence".into(),
        }
    }

    const OK: &str = r#"{"ok":true,"sha":"abc1234","dirty":false,"scope":"unit","steps":[]}"#;

    fn report(block: &str) -> String {
        format!("Fixed the thing.\n\n## Evidence\n\n```json\n{block}\n```\n\nThanks.")
    }

    #[test]
    fn evidence_in_a_json_fence_under_the_heading_is_accepted_wherever_it_sits() {
        assert_eq!(check(&report(OK), &terms()), Ok(()));
        let first = format!("## Evidence\n```json\n{OK}\n```\n## Notes\nlater");
        assert_eq!(check(&first, &terms()), Ok(()));
        let longer = format!("Done.\n\n## Evidence\n\n````json\n{OK}\n````\n");
        assert_eq!(
            check(&longer, &terms()),
            Ok(()),
            "a longer fence is a fence"
        );
        let pretty = report("{\n  \"ok\": true,\n  \"dirty\": false\n}");
        assert_eq!(check(&pretty, &terms()), Ok(()));
    }

    #[test]
    fn a_report_without_the_block_is_told_the_command_and_what_to_do() {
        for text in [
            "Fixed the thing.".to_owned(),
            format!("## Evidence\n\n{OK}\n"),
            format!("## Evidence\n\nran it\n```json\n{OK}\n```"),
            format!("## Evidence\n```text\n{OK}\n```"),
            format!("## Evidence\n```json\n{OK}\n"),
            format!("### Evidence\n```json\n{OK}\n```"),
            format!("Not a heading: ## Evidence\n```json\n{OK}\n```"),
        ] {
            let why = check(&text, &terms()).unwrap_err();
            assert!(why.starts_with("Not accepted"), "{why}");
            assert!(
                why.contains("bun run solos dev verify --scope unit --json")
                    && why.contains("`## Evidence`")
                    && why.contains("report blocked"),
                "{why}"
            );
        }
    }

    #[test]
    fn a_heading_inside_another_code_block_is_an_example_not_the_evidence() {
        let text =
            format!("How it looks:\n```md\n## Evidence\n```json\n{OK}\n```\n```\nThat is all.");
        assert!(check(&text, &terms()).is_err());
    }

    #[test]
    fn json_that_does_not_parse_or_is_not_an_object_is_refused_with_the_cut_in_mind() {
        let cut = report(r#"{"ok":true,"steps":[{"name":"lint","ok":tr"#);
        let why = check(&cut, &terms()).unwrap_err();
        assert!(
            why.contains("does not parse") && why.contains("8,000"),
            "{why}"
        );
        let list = check(&report("[1,2]"), &terms()).unwrap_err();
        assert!(list.contains("is not the object"), "{list}");
    }

    #[test]
    fn evidence_that_says_the_checks_failed_or_the_tree_was_dirty_is_refused() {
        for (block, said) in [
            (
                r#"{"ok":false,"dirty":false}"#,
                "does not say `\"ok\": true`",
            ),
            (r#"{"dirty":false}"#, "does not say `\"ok\": true`"),
            (r#"{"ok":"true"}"#, "does not say `\"ok\": true`"),
            (r#"{"ok":true,"dirty":true}"#, "says the tree was dirty"),
        ] {
            let why = check(&report(block), &terms()).unwrap_err();
            assert!(why.contains(said), "{block}: {why}");
        }
        assert_eq!(
            check(&report(r#"{"ok":true}"#), &terms()),
            Ok(()),
            "the sha is CI's to judge"
        );
    }

    #[test]
    fn another_heading_is_matched_as_written() {
        let mut other = terms();
        other.heading = "### Proof".into();
        let text = format!("### Proof\n```json\n{OK}\n```");
        assert_eq!(check(&text, &other), Ok(()));
        assert!(check(&report(OK), &other).is_err());
    }
}

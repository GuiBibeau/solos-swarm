//! `write_file` and `edit_file`: a worker writes and changes files in its
//! Ticket's `/work` without a shell, so it never quotes a file into a
//! heredoc. The broker writes as the host, outside the sandbox, so every path
//! goes through [`Beneath`], which follows no link the worker made there.

use super::{Refusal, Reply, WORK_MOUNT, WorkerSeat, arguments, failed, forbidden, refuse};
use crate::beneath::{Beneath, Refused};
use crate::code::MAX_FILE_BYTES;
use axum::Json;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
}

/// `write_file`: create or replace one file.
pub(super) async fn write(seat: &WorkerSeat, args: Value) -> Reply {
    let WriteArgs { path, content } = arguments(args)?;
    let rel = beneath_work(&path)?.to_owned();
    let shown = format!("{WORK_MOUNT}/{rel}");
    fits(&shown, content.len())?;
    let bytes = content.len();
    let written = in_work(seat, move |work| {
        work.write(&rel, content.as_bytes()).map_err(refusal)
    })
    .await?;
    Ok(Json(json!({
        "path": shown,
        "bytes": bytes,
        "created": written.created,
    })))
}

/// `edit_file`: replace the one place `old` is with `new`.
pub(super) async fn edit(seat: &WorkerSeat, args: Value) -> Reply {
    let EditArgs { path, old, new } = arguments(args)?;
    let rel = beneath_work(&path)?.to_owned();
    let shown = format!("{WORK_MOUNT}/{rel}");
    if old.is_empty() {
        return Err(unprocessable(
            "old is empty: copy the text to replace from the file",
        ));
    }
    let named = shown.clone();
    let (bytes, line) = in_work(seat, move |work| {
        let text = work.read_text(&rel, MAX_FILE_BYTES).map_err(refusal)?;
        let (text, line) = replace_once(&text, &old, &new)
            .map_err(|why| unprocessable(format!("{named}: {why}")))?;
        fits(&named, text.len())?;
        work.write(&rel, text.as_bytes()).map_err(refusal)?;
        Ok((text.len(), line))
    })
    .await?;
    Ok(Json(json!({"path": shown, "bytes": bytes, "line": line})))
}

/// `path` beneath `/work`: relative to it, or absolute under it, as `read`
/// and `run_code` name it.
fn beneath_work(path: &str) -> Result<&str, Refusal> {
    let outside = || {
        unprocessable(format!(
            "{path} is not in {WORK_MOUNT}: name a file there, such as {WORK_MOUNT}/README.md \
             or README.md"
        ))
    };
    match path.strip_prefix(WORK_MOUNT) {
        Some("") => Err(unprocessable(format!(
            "{WORK_MOUNT} is a directory: name a file in it"
        ))),
        Some(rest) => rest.strip_prefix('/').ok_or_else(outside),
        None if path.starts_with('/') => Err(outside()),
        None => Ok(path),
    }
}

/// `text` with its one `old` replaced by `new`, and the line the change
/// starts on, or why there is not exactly one `old`.
fn replace_once(text: &str, old: &str, new: &str) -> Result<(String, usize), String> {
    let Some(at) = text.find(old) else {
        return Err(
            "old is not in it. Read the file again and copy old exactly, white space included \
             and without read's line numbers."
                .into(),
        );
    };
    let next = at + old.chars().next().map_or(1, char::len_utf8);
    if text[next..].contains(old) {
        let times = text.matches(old).count().max(2);
        return Err(format!(
            "old is in it {times} times. Include more of the lines around the one to change, so \
             that old is there once."
        ));
    }
    let line = text[..at].matches('\n').count() + 1;
    let changed = format!("{}{new}{}", &text[..at], &text[at + old.len()..]);
    Ok((changed, line))
}

fn fits(shown: &str, bytes: usize) -> Result<(), Refusal> {
    if bytes > MAX_FILE_BYTES {
        return Err(unprocessable(format!(
            "{shown} would be {bytes} bytes, and a file written here is at most \
             {MAX_FILE_BYTES}: use run_code for it"
        )));
    }
    Ok(())
}

/// Run `call` on the seat's `/work`, opened once, off the async workers.
async fn in_work<T: Send + 'static>(
    seat: &WorkerSeat,
    call: impl FnOnce(&Beneath) -> Result<T, Refusal> + Send + 'static,
) -> Result<T, Refusal> {
    let dir = seat.work_dir.clone();
    tokio::task::spawn_blocking(move || {
        let work = Beneath::open(&dir, WORK_MOUNT)
            .map_err(|err| failed(format!("cannot open {WORK_MOUNT}: {err}")))?;
        call(&work)
    })
    .await
    .map_err(failed)?
}

fn refusal(refused: Refused) -> Refusal {
    match refused {
        Refused::Link(why) => forbidden(why),
        Refused::Missing(why) => refuse(StatusCode::NOT_FOUND, why),
        Refused::Host(why) => failed(why),
        Refused::Invalid(why)
        | Refused::NotFile(why)
        | Refused::TooLarge(why)
        | Refused::NotText(why) => unprocessable(why),
    }
}

fn unprocessable(why: impl std::fmt::Display) -> Refusal {
    refuse(StatusCode::UNPROCESSABLE_ENTITY, why)
}

#[cfg(test)]
mod tests {
    use super::{beneath_work, replace_once};

    #[test]
    fn exactly_one_old_is_replaced_and_its_line_is_given() {
        let text = "fn a() {}\nfn b() {\n    1\n}\n";
        assert_eq!(
            replace_once(text, "    1\n", "    2\n"),
            Ok(("fn a() {}\nfn b() {\n    2\n}\n".to_owned(), 3))
        );
        let none = replace_once(text, "fn c", "x").unwrap_err();
        assert!(none.starts_with("old is not in it."), "{none}");
        let twice = replace_once(text, "fn ", "x").unwrap_err();
        assert!(twice.starts_with("old is in it 2 times."), "{twice}");
        let overlapping = replace_once("aaa", "aa", "b").unwrap_err();
        assert!(
            overlapping.starts_with("old is in it 2 times."),
            "{overlapping}"
        );
        assert_eq!(replace_once("é\né!", "é!", "e"), Ok(("é\ne".to_owned(), 2)));
    }

    #[test]
    fn a_path_is_relative_to_work_or_absolute_under_it() {
        assert_eq!(beneath_work("src/main.rs").ok(), Some("src/main.rs"));
        assert_eq!(beneath_work("/work/src/main.rs").ok(), Some("src/main.rs"));
        for outside in ["/work", "/workshop/a", "/etc/passwd", "/host/repo.json"] {
            assert!(beneath_work(outside).is_err(), "{outside}");
        }
    }
}

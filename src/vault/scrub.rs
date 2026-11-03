use super::file::Stored;
use base64::{
    Engine,
    engine::general_purpose::{GeneralPurpose, STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::Value;
use std::fmt::{self, Write as _};
use zeroize::Zeroizing;

/// Looks for each value in these forms:
/// - raw, and with a backslash before any of its ASCII punctuation, as
///   markdown escaping (Lightpanda's `LP.getMarkdown`) writes page text;
/// - percent-encoded as a form posts it, as `encodeURIComponent` writes it
///   and as a URL's query keeps it, in upper- and lowercase hex;
/// - JSON-escaped as serde and JavaScript write it, with non-ASCII as
///   `\uXXXX` (Python, PHP), with `/` as `\/` (PHP), and with `&<>` as
///   `&` and so on (Go);
/// - HTML-escaped as a text node (`&<>`), as an attribute (`&"`) and in
///   full, with `"` as `&quot;` or `&#34;` and `'` as `&#39;`, `&#x27;` or
///   itself;
/// - base64, standard and URL-safe, wherever the value falls among the
///   encoded bytes, as Basic auth's `username:password` puts it.
///
/// The longest form is replaced first, so a form that holds another goes
/// whole. A value that a page transforms any other way is not found.
pub struct Scrubber {
    needles: Vec<Needle>,
}

struct Needle {
    form: Zeroizing<String>,
    marker: String,
    markdown: bool,
}

impl Scrubber {
    pub(super) fn new(items: &[Stored]) -> Self {
        let mut needles = Vec::new();
        for item in items {
            let marker = format!("[vault:{}]", item.info.name);
            let value = item.value.reveal();
            needles.push(Needle {
                form: Zeroizing::new(value.to_owned()),
                marker: marker.clone(),
                markdown: true,
            });
            needles.extend(forms(value).into_iter().map(|form| Needle {
                form,
                marker: marker.clone(),
                markdown: false,
            }));
        }
        needles.sort_by(|left, right| {
            right
                .form
                .len()
                .cmp(&left.form.len())
                .then_with(|| right.markdown.cmp(&left.markdown))
                .then_with(|| left.form.as_str().cmp(right.form.as_str()))
        });
        needles.dedup_by(|later, kept| later.form == kept.form);
        Self { needles }
    }

    pub fn clean(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for needle in &self.needles {
            if needle.markdown {
                if let Some(cleaned) = replace_escaped(&text, &needle.form, &needle.marker) {
                    text = cleaned;
                }
            } else if text.contains(needle.form.as_str()) {
                text = text.replace(needle.form.as_str(), &needle.marker);
            }
        }
        text
    }

    pub fn clean_json(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.clean(text),
            Value::Number(number) => {
                let text = number.to_string();
                let cleaned = self.clean(&text);
                if cleaned != text {
                    *value = Value::String(cleaned);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.clean_json(item)),
            Value::Object(fields) => {
                *fields = std::mem::take(fields)
                    .into_iter()
                    .map(|(key, mut field)| {
                        self.clean_json(&mut field);
                        (self.clean(&key), field)
                    })
                    .collect();
            }
            Value::Null | Value::Bool(_) => {}
        }
    }
}

impl fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Scrubber({} needles)", self.needles.len())
    }
}

fn forms(value: &str) -> Vec<Zeroizing<String>> {
    let json = Zeroizing::new(Value::from(value).to_string());
    let json = Zeroizing::new(json[1..json.len() - 1].to_owned());
    let ascii = Zeroizing::new(ascii_json(&json));
    let mut forms = vec![
        escape(
            &json,
            &[('&', "\\u0026"), ('<', "\\u003c"), ('>', "\\u003e")],
        ),
        escape(&ascii, &[('/', "\\/")]),
        ascii.to_string(),
        json.to_string(),
        escape(value, &[('&', "&amp;"), ('<', "&lt;"), ('>', "&gt;")]),
        escape(value, &[('&', "&amp;"), ('"', "&quot;")]),
    ];
    for quote in ["&quot;", "&#34;"] {
        for apostrophe in ["&#39;", "&#x27;", "'"] {
            forms.push(escape(
                value,
                &[
                    ('&', "&amp;"),
                    ('<', "&lt;"),
                    ('>', "&gt;"),
                    ('"', quote),
                    ('\'', apostrophe),
                ],
            ));
        }
    }
    let form = |byte: u8| byte.is_ascii_alphanumeric() || b"*-._".contains(&byte);
    let component = |byte: u8| byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte);
    let query = |byte: u8| (0x21..0x7f).contains(&byte) && !b"\"#<>'".contains(&byte);
    for upper in [true, false] {
        forms.push(percent(value, form, true, upper));
        forms.push(percent(value, component, false, upper));
        forms.push(percent(value, query, false, upper));
    }
    forms.push(STANDARD.encode(value));
    forms.extend(base64_anywhere(value, &STANDARD));
    forms.extend(base64_anywhere(value, &URL_SAFE_NO_PAD));
    forms.into_iter().map(Zeroizing::new).collect()
}

/// `text` with each character that `table` names replaced by its escape.
fn escape(text: &str, table: &[(char, &str)]) -> String {
    let mut escaped = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        match table.iter().find(|(from, _)| *from == c) {
            Some((_, to)) => escaped.push_str(to),
            None => escaped.push(c),
        }
    }
    escaped
}

/// A JSON string body with every non-ASCII character as `\uXXXX`.
fn ascii_json(json: &str) -> String {
    let mut ascii = String::with_capacity(json.len() * 2);
    for c in json.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(ascii, "\\u{unit:04x}");
            }
        }
    }
    ascii
}

fn percent(value: &str, keep: impl Fn(u8) -> bool, plus: bool, upper: bool) -> String {
    let mut encoded = String::with_capacity(value.len() * 3);
    for &byte in value.as_bytes() {
        if keep(byte) {
            encoded.push(char::from(byte));
        } else if plus && byte == b' ' {
            encoded.push('+');
        } else if upper {
            let _ = write!(encoded, "%{byte:02X}");
        } else {
            let _ = write!(encoded, "%{byte:02x}");
        }
    }
    encoded
}

/// The base64 characters that depend on `value` alone when 0, 1 or 2 bytes
/// come before it in what was encoded, and some may follow it.
fn base64_anywhere(value: &str, engine: &GeneralPurpose) -> Vec<String> {
    let mut forms = Vec::new();
    for (before, skip) in [(0, 0), (1, 2), (2, 3)] {
        let mut bytes = Zeroizing::new(vec![0u8; before]);
        bytes.extend_from_slice(value.as_bytes());
        let encoded = Zeroizing::new(engine.encode(&*bytes));
        let end = bytes.len() / 3 * 4;
        if end > skip {
            forms.push(encoded[skip..end].to_owned());
        }
    }
    forms
}

/// `text` with each copy of `value` replaced by `marker`, where a copy may
/// have a backslash before any of its ASCII punctuation. `None` when there
/// is no copy.
fn replace_escaped(text: &str, value: &str, marker: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut cleaned = String::new();
    let (mut kept, mut at, mut found) = (0, 0, false);
    while at < bytes.len() {
        match escaped_copy_at(bytes, at, value.as_bytes()) {
            Some(end) => {
                cleaned.push_str(&text[kept..at]);
                cleaned.push_str(marker);
                (kept, at, found) = (end, end, true);
            }
            None => at += 1,
        }
    }
    found.then(|| {
        cleaned.push_str(&text[kept..]);
        cleaned
    })
}

/// Where a copy of `value` that starts at `at` ends.
fn escaped_copy_at(text: &[u8], mut at: usize, value: &[u8]) -> Option<usize> {
    for &byte in value {
        if byte.is_ascii_punctuation()
            && text.get(at) == Some(&b'\\')
            && text.get(at + 1) == Some(&byte)
        {
            at += 1;
        }
        if text.get(at) != Some(&byte) {
            return None;
        }
        at += 1;
    }
    Some(at)
}

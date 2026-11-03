//! A node's secrets never leave it in what a command prints. Every exact
//! copy of a secret value is replaced before output is sent. Output sent
//! while a command runs holds back an ending that could be the start of a
//! secret, so a secret printed across two sends is still matched whole.
//! A value that a command transforms (encodes, splits, reverses) is not
//! matched; neither is one cut at the output limit.

use zeroize::Zeroizing;

/// The shortest secret value: a shorter one would match ordinary output.
pub const MIN_SECRET_BYTES: usize = 8;

/// The values to find, longest first, each with the name its marker shows.
#[derive(Default)]
pub struct Secrets {
    values: Vec<(String, Zeroizing<String>)>,
}

impl Secrets {
    pub fn new(mut values: Vec<(String, Zeroizing<String>)>) -> Self {
        values.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
        Self { values }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `text` with every copy of every value replaced by its marker.
    pub fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (name, value) in &self.values {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), &format!("[redacted {name}]"));
            }
        }
        text
    }

    /// Where the longest end of `text` that could start a value begins.
    fn held_from(&self, text: &str) -> usize {
        let mut longest = 0;
        for (_, value) in &self.values {
            // Each char start but the first ends a proper prefix, longest last.
            for (at, _) in value.char_indices().rev() {
                if at <= longest {
                    break;
                }
                if text.ends_with(&value[..at]) {
                    longest = at;
                    break;
                }
            }
        }
        text.len() - longest
    }
}

/// One stream of one command, as it is sent.
#[derive(Default)]
pub struct Stream {
    held: String,
}

impl Stream {
    /// `text` printed since the last send, ready to send.
    pub fn send(&mut self, secrets: &Secrets, text: &str) -> String {
        if secrets.is_empty() {
            return text.to_owned();
        }
        let mut all = std::mem::take(&mut self.held);
        all.push_str(text);
        let mut redacted = secrets.redact(&all);
        let from = secrets.held_from(&redacted);
        self.held = redacted.split_off(from);
        redacted
    }

    /// The last of the stream: whatever was held back, then `text`.
    pub fn finish(&mut self, secrets: &Secrets, text: &str) -> String {
        let mut all = std::mem::take(&mut self.held);
        all.push_str(text);
        secrets.redact(&all)
    }
}

#[cfg(test)]
mod tests {
    use super::{Secrets, Stream};
    use zeroize::Zeroizing;

    fn secrets() -> Secrets {
        Secrets::new(vec![
            (
                "API_TOKEN".into(),
                Zeroizing::new("s3cr3t-token-value".into()),
            ),
            ("short".into(), Zeroizing::new("hunter22".into())),
        ])
    }

    #[test]
    fn every_copy_of_a_secret_is_replaced_by_its_name() {
        let secrets = secrets();
        assert_eq!(
            secrets.redact("a s3cr3t-token-value b hunter22 s3cr3t-token-value"),
            "a [redacted API_TOKEN] b [redacted short] [redacted API_TOKEN]"
        );
        assert_eq!(secrets.redact("nothing here"), "nothing here");
    }

    #[test]
    fn a_secret_printed_across_sends_is_held_back_and_replaced_whole() {
        let secrets = secrets();
        let mut stream = Stream::default();
        let mut sent = String::new();
        for piece in ["log: s3cr", "3t-tok", "en-value\nnext hun", "ter2"] {
            let out = stream.send(&secrets, piece);
            assert!(!out.contains("s3cr") && !out.contains("hun"), "{out:?}");
            sent.push_str(&out);
        }
        sent.push_str(&stream.finish(&secrets, "2 done"));
        assert_eq!(
            sent,
            "log: [redacted API_TOKEN]\nnext [redacted short] done"
        );

        let mut stream = Stream::default();
        let out = stream.send(&secrets, "ends with s3");
        assert_eq!(out, "ends with ");
        assert_eq!(
            stream.finish(&secrets, "x"),
            "s3x",
            "a false start is sent at the end"
        );
    }

    #[test]
    fn without_secrets_output_passes_as_it_is() {
        let none = Secrets::default();
        let mut stream = Stream::default();
        assert_eq!(stream.send(&none, "héllo s3"), "héllo s3");
        assert_eq!(stream.finish(&none, ""), "");
    }
}

//! The admission keys of a relay: one for each customer copy and each
//! technician (ST-M007-014, ADR-0016 §4).
//!
//! An operator gives every copy a key of its own and can withdraw one without
//! touching the others. The keys live in a text file that the relay reads at
//! startup and again whenever it changes.
//!
//! # File format
//!
//! One `label:key` per line:
//!
//! ```text
//! # customers
//! acme:5f2c…
//! frontdesk:9a41…
//! ```
//!
//! - A **label** names who the key belongs to. It holds ASCII letters, digits,
//!   `.`, `_` and `-`, at most [`MAX_LABEL_CHARS`] characters. Metrics and
//!   operational events name the label; they never hold a key.
//! - A **key** is everything after the first colon, without the blanks around
//!   it: [`MIN_KEY_BYTES`] to [`MAX_PREAMBLE_TOKEN_BYTES`] printable ASCII
//!   characters without a blank. `cretlink-relay-server keygen` prints one.
//! - Blank lines and lines that start with `#` are ignored.
//! - Every label and every key is listed once. A file that breaks a rule is
//!   refused as a whole, with the number of the line: at startup the relay
//!   does not start, and after a change it keeps the keys it had.
//!
//! # What a key is
//!
//! A key admits a connection to the forwarder and decides nothing else. It
//! gives no control over a session and repairs no missing session authority:
//! trust in a session comes only from the end-to-end handshake and the
//! approval code that two people compare. The relay holds the keys only to
//! keep strangers from using it as an anonymous forwarding service.
//!
//! No message of this module ever contains a key: an error names the line and
//! the label.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cretlink_relay::preamble::MAX_PREAMBLE_TOKEN_BYTES;
use cretlink_relay::{AdmissionControl, AdmissionPolicy, AdmissionToken, ClientId, StopHandle};
use thiserror::Error;

/// The largest key file the relay reads, in bytes.
pub const MAX_KEYS_FILE_BYTES: u64 = 1024 * 1024;

/// The most keys one file may list.
pub const MAX_KEYS: usize = 10_000;

/// The longest label, in characters.
pub const MAX_LABEL_CHARS: usize = 64;

/// The shortest key, in characters. A shorter one is too easy to guess.
pub const MIN_KEY_BYTES: usize = 16;

/// How often a running relay looks whether its key file changed.
pub const DEFAULT_KEYS_POLL: Duration = Duration::from_secs(5);

/// The keys of a file, each with the label it is accounted under: the entries
/// of an [`AdmissionPolicy::Allowlist`].
pub type Keys = Vec<(AdmissionToken, ClientId)>;

/// Why a key file was refused. Every variant names the line and, where there
/// is one, the label; none holds a key (REQ-N-015, REQ-S-017).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeysError {
    /// The file could not be read.
    #[error("the key file {path} could not be read: {reason}")]
    Unreadable {
        /// The path as it was given.
        path: String,
        /// The reason the operating system gave.
        reason: String,
    },
    /// The file is larger than [`MAX_KEYS_FILE_BYTES`].
    #[error("the key file is larger than {MAX_KEYS_FILE_BYTES} bytes")]
    TooLarge,
    /// The file is not UTF-8 text.
    #[error("the key file is not a text file")]
    NotText,
    /// A line has no colon between a label and a key.
    #[error("line {line}: expected `label:key`")]
    NoSeparator {
        /// The line, counted from 1.
        line: usize,
    },
    /// A line has nothing before the colon.
    #[error("line {line}: the label is missing")]
    EmptyLabel {
        /// The line, counted from 1.
        line: usize,
    },
    /// A label holds something else than letters, digits, `.`, `_` and `-`,
    /// or is too long.
    #[error(
        "line {line}: a label holds letters, digits, `.`, `_` and `-`, at most {MAX_LABEL_CHARS} characters"
    )]
    BadLabel {
        /// The line, counted from 1.
        line: usize,
    },
    /// A key is shorter than [`MIN_KEY_BYTES`].
    #[error("line {line}: the key of `{label}` is shorter than {MIN_KEY_BYTES} characters")]
    KeyTooShort {
        /// The line, counted from 1.
        line: usize,
        /// The label of that line.
        label: String,
    },
    /// A key is longer than a rendezvous preamble can carry.
    #[error(
        "line {line}: the key of `{label}` is longer than {MAX_PREAMBLE_TOKEN_BYTES} characters"
    )]
    KeyTooLong {
        /// The line, counted from 1.
        line: usize,
        /// The label of that line.
        label: String,
    },
    /// A key holds a blank, a control character or a character outside ASCII.
    #[error(
        "line {line}: the key of `{label}` holds a blank or a character that is not plain text"
    )]
    BadKey {
        /// The line, counted from 1.
        line: usize,
        /// The label of that line.
        label: String,
    },
    /// A label is listed twice.
    #[error("line {line}: the label `{label}` is already used on line {first}")]
    DuplicateLabel {
        /// The line, counted from 1.
        line: usize,
        /// The line of its first use.
        first: usize,
        /// The label.
        label: String,
    },
    /// A key is listed twice: two labels would then share one key, and
    /// withdrawing one would not withdraw the other.
    #[error(
        "line {line}: this key is already listed on line {first}; every label gets a key of its own"
    )]
    DuplicateKey {
        /// The line, counted from 1.
        line: usize,
        /// The line of its first use.
        first: usize,
    },
    /// The file lists no key at all: nobody could use the relay.
    #[error("the key file lists no key, so nobody could use the relay")]
    Empty,
    /// The file lists more than [`MAX_KEYS`] keys.
    #[error("the key file lists more than {MAX_KEYS} keys")]
    TooMany,
}

/// Parses the text of a key file. See the [module documentation](self) for the
/// format.
///
/// # Errors
///
/// The first rule the text breaks, as a [`KeysError`].
pub fn parse_keys(text: &str) -> Result<Keys, KeysError> {
    // Some editors put a byte-order mark in front of a text file.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // The entries so far, and the line of every label and key, to find a
    // second use.
    let mut entries: Vec<(&str, &str)> = Vec::new();
    let mut label_lines: HashMap<&str, usize> = HashMap::new();
    let mut key_lines: HashMap<&str, usize> = HashMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let entry = raw.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let (label, key) = entry
            .split_once(':')
            .ok_or(KeysError::NoSeparator { line })?;
        let (label, key) = (label.trim(), key.trim());

        if label.is_empty() {
            return Err(KeysError::EmptyLabel { line });
        }
        let label_ok = label.len() <= MAX_LABEL_CHARS
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !label_ok {
            return Err(KeysError::BadLabel { line });
        }
        let label_of_line = || label.to_string();
        if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(KeysError::BadKey {
                line,
                label: label_of_line(),
            });
        }
        if key.len() < MIN_KEY_BYTES {
            return Err(KeysError::KeyTooShort {
                line,
                label: label_of_line(),
            });
        }
        if key.len() > MAX_PREAMBLE_TOKEN_BYTES {
            return Err(KeysError::KeyTooLong {
                line,
                label: label_of_line(),
            });
        }
        if let Some(first) = label_lines.get(label) {
            return Err(KeysError::DuplicateLabel {
                line,
                first: *first,
                label: label_of_line(),
            });
        }
        if let Some(first) = key_lines.get(key) {
            return Err(KeysError::DuplicateKey {
                line,
                first: *first,
            });
        }
        if entries.len() == MAX_KEYS {
            return Err(KeysError::TooMany);
        }
        label_lines.insert(label, line);
        key_lines.insert(key, line);
        entries.push((label, key));
    }
    if entries.is_empty() {
        return Err(KeysError::Empty);
    }
    Ok(entries
        .into_iter()
        .map(|(label, key)| (AdmissionToken::from_text(key), ClientId::new(label)))
        .collect())
}

/// Reads and parses a key file.
///
/// # Errors
///
/// [`KeysError::Unreadable`], [`KeysError::TooLarge`] or
/// [`KeysError::NotText`] for the file itself, otherwise what
/// [`parse_keys`] reports.
pub fn load_keys(path: &Path) -> Result<Keys, KeysError> {
    let unreadable = |error: std::io::Error| KeysError::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    let size = std::fs::metadata(path).map_err(unreadable)?.len();
    if size > MAX_KEYS_FILE_BYTES {
        return Err(KeysError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    // The file may have grown between the two calls.
    if bytes.len() as u64 > MAX_KEYS_FILE_BYTES {
        return Err(KeysError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| KeysError::NotText)?;
    parse_keys(&text)
}

/// What tells one state of the key file from another: when it was last
/// written and how long it is. `None` while the file cannot be read.
fn fingerprint(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Applies a changed key file to a running relay until `stop` fires
/// (ST-M007-014).
///
/// Every `poll` it looks whether the file was written since. A file that
/// parses replaces the admission policy for every leg that arrives from then
/// on; sessions in progress are not touched. A file that cannot be read or
/// breaks a rule changes nothing: the keys that were in force stay in force,
/// and the reason is logged with the line and the label, never with a key.
pub async fn watch_keys(
    path: PathBuf,
    control: AdmissionControl,
    poll: Duration,
    mut stop: StopHandle,
) {
    let mut seen = fingerprint(&path);
    loop {
        tokio::select! {
            () = stop.stopped() => return,
            () = tokio::time::sleep(poll) => {}
        }
        let now = fingerprint(&path);
        if now == seen {
            continue;
        }
        seen = now;
        let file = path.clone();
        let loaded = tokio::task::spawn_blocking(move || load_keys(&file)).await;
        match loaded {
            Ok(Ok(keys)) => {
                let count = keys.len();
                if control.replace_policy(AdmissionPolicy::Allowlist(keys)) {
                    eprintln!("relay: the key file changed; {count} keys are admitted from now on");
                } else {
                    eprintln!("relay: the key file changed, but the keys could not be replaced");
                }
            }
            Ok(Err(error)) => eprintln!(
                "relay: the key file changed and cannot be used; the keys from before stay in force: {error}"
            ),
            Err(_) => eprintln!("relay: the key file could not be read again"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const KEY_A: &str = "0123456789abcdef0123456789abcdef";
    const KEY_B: &str = "fedcba9876543210fedcba9876543210";

    fn labels(keys: &Keys) -> Vec<&str> {
        keys.iter().map(|(_key, label)| label.as_str()).collect()
    }

    #[test]
    fn a_file_lists_one_key_per_label() {
        let text = format!("# customers\n\nacme:{KEY_A}\n  frontdesk : {KEY_B}  \n# end\n");
        let keys = parse_keys(&text).unwrap();
        assert_eq!(labels(&keys), ["acme", "frontdesk"]);
        assert_eq!(keys[0].0.as_bytes(), KEY_A.as_bytes());
        assert_eq!(keys[1].0.as_bytes(), KEY_B.as_bytes());
    }

    #[test]
    fn a_file_from_a_windows_editor_reads_the_same() {
        // A byte-order mark in front and line ends of two characters.
        let text = format!("\u{feff}acme:{KEY_A}\r\nfrontdesk:{KEY_B}\r\n");
        let keys = parse_keys(&text).unwrap();
        assert_eq!(labels(&keys), ["acme", "frontdesk"]);
        assert_eq!(keys[1].0.as_bytes(), KEY_B.as_bytes());
    }

    #[test]
    fn a_key_may_hold_a_colon_and_a_label_other_marks() {
        let key = "part-one:part-two:three";
        let keys = parse_keys(&format!("mom-laptop.home_1:{key}")).unwrap();
        assert_eq!(labels(&keys), ["mom-laptop.home_1"]);
        assert_eq!(keys[0].0.as_bytes(), key.as_bytes());
    }

    #[test]
    fn a_line_that_breaks_a_rule_is_named_by_its_number() {
        let long_label = "a".repeat(MAX_LABEL_CHARS + 1);
        let long_key = "k".repeat(MAX_PREAMBLE_TOKEN_BYTES + 1);
        let cases: Vec<(String, KeysError)> = vec![
            ("acme".into(), KeysError::NoSeparator { line: 1 }),
            (format!("\n:{KEY_A}"), KeysError::EmptyLabel { line: 2 }),
            (format!("ac me:{KEY_A}"), KeysError::BadLabel { line: 1 }),
            (
                format!("acm\u{e9}:{KEY_A}"),
                KeysError::BadLabel { line: 1 },
            ),
            (
                format!("{long_label}:{KEY_A}"),
                KeysError::BadLabel { line: 1 },
            ),
            (
                "acme:short".into(),
                KeysError::KeyTooShort {
                    line: 1,
                    label: "acme".into(),
                },
            ),
            (
                "acme:".into(),
                KeysError::KeyTooShort {
                    line: 1,
                    label: "acme".into(),
                },
            ),
            (
                format!("acme:{long_key}"),
                KeysError::KeyTooLong {
                    line: 1,
                    label: "acme".into(),
                },
            ),
            (
                "acme:0123456789 abcdef0123".into(),
                KeysError::BadKey {
                    line: 1,
                    label: "acme".into(),
                },
            ),
            (
                format!("acme:{KEY_A}\n# note\nacme:{KEY_B}"),
                KeysError::DuplicateLabel {
                    line: 3,
                    first: 1,
                    label: "acme".into(),
                },
            ),
            (
                format!("acme:{KEY_A}\nfrontdesk:{KEY_A}"),
                KeysError::DuplicateKey { line: 2, first: 1 },
            ),
            (String::new(), KeysError::Empty),
            ("# nothing\n\n".into(), KeysError::Empty),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_keys(&text).unwrap_err(), expected, "{text:?}");
        }
    }

    #[test]
    fn no_error_ever_shows_a_key() {
        let secret = "this-is-the-secret-key-0001";
        for text in [
            format!("acme:{secret}\nacme:{secret}-other"),
            format!("acme:{secret}\nfrontdesk:{secret}"),
            format!("acme:{secret} with a blank"),
            format!("ac me:{secret}"),
        ] {
            let error = parse_keys(&text).unwrap_err();
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn the_number_of_keys_is_bounded() {
        let mut text = String::new();
        for index in 0..=MAX_KEYS {
            text.push_str(&format!("label-{index}:key-number-{index:012}\n"));
        }
        assert_eq!(parse_keys(&text).unwrap_err(), KeysError::TooMany);
    }

    #[test]
    fn a_file_is_read_with_its_bounds() {
        let dir = std::env::temp_dir().join(format!(
            "cretlink-relay-keys-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("keys.txt");
        std::fs::write(&good, format!("acme:{KEY_A}\n")).unwrap();
        assert_eq!(labels(&load_keys(&good).unwrap()), ["acme"]);

        let missing = dir.join("absent.txt");
        assert!(matches!(
            load_keys(&missing).unwrap_err(),
            KeysError::Unreadable { .. }
        ));

        let binary = dir.join("binary.txt");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(load_keys(&binary).unwrap_err(), KeysError::NotText);

        let huge = dir.join("huge.txt");
        std::fs::write(&huge, vec![b'#'; MAX_KEYS_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(load_keys(&huge).unwrap_err(), KeysError::TooLarge);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

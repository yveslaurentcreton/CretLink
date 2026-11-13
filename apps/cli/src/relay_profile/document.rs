//! The small form of TOML that CretLink's own files use (ST-M007-011).
//!
//! `cretlink.toml` and the settings of the installed copy hold a few short
//! texts under one or two section headers:
//!
//! ```toml
//! # a comment
//! [relay]
//! address = "relay.example.org:443"
//! name = 'relay.example.org'
//! ```
//!
//! This reader takes exactly that and refuses everything else with the number
//! of the line: a section header `[name]`, a setting `name = "text"` on one
//! line, with the text in double quotes (with the escapes of TOML) or in
//! single quotes (taken as written), blank lines and comments. Numbers,
//! lists, inline tables, dotted names, quoted names and texts over several
//! lines are valid TOML that these files have no use for; they are refused
//! instead of half understood.
//!
//! Such a file may hold an admission key, and it may have been written by
//! somebody else. So the reader is bounded in every direction, and **no
//! message it produces holds text from the file**: a problem names the line,
//! and the setting only when it is one the caller listed as known.

use zeroize::Zeroizing;

use super::{Field, Problem};

/// A setting that a message may name, and the setting of a relay profile it
/// is, when it is one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Known {
    pub(crate) name: &'static str,
    pub(crate) field: Option<Field>,
}

/// The longest name of a section or a setting.
const MAX_NAME_CHARS: usize = 32;

/// The longest text of one setting, in bytes.
pub(crate) const MAX_VALUE_BYTES: usize = 1024;

/// The most sections one file may have.
const MAX_SECTIONS: usize = 8;

/// The most settings one section may have.
const MAX_SETTINGS: usize = 16;

/// What a file holds: its sections in the order they stand in.
#[derive(Debug, Default)]
pub(crate) struct Document {
    pub(crate) sections: Vec<Section>,
}

/// One `[name]` with the settings under it.
#[derive(Debug)]
pub(crate) struct Section {
    pub(crate) name: String,
    /// The line of the header, counted from 1.
    pub(crate) line: usize,
    pub(crate) settings: Vec<Setting>,
}

/// One `name = "text"`.
pub(crate) struct Setting {
    pub(crate) name: String,
    /// The line it stands on, counted from 1.
    pub(crate) line: usize,
    /// The text, erased when dropped: it may be an admission key.
    pub(crate) value: Zeroizing<String>,
}

/// Redacted: a setting may be an admission key.
impl std::fmt::Debug for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setting")
            .field("name", &self.name)
            .field("line", &self.line)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl Document {
    /// The section called `name`, if the file has it.
    pub(crate) fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|section| section.name == name)
    }
}

/// Reads `text`. `known` lists the settings that a message may name; any
/// other setting is called "this setting", so that nothing typed into the
/// file by mistake is repeated in a message.
pub(crate) fn parse(text: &str, known: &[Known]) -> Result<Document, Problem> {
    // An editor on Windows may put a byte-order mark first.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut document = Document::default();
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.chars().any(|c| c.is_control() && c != '\t') {
            return Err(Problem::at(
                line,
                "this line holds a character that is not text",
            ));
        }
        let content = raw.trim_matches([' ', '\t']);
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if let Some(rest) = content.strip_prefix('[') {
            let name = header(rest, line)?;
            if let Some(first) = document.section(&name) {
                return Err(Problem::at(
                    line,
                    format!("this section is already on line {}", first.line),
                ));
            }
            if document.sections.len() == MAX_SECTIONS {
                return Err(Problem::at(line, "the file has too many sections"));
            }
            document.sections.push(Section {
                name,
                line,
                settings: Vec::new(),
            });
            continue;
        }
        let (name, value) = setting(content, line, known)?;
        let Some(section) = document.sections.last_mut() else {
            return Err(Problem::at(
                line,
                "this setting stands before a section; a section starts with a line such as [relay]",
            ));
        };
        if let Some(first) = section.settings.iter().find(|other| other.name == name) {
            return Err(about(
                &name,
                known,
                line,
                format!(
                    "{} is already set on line {}",
                    called(&name, known),
                    first.line
                ),
            ));
        }
        if section.settings.len() == MAX_SETTINGS {
            return Err(Problem::at(line, "the section has too many settings"));
        }
        section.settings.push(Setting { name, line, value });
    }
    Ok(document)
}

/// How a message names a setting: by its name when the caller knows it.
fn called(name: &str, known: &[Known]) -> String {
    if known.iter().any(|known| known.name == name) {
        format!("`{name}`")
    } else {
        "this setting".to_owned()
    }
}

/// A problem with the setting called `name` on `line`.
fn about(name: &str, known: &[Known], line: usize, text: String) -> Problem {
    Problem {
        line: Some(line),
        field: known
            .iter()
            .find(|known| known.name == name)
            .and_then(|known| known.field),
        text,
    }
}

/// Whether `text` is a name as these files write it: letters, digits, `_`
/// and `-`.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_NAME_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// What may follow a header or a text on its line: nothing, or a comment.
fn only_a_comment_follows(rest: &str) -> bool {
    let rest = rest.trim_start_matches([' ', '\t']);
    rest.is_empty() || rest.starts_with('#')
}

/// Reads a section header after its opening bracket.
fn header(rest: &str, line: usize) -> Result<String, Problem> {
    let plain = || {
        Problem::at(
            line,
            "a section is one name in square brackets, such as [relay]",
        )
    };
    let (name, after) = rest.split_once(']').ok_or_else(plain)?;
    let name = name.trim_matches([' ', '\t']);
    if !is_name(name) || !only_a_comment_follows(after) {
        return Err(plain());
    }
    Ok(name.to_owned())
}

/// Reads `name = "text"`.
fn setting(
    content: &str,
    line: usize,
    known: &[Known],
) -> Result<(String, Zeroizing<String>), Problem> {
    let not_a_setting = || {
        Problem::at(
            line,
            "this line is not a setting; a setting reads: name = \"text\"",
        )
    };
    let (name, rest) = content.split_once('=').ok_or_else(not_a_setting)?;
    let name = name.trim_matches([' ', '\t']);
    if !is_name(name) {
        return Err(not_a_setting());
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let what = called(name, known);
    let problem = |text: String| about(name, known, line, text);
    let (value, after) = if let Some(body) = rest.strip_prefix('"') {
        if body.starts_with("\"\"") {
            return Err(problem(format!(
                "the text of {what} must stand on one line, between two double quotes"
            )));
        }
        basic_string(body).map_err(|wrong| {
            problem(match wrong {
                BadString::Escape => format!(
                    "the text of {what} holds a backslash that starts no escape of TOML; write \
                     a backslash twice, or put the text between single quotes"
                ),
                BadString::TooLong => format!("the text of {what} is too long"),
                BadString::Open => {
                    format!("the text of {what} is not closed with a double quote")
                }
            })
        })?
    } else if let Some(body) = rest.strip_prefix('\'') {
        if body.starts_with("''") {
            return Err(problem(format!(
                "the text of {what} must stand on one line, between two quotes"
            )));
        }
        let (text, after) = body
            .split_once('\'')
            .ok_or_else(|| problem(format!("the text of {what} is not closed with a quote")))?;
        (Zeroizing::new(text.to_owned()), after)
    } else {
        return Err(problem(format!(
            "the value of {what} must be text in double quotes"
        )));
    };
    if !only_a_comment_follows(after) {
        return Err(problem(format!(
            "something follows the closing quote of {what}"
        )));
    }
    if value.len() > MAX_VALUE_BYTES {
        return Err(problem(format!("the text of {what} is too long")));
    }
    Ok((name.to_owned(), value))
}

/// Why a text in double quotes could not be read.
enum BadString {
    /// A backslash that starts no escape of TOML.
    Escape,
    /// Longer than [`MAX_VALUE_BYTES`].
    TooLong,
    /// No closing quote on the line.
    Open,
}

/// Reads a text in double quotes after its opening quote, with the escapes of
/// TOML. Returns the text and what follows the closing quote.
fn basic_string(body: &str) -> Result<(Zeroizing<String>, &str), BadString> {
    let mut value = Zeroizing::new(String::new());
    let mut chars = body.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Ok((value, &body[index + 1..])),
            '\\' => {
                let (_, escape) = chars.next().ok_or(BadString::Escape)?;
                match escape {
                    'b' => value.push('\u{8}'),
                    't' => value.push('\t'),
                    'n' => value.push('\n'),
                    'f' => value.push('\u{c}'),
                    'r' => value.push('\r'),
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'u' | 'U' => {
                        let digits = if escape == 'u' { 4 } else { 8 };
                        let mut code: u32 = 0;
                        for _ in 0..digits {
                            let digit = chars
                                .next()
                                .and_then(|(_, hex)| hex.to_digit(16))
                                .ok_or(BadString::Escape)?;
                            code = code * 16 + digit;
                        }
                        value.push(char::from_u32(code).ok_or(BadString::Escape)?);
                    }
                    _ => return Err(BadString::Escape),
                }
            }
            other => value.push(other),
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(BadString::TooLong);
        }
    }
    Err(BadString::Open)
}

/// Writes `text` as a TOML text in double quotes, so that [`parse`] reads the
/// same text back.
pub(crate) fn quoted(text: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(text.len() + 2));
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

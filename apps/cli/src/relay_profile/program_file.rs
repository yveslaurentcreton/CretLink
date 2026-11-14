//! The relay file next to the program, written on the technician's request
//! (ST-M007-011).
//!
//! A technician copy that runs once keeps no settings. Its relay stands in
//! the file [`PROFILE_FILE`] next to the program ([`path_next_to`]), which it
//! reads like every copy does. **Settings → Relay** of such a copy fills its
//! form from that file, and **Save** and **Remove** change it here:
//!
//! - **One place.** The profile file is the one next to the running program
//!   and nothing else; the certificate authority file is [`CA_FILE`] beside
//!   it, under that one name. No path comes from the window.
//! - **Only on request.** Nothing here runs unless the technician pressed
//!   Save or Remove: it is the one thing besides the audit trail that a copy
//!   that runs once writes. The customer role never calls it.
//! - **Whole or not at all.** The profile file and the certificate file
//!   change together, as the settings of the installed copy do
//!   ([`settings::write_with_relay_ca`]): new files are made in
//!   the same folder and then moved into place; when a step fails, both are
//!   as they were.
//! - **Never through a link.** When the profile file or the certificate file
//!   is there and is not a plain file (a symbolic link, a junction or another
//!   reparse point, a folder), nothing is written or removed.
//! - **For this user only, where the system allows.** On macOS and Linux the
//!   files are made with mode `0600`. On Windows they take the access rules
//!   of the folder of the program.
//! - **What a person wrote stays.** Save replaces the settings of the
//!   `[relay]` section; comments stay where they stood, the header with its
//!   comment too. A file that cannot be read as a relay profile is not
//!   overwritten ([`WriteError::Unreadable`]); Remove removes it.
//! - **The key is in the file in readable form**, as in every profile file.
//!   [`text_by_hand`], the text a person can write when the folder cannot be
//!   written, never holds it.
//!
//! [`path_next_to`]: super::path_next_to
//! [`PROFILE_FILE`]: super::PROFILE_FILE

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use super::{MAX_PROFILE_BYTES, Problem, ProfileError, RelayProfile, parse, read_bounded};
use crate::install::RELAY_CA_FILE;
use crate::settings::{self, RelayCa};

/// The name of the certificate authority file that Save writes beside the
/// profile file, and the one file of that kind that Remove removes.
pub const CA_FILE: &str = RELAY_CA_FILE;

/// The line that stands for the key in [`text_by_hand`].
pub const KEY_PLACEHOLDER: &str = "key = \"the key of this copy\"";

/// Why the relay file next to the program was not written or removed.
#[derive(Debug)]
pub enum WriteError {
    /// The profile file there cannot be read as a relay profile. Save does
    /// not replace what it cannot read; Remove removes it.
    Unreadable(ProfileError),
    /// The profile file or the certificate file is there and is not a plain
    /// file: nothing is written or removed through it.
    NotAPlainFile(PathBuf),
    /// No new file could be made in the folder of the program: it cannot be
    /// written, or is not there. Nothing was changed.
    FolderNotWritable(io::Error),
    /// Another error of the file system. Nothing was changed, except in the
    /// one case that [`settings::is_not_put_back`] says.
    Io(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => error.fmt(f),
            Self::NotAPlainFile(path) => write!(
                f,
                "{} is not a plain file: it is a link or a folder, and nothing is written or \
                 removed through it",
                path.display()
            ),
            Self::FolderNotWritable(error) => {
                write!(f, "the folder of the program cannot be written: {error}")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WriteError {}

fn of_io(error: io::Error) -> WriteError {
    if settings::is_not_created(&error) {
        WriteError::FolderNotWritable(error)
    } else {
        WriteError::Io(error)
    }
}

/// The certificate authority file beside the profile file `profile_file`.
#[must_use]
pub fn ca_path(profile_file: &Path) -> PathBuf {
    profile_file.with_file_name(CA_FILE)
}

/// Refuses a file that is there and is not a plain file. A file that is not
/// there is fine.
fn plain_or_absent(path: &Path) -> Result<(), WriteError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(WriteError::Io(error)),
    };
    if meta.file_type().is_file() && !is_reparse_point(&meta) {
        Ok(())
    } else {
        Err(WriteError::NotAPlainFile(path.to_path_buf()))
    }
}

/// Whether a file is a reparse point: a junction, a symbolic link or any
/// other file that the system resolves to something else.
#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &fs::Metadata) -> bool {
    false
}

/// Writes `profile` to the profile file `profile_file` next to the program,
/// with its certificate authority file [`CA_FILE`] beside it, as one change.
///
/// `profile` names [`CA_FILE`] when `ca` replaces or keeps that file, and no
/// file when `ca` removes it. Comments of the file that is there stay; its
/// relay settings are replaced.
///
/// # Errors
///
/// [`WriteError`]: nothing was written then, with the one exception that
/// [`settings::is_not_put_back`] says of a [`WriteError::Io`].
pub fn write(
    profile_file: &Path,
    profile: &RelayProfile,
    ca: RelayCa<'_>,
) -> Result<(), WriteError> {
    let ca_file = ca_path(profile_file);
    plain_or_absent(profile_file)?;
    plain_or_absent(&ca_file)?;
    let before = read_text(profile_file)?;
    let text = merged(before.as_deref().map(String::as_str), &profile.to_section());
    settings::write_together(profile_file, Some(text.as_bytes()), &ca_file, ca).map_err(of_io)
}

/// Removes the profile file `profile_file` next to the program, and the
/// certificate authority file [`CA_FILE`] beside it, as one change. A
/// certificate file that a profile written by hand names under another name
/// is left alone. A profile file that cannot be read is removed as well.
///
/// # Errors
///
/// [`WriteError`]: nothing was removed then, with the one exception that
/// [`settings::is_not_put_back`] says of a [`WriteError::Io`].
pub fn remove(profile_file: &Path) -> Result<(), WriteError> {
    let ca_file = ca_path(profile_file);
    plain_or_absent(profile_file)?;
    plain_or_absent(&ca_file)?;
    settings::write_together(profile_file, None, &ca_file, RelayCa::Remove).map_err(of_io)
}

/// The text of the profile file that is there, when it can be read as a
/// relay profile; `None` when there is none. It is read as every copy reads
/// it ([`super::read_file`]): a file that a copy cannot use as a profile is not one
/// that Save may change.
fn read_text(path: &Path) -> Result<Option<Zeroizing<String>>, WriteError> {
    let unreadable = |problem: Problem| {
        WriteError::Unreadable(ProfileError {
            file: path.to_path_buf(),
            problem,
        })
    };
    let Some(bytes) = read_bounded(path, MAX_PROFILE_BYTES, "the file").map_err(unreadable)? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| unreadable(Problem::file("the file is not text (UTF-8)")))?;
    parse(text).map_err(unreadable)?;
    Ok(Some(Zeroizing::new(text.to_owned())))
}

/// The text of a profile file that held `before` (a text that
/// [`super::parse`] reads) with the relay `section` of
/// [`RelayProfile::to_section`] in place of its relay settings.
///
/// Every line that is not a setting stays where it stood: comments, blank
/// lines and the `[relay]` header with a comment after it. The new settings
/// stand right under the header. A file without a `[relay]` section gets
/// one at its end. The line ends of the file are kept.
#[must_use]
pub(crate) fn merged(before: Option<&str>, section: &str) -> Zeroizing<String> {
    let before = before.unwrap_or_default();
    let newline = if before.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    // The settings of the section, without its header.
    let mut settings = Zeroizing::new(String::new());
    for line in section.lines().skip(1) {
        settings.push_str(line);
        settings.push_str(newline);
    }
    let mut out = Zeroizing::new(String::with_capacity(before.len() + section.len()));
    let mut in_section = false;
    for raw in before.split_inclusive('\n') {
        let content = raw
            .trim_end_matches(['\n', '\r'])
            .trim_start_matches('\u{feff}')
            .trim_matches([' ', '\t']);
        if !in_section && content.starts_with('[') {
            // The one section a profile file may have: [relay].
            out.push_str(raw);
            if !raw.ends_with('\n') {
                out.push_str(newline);
            }
            out.push_str(&settings);
            in_section = true;
            continue;
        }
        let is_setting = !content.is_empty() && !content.starts_with('#');
        if in_section && is_setting {
            continue;
        }
        out.push_str(raw);
    }
    if !in_section {
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(newline);
            }
            out.push_str(newline);
        }
        out.push_str("[relay]");
        out.push_str(newline);
        out.push_str(&settings);
    }
    out
}

/// The text of a profile file for `profile` that a person writes by hand:
/// what Save would write, with a line that stands for the key in place of
/// the key ([`KEY_PLACEHOLDER`]). It never holds the key.
#[must_use]
pub fn text_by_hand(profile: &RelayProfile) -> String {
    let had_key = profile.key().is_some();
    let section = profile.clone().with_key(None).to_section();
    let mut out = String::with_capacity(section.len() + KEY_PLACEHOLDER.len() + 1);
    for (index, line) in section.lines().enumerate() {
        out.push_str(line);
        out.push('\n');
        // The address comes first, right under the header.
        if index == 1 && had_key {
            out.push_str(KEY_PLACEHOLDER);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests;

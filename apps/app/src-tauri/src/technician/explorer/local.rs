//! The technician computer's side of the explorer's transfers (ST-M010-002).
//!
//! A download writes files on this computer under names that the customer
//! computer gives, and a send reads a folder that the technician dropped.
//! This module holds the rules for both, as pure functions of names and of
//! this computer's own disk:
//!
//! - [`local_name`]: whether a name from the customer computer may be a name
//!   on this computer. **Every name from the customer computer is untrusted**:
//!   a listing is an answer of the other computer, and nothing in it may make
//!   a file land outside the folder the technician chose. A name is taken
//!   only as one plain name. Anything else — a separator, a drive or a
//!   stream, `.` or `..`, a reserved device name, a trailing dot or space —
//!   is refused, never repaired: a repaired name would be a file the customer
//!   computer does not have.
//! - [`target_folder_name`]: the folder name for a customer computer under
//!   `Downloads\CretLink`. The host name is the other computer's own word
//!   too; here it only labels a folder, so it is made safe instead of refused.
//! - [`kept_beside`]: the name of a second copy beside one that is taken, in
//!   the pattern Windows uses: `name (2).ext`.
//! - [`walk`]: the files and folders of a dropped folder, bounded in number
//!   and in depth, with a link never followed and everything that was left
//!   out named.

use std::path::{Component, Path, PathBuf};

/// The suffix of the part a download keeps beside its destination
/// ([`cretlink_session::download_part_path`]).
const PART_SUFFIX: &str = ".cretlink-part";

/// The most UTF-16 units of one name on this computer, as Windows has it.
const MAX_NAME_UNITS: usize = 255;

/// Whether `name`, as the customer computer gives it, may be the name of a
/// file or a folder on this computer: itself when it may, else why not, in
/// words that finish the sentence "its name …".
pub(crate) fn local_name(name: &str) -> Result<&str, &'static str> {
    if name.is_empty() {
        return Err("is empty");
    }
    if name == "." || name == ".." {
        return Err("stands for a folder, not for an entry");
    }
    if name.contains(['/', '\\']) {
        return Err("holds a path separator");
    }
    if name.contains(':') {
        return Err("holds a colon");
    }
    if name.chars().any(char::is_control) {
        return Err("holds a control character");
    }
    if name.contains(['<', '>', '"', '|', '?', '*']) {
        return Err("holds a character that a file name cannot have");
    }
    // Windows drops a trailing dot or space, so such a name is another file's.
    if name.ends_with(['.', ' ']) {
        return Err("ends in a dot or a space");
    }
    if is_device_name(name) {
        return Err("is the name of a device");
    }
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return Err("is too long");
    }
    if name.to_ascii_lowercase().ends_with(PART_SUFFIX) {
        return Err("is one CretLink keeps for an unfinished download");
    }
    Ok(name)
}

/// Whether the part of `name` before its first dot names a device of Windows:
/// `CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9` (the digit
/// also as `¹`, `²` or `³`), `CONIN$` and `CONOUT$`. Windows opens the device
/// whatever the folder and whatever follows the dot.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|rest| {
            let mut digits = rest.chars();
            matches!(
                (digits.next(), digits.next()),
                (Some('1'..='9' | '¹' | '²' | '³'), None)
            )
        })
    })
}

/// The name of the folder that holds what was copied from the customer
/// computer `hostname`, under `Downloads\CretLink`. Letters, digits, `-`, `_`
/// and `.` stay; anything else becomes `_`; a name that is no name at all
/// afterwards is `customer-computer`.
pub(crate) fn target_folder_name(hostname: &str) -> String {
    let cleaned: String = hostname
        .trim()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_end_matches('.').to_owned();
    if cleaned.chars().all(|c| matches!(c, '_' | '.')) || local_name(&cleaned).is_err() {
        "customer-computer".to_owned()
    } else {
        cleaned
    }
}

/// The folder a download goes to when the technician chose none:
/// `Downloads\CretLink\<target>`.
pub(crate) fn default_folder(downloads: &Path, hostname: &str) -> PathBuf {
    downloads
        .join("CretLink")
        .join(target_folder_name(hostname))
}

/// The path of the entry `name` in `folder` on this computer, when `name`
/// may be a name here ([`local_name`]). The result lies directly in `folder`.
pub(crate) fn beside(folder: &Path, name: &str) -> Result<PathBuf, &'static str> {
    let name = local_name(name)?;
    let path = folder.join(name);
    // One plain name was added: nothing else may have come of it.
    match path.strip_prefix(folder) {
        Ok(rest) if is_one_name(rest) => Ok(path),
        _ => Err("does not stay in its folder"),
    }
}

/// Whether `path` is exactly one plain name.
fn is_one_name(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Whether `path` lies inside `root`, through plain names only.
pub(crate) fn stays_inside(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rest) => {
            let mut parts = rest.components().peekable();
            parts.peek().is_some() && parts.all(|part| matches!(part, Component::Normal(_)))
        }
        Err(_) => false,
    }
}

/// The name of a second copy beside `name`: `report (2).pdf`, then
/// `report (3).pdf`, and so on, the first one that `taken` does not hold. A
/// folder has no extension to keep at the end. `None` when a thousand such
/// names are taken.
pub(crate) fn kept_beside(
    name: &str,
    is_folder: bool,
    taken: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if !is_folder && dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (2..=1000)
        .map(|n| format!("{stem} ({n}){extension}"))
        .find(|candidate| !taken(candidate))
}

/// One entry of a dropped folder that is not sent, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeftOut {
    /// Its path below the dropped folder, the folder's own name first.
    pub(crate) name: String,
    /// Why it is left out, as a sentence.
    pub(crate) why: &'static str,
}

/// What a dropped folder holds, as far as it was walked.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Walked {
    /// The folders below the dropped one, each by its names from the dropped
    /// folder down, a folder before what it holds.
    pub(crate) folders: Vec<Vec<String>>,
    /// The files, each by its names from the dropped folder down, with its
    /// length and its place on this computer.
    pub(crate) files: Vec<(Vec<String>, u64, PathBuf)>,
    /// What was left out.
    pub(crate) left_out: Vec<LeftOut>,
    /// Set when the walk stopped at a bound: what it says is why the folder
    /// was not walked to its end.
    pub(crate) stopped: Option<String>,
}

/// A link is not followed, in the words of a row of the queue.
pub(crate) const LINK_LEFT_OUT: &str = "A link is not followed.";

/// Walks the folder `root` on this computer: its folders and files, at most
/// `budget` of them in all and at most `max_depth` folders deep. `budget` is
/// lowered by what was taken, so several folders of one drop share it.
///
/// A link — a symbolic link or a junction — is named and never followed, so a
/// drop sends what lies in the folder and nothing a link leads to. An entry
/// that is neither a file nor a folder, one whose name is not text, and one
/// that cannot be read are named too. A walk that reaches a bound says so
/// ([`Walked::stopped`]): what it returns is then not the whole folder.
pub(crate) fn walk(root: &Path, budget: &mut usize, max_depth: usize) -> Walked {
    let mut walked = Walked::default();
    // Folders still to read: where they are, and their names from the top.
    let mut pending: Vec<(PathBuf, Vec<String>)> = vec![(root.to_path_buf(), Vec::new())];
    while let Some((folder, names)) = pending.pop() {
        let shown = |name: &str| {
            let mut parts = names.clone();
            parts.push(name.to_owned());
            parts.join("\\")
        };
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(_) => {
                walked.left_out.push(LeftOut {
                    name: if names.is_empty() {
                        ".".to_owned()
                    } else {
                        names.join("\\")
                    },
                    why: "This folder could not be read on this computer.",
                });
                continue;
            }
        };
        // In the order of the names, so that the same drop queues the same way.
        let mut listed: Vec<std::fs::DirEntry> = entries.filter_map(Result::ok).collect();
        listed.sort_by_key(std::fs::DirEntry::file_name);
        let mut below: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for entry in listed {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                walked.left_out.push(LeftOut {
                    name: shown(&entry.file_name().to_string_lossy()),
                    why: "Its name is not text that can be sent.",
                });
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                walked.left_out.push(LeftOut {
                    name: shown(&name),
                    why: "It could not be read on this computer.",
                });
                continue;
            };
            let kind = meta.file_type();
            if kind.is_symlink() {
                walked.left_out.push(LeftOut {
                    name: shown(&name),
                    why: LINK_LEFT_OUT,
                });
                continue;
            }
            if !kind.is_dir() && !kind.is_file() {
                walked.left_out.push(LeftOut {
                    name: shown(&name),
                    why: "It is neither a file nor a folder.",
                });
                continue;
            }
            if *budget == 0 {
                walked.stopped = Some(
                    "It holds more files and folders than one drop may queue; the rest was \
                     not queued."
                        .to_owned(),
                );
                return walked;
            }
            let mut path = names.clone();
            path.push(name);
            if kind.is_dir() {
                if path.len() > max_depth {
                    walked.stopped = Some(format!(
                        "Its folders go deeper than {max_depth} levels; what lies deeper was \
                         not queued."
                    ));
                    continue;
                }
                *budget -= 1;
                walked.folders.push(path.clone());
                below.push((entry.path(), path));
            } else {
                *budget -= 1;
                walked.files.push((path, meta.len(), entry.path()));
            }
        }
        // Depth first, in the order of the names: a folder's files follow it.
        pending.extend(below.into_iter().rev());
    }
    // A folder is listed before what it holds, whatever the order of the walk.
    walked.folders.sort();
    walked
}

#[cfg(test)]
mod tests;

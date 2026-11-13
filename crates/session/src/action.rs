//! Attribution and the customer-visible description of one remote action
//! (ST-M004-006, ST-M009-007).
//!
//! The console stamps every console→agent request with an [`Attribution`]: who
//! drives it ([`Origin`]) and, for an AI action, the caller's stated
//! [`Purpose`] (REQ-F-026). The agent turns each request it handles into a
//! [`crate::SessionEvent::RequestHandled`] carrying that attribution beside a
//! plain description of the **real** operation ([`describe_operation`]) and the
//! honest [`ActionOutcome`], so the attended customer surface can show what is
//! done on their computer, by whom, why the caller says it is done, and how it
//! ended. The purpose is a claim by the caller; it never replaces the real
//! operation, which is always shown beside it.

use cretlink_protocol::{
    CleanupTrigger, ErrorCode, KeyAction, Operation, Origin, PointerAction, PointerButton, Purpose,
    RemotePath, Shell,
};

/// Who drives one console→agent request and, for an AI action, why the caller
/// says it is done (ST-M004-006, REQ-F-017, REQ-F-026).
///
/// Attribution only: it grants no authority, and routing, consent, and
/// fail-closed behaviour are unchanged by it. A technician action carries no
/// purpose and is labelled by its [`Origin`] alone. `From<Origin>` keeps the
/// purpose-free call sites unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attribution {
    /// Which local actor drives the request.
    pub origin: Origin,
    /// The caller's stated purpose, for an AI action that supplied one.
    pub purpose: Option<Purpose>,
}

impl Attribution {
    /// An attribution with an explicit origin and optional stated purpose.
    #[must_use]
    pub fn new(origin: Origin, purpose: Option<Purpose>) -> Self {
        Self { origin, purpose }
    }
}

impl From<Origin> for Attribution {
    fn from(origin: Origin) -> Self {
        Self {
            origin,
            purpose: None,
        }
    }
}

/// How the agent answered one handled request, as the customer surface shows it
/// (REQ-N-008: never a fabricated success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The agent answered and the transfer the request began is still going:
    /// a file has started to leave the computer and a later report with the
    /// same id says how it ended (ST-M009-011).
    InProgress,
    /// The agent performed the operation and answered with a result.
    Succeeded,
    /// The agent refused or failed the operation with this error category. A
    /// folder removal that left something behind is one of these, with the
    /// reason of the first entry it could not remove (ST-M010-001).
    Failed {
        /// The machine-readable category the agent answered with.
        code: ErrorCode,
    },
    /// The caller ended the action before it was finished: a folder removal
    /// that the technician cancelled (ST-M010-001). What it had done until
    /// then stays done.
    Cancelled,
    /// The agent sent no answer (the channel failed first), so the outcome is
    /// not known.
    Unknown,
}

/// The longest description [`describe_operation`] produces, in characters. A
/// longer command or path is cut on a character boundary and visibly marked as
/// shortened (never silently); the full command of a started job still arrives
/// separately on [`crate::SessionEvent::CommandStarted`].
pub const MAX_ACTION_DESCRIPTION_CHARS: usize = 512;

/// Describes the real operation of one request in plain words for the attended
/// customer surface (REQ-F-017, REQ-F-026).
///
/// It names the actual command, path, key, or coordinate, so a stated purpose
/// can never stand in for what is done. Content the customer should not have
/// echoed into an activity list — typed text, clipboard text, stdin bytes, file
/// bytes — is described by its size only (REQ-S-018). The result is bounded by
/// [`MAX_ACTION_DESCRIPTION_CHARS`].
#[must_use]
pub fn describe_operation(op: &Operation) -> String {
    let text = match op {
        Operation::SystemInfo {} => "read system information".to_owned(),
        Operation::Exec { shell, command, .. } => return describe_exec(*shell, command),
        Operation::WriteStdin { job, data } => {
            format!("send {} bytes of input to job {job}", data.len())
        }
        Operation::ListJobs {} => "list running jobs".to_owned(),
        Operation::TermOpen { shell, .. } => {
            format!("open an interactive {} terminal", shell_name(*shell))
        }
        Operation::TermResize { job, cols, rows } => {
            format!("resize terminal job {job} to {cols}x{rows}")
        }
        Operation::TermClose { job } => format!("close terminal job {job}"),
        Operation::FsStat { path } => format!("look up {}", path_text(path)),
        Operation::FsList { path, .. } => format!("list the folder {}", path_text(path)),
        Operation::CreateDir { path } => format!("create the folder {}", path_text(path)),
        Operation::FsRead { path, offset, .. } => {
            if *offset == 0 {
                format!(
                    "read the file {} (a copy goes to the technician's computer)",
                    path_text(path)
                )
            } else {
                format!("continue reading the file {}", path_text(path))
            }
        }
        Operation::FsWrite { dest, data, .. } => {
            format!("write {} bytes to {}", data.len(), path_text(dest))
        }
        Operation::FsCommit { dest, len, .. } => {
            format!("save the file {} ({len} bytes)", path_text(dest))
        }
        Operation::FsAbort { dest } => {
            format!("discard the partial file {}", path_text(dest))
        }
        Operation::FsDigest { path, .. } => format!("check the contents of {}", path_text(path)),
        Operation::FsMove { from, to, .. } => {
            format!("move {} to {}", path_text(from), path_text(to))
        }
        Operation::FsDelete { path } => format!("delete the file {}", path_text(path)),
        Operation::RemoveDir { path } => format!("remove the folder {}", path_text(path)),
        Operation::FsDrives {} => "list the drives".to_owned(),
        Operation::RemoveTree {
            path, stop: false, ..
        } => {
            format!("delete the folder {} and everything in it", path_text(path))
        }
        Operation::RemoveTree {
            path, stop: true, ..
        } => {
            format!("stop deleting the folder {}", path_text(path))
        }
        Operation::ListStaged {} => "list the files CretLink staged".to_owned(),
        Operation::StagingCleanup { trigger } => format!(
            "remove files CretLink staged ({})",
            match trigger {
                CleanupTrigger::SessionEnd => "session-end files",
                CleanupTrigger::Shutdown => "session-end and shutdown files",
                CleanupTrigger::All => "all files",
            }
        ),
        Operation::DesktopCapture { display, .. } => {
            format!("take a screenshot of display {display}")
        }
        Operation::DesktopPointer {
            action,
            button,
            x,
            y,
        } => match action {
            PointerAction::Move => format!("move the pointer to ({x}, {y})"),
            PointerAction::Down => {
                format!("press the {} button at ({x}, {y})", button_name(*button))
            }
            PointerAction::Up => {
                format!("release the {} button at ({x}, {y})", button_name(*button))
            }
            PointerAction::Click => format!("{}-click at ({x}, {y})", button_name(*button)),
            PointerAction::DoubleClick => {
                format!("{}-double-click at ({x}, {y})", button_name(*button))
            }
        },
        Operation::DesktopScroll { x, y, dx, dy } => {
            format!("scroll by ({dx}, {dy}) at ({x}, {y})")
        }
        Operation::DesktopKey { action, key } => match action {
            KeyAction::Down => format!("hold down the {key} key"),
            KeyAction::Up => format!("release the {key} key"),
            KeyAction::Press => format!("press the {key} key"),
        },
        Operation::DesktopType { text } => {
            format!("type {} characters", text.chars().count())
        }
        Operation::DesktopHotkey { keys } => format!("press {}", keys.join("+")),
        Operation::DesktopWait { millis } => format!("wait {millis} ms"),
        Operation::ClipboardGet {} => "read the clipboard text".to_owned(),
        Operation::ClipboardSet { text } => format!(
            "replace the clipboard text ({} characters)",
            text.chars().count()
        ),
    };
    clamp(text)
}

/// Describes a started command the way [`describe_operation`] describes its
/// `Exec` request, bounded by [`MAX_ACTION_DESCRIPTION_CHARS`].
pub(crate) fn describe_exec(shell: Shell, command: &str) -> String {
    clamp(format!("run a command in {}: {command}", shell_name(shell)))
}

/// Describes a job cancellation for the customer surface.
#[must_use]
pub fn describe_cancel(job: cretlink_protocol::JobId) -> String {
    format!("stop job {job}")
}

/// Whether a request is an internal step of a file transfer (a continuing
/// chunk read, a chunk write, a resume digest, or an abort). The transfer engine
/// issues thousands of these for one file, so the agent does not report each
/// one as a separate customer-visible action. The steps that mark what a
/// transfer does to the customer computer are reported instead: an upload's
/// commit (the file appears) and a download's first read from offset zero (a
/// copy starts leaving the computer).
#[must_use]
pub fn is_transfer_step(op: &Operation) -> bool {
    match op {
        Operation::FsRead { offset, .. } => *offset != 0,
        Operation::FsWrite { .. } | Operation::FsDigest { .. } | Operation::FsAbort { .. } => true,
        _ => false,
    }
}

fn shell_name(shell: Shell) -> &'static str {
    match shell {
        Shell::PowerShell => "PowerShell",
        Shell::Cmd => "Command Prompt",
    }
}

fn button_name(button: PointerButton) -> &'static str {
    match button {
        PointerButton::Left => "left",
        PointerButton::Right => "right",
        PointerButton::Middle => "middle",
    }
}

fn path_text(path: &RemotePath) -> String {
    match path {
        RemotePath::Absolute { path } => path.clone(),
        RemotePath::Staging { name } => format!("{name} (CretLink staging folder)"),
    }
}

/// Bounds a description, cutting on a character boundary and marking the cut.
fn clamp(text: String) -> String {
    if text.chars().count() <= MAX_ACTION_DESCRIPTION_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_ACTION_DESCRIPTION_CHARS).collect();
    out.push_str(" [shortened]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cretlink_protocol::{B64Bytes, JobId};

    #[test]
    fn describes_the_real_operation_not_a_summary() {
        let op = Operation::FsDelete {
            path: RemotePath::Absolute {
                path: "C:\\Temp\\old.log".to_owned(),
            },
        };
        assert_eq!(describe_operation(&op), "delete the file C:\\Temp\\old.log");
        let op = Operation::DesktopHotkey {
            keys: vec!["ctrl".to_owned(), "shift".to_owned(), "esc".to_owned()],
        };
        assert_eq!(describe_operation(&op), "press ctrl+shift+esc");
    }

    /// The drive list and the removal of a folder say what they are, the
    /// removal with its folder and with the fact that everything in it goes
    /// too; the request that ends a removal says that it stops one
    /// (ST-M010-001).
    #[test]
    fn describes_the_explorer_operations() {
        assert_eq!(
            describe_operation(&Operation::FsDrives {}),
            "list the drives"
        );
        let folder = RemotePath::Absolute {
            path: "C:\\Users\\ann\\Downloads\\old-setup".to_owned(),
        };
        assert_eq!(
            describe_operation(&Operation::RemoveTree {
                path: folder.clone(),
                stop: false,
                folder_id: None,
            }),
            "delete the folder C:\\Users\\ann\\Downloads\\old-setup and everything in it"
        );
        assert_eq!(
            describe_operation(&Operation::RemoveTree {
                path: folder,
                stop: true,
                folder_id: None,
            }),
            "stop deleting the folder C:\\Users\\ann\\Downloads\\old-setup"
        );
        assert_eq!(
            describe_operation(&Operation::RemoveTree {
                path: RemotePath::Staging {
                    name: "unpacked".to_owned(),
                },
                stop: false,
                folder_id: None,
            }),
            "delete the folder unpacked (CretLink staging folder) and everything in it"
        );
        // Neither is a step of a transfer: both are reported as actions.
        assert!(!is_transfer_step(&Operation::FsDrives {}));
        assert!(!is_transfer_step(&Operation::RemoveTree {
            path: RemotePath::Staging {
                name: "unpacked".to_owned(),
            },
            stop: false,
            folder_id: None,
        }));
    }

    #[test]
    fn never_echoes_typed_clipboard_or_stdin_content() {
        let secret = "hunter2-password";
        for op in [
            Operation::DesktopType {
                text: secret.to_owned(),
            },
            Operation::ClipboardSet {
                text: secret.to_owned(),
            },
            Operation::WriteStdin {
                job: JobId(1),
                data: B64Bytes::new(secret.as_bytes().to_vec()),
            },
        ] {
            let text = describe_operation(&op);
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[test]
    fn long_commands_are_visibly_shortened() {
        let op = Operation::Exec {
            shell: Shell::Cmd,
            command: "x".repeat(10_000),
            working_dir: None,
            env: std::collections::BTreeMap::new(),
            timeout_ms: None,
        };
        let text = describe_operation(&op);
        assert!(text.ends_with(" [shortened]"));
        assert!(text.chars().count() <= MAX_ACTION_DESCRIPTION_CHARS + " [shortened]".len());
    }

    #[test]
    fn origin_converts_to_a_purpose_free_attribution() {
        let attribution = Attribution::from(Origin::Terminal);
        assert_eq!(attribution.origin, Origin::Terminal);
        assert_eq!(attribution.purpose, None);
    }
}

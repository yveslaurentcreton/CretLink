//! What a `cretlink` process does, decided before any window opens
//! (ST-M009-005, ADR-0018).
//!
//! The decision is a pure function of the arguments, the program's file name,
//! the operating system and whether this copy is the installed one, so it is
//! tested exhaustively. Its rules:
//!
//! - A copy whose file name starts with `CretLink-Support` (any case) is for
//!   getting help only: it opens the customer role, and refuses subcommands
//!   and the technician role, so a file handed to a customer cannot be used
//!   the other way. It accepts two subcommands, both the customer's own: the
//!   clean-up `recover [--remove]` (ST-M009-010, ST-M006-003), and
//!   `uninstall`, which removes the support program that a customer kept on
//!   the computer (ST-M009-017) and is what its entry in Installed apps
//!   runs. The technician's install and uninstall stay out of its reach
//!   (ST-M009-006), and nothing on a command line keeps a program: that is a
//!   choice in the customer's own window.
//! - `--role technician` and `--role customer` open that role directly; the
//!   application passes them to itself when it starts the chosen role in a new
//!   process. The customer role exists only on Windows. Get help on the start
//!   screen starts `--role customer --from-start`, unelevated, so the customer
//!   first sees what the UAC prompt is for (`#c-admin`, ST-M009-009); its
//!   **Continue** starts `--role customer` elevated, and that process never
//!   explains or relaunches again ([`customer_admin_step`]).
//! - Any other arguments are subcommands (`mcp`, `mcp install`, `version`, …),
//!   handled by the shared command line without a window.
//! - Without arguments, Windows shows the start screen (Get help or Give help).
//!   On macOS and Linux the installed copy opens the technician role directly
//!   (ST-M009-005); a run-once copy shows the start screen with Give help only,
//!   so it can still be installed (ST-M009-006).

/// The two roles; one per process, for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Give help: the technician console, its IPC endpoint and listener.
    Technician,
    /// Get help: the customer's attended, portable support agent.
    Customer,
}

impl Role {
    /// The `--role` value for this role.
    #[must_use]
    pub fn arg(self) -> &'static str {
        match self {
            Self::Technician => "technician",
            Self::Customer => "customer",
        }
    }
}

/// How a role's process was started (ST-M009-009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Without arguments: a `CretLink-Support*` copy, or the installed copy on
    /// macOS and Linux.
    Direct,
    /// From Get help on the start screen (`--role customer --from-start`):
    /// **Back** returns to the start screen.
    FromStart,
    /// Started by the application itself with `--role`: the elevated customer
    /// after the UAC prompt, or Give help from the start screen. It never
    /// relaunches again.
    Relaunched,
}

/// What this process does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Run a subcommand of the shared command line, without a window.
    Subcommand(Vec<String>),
    /// Show the start screen.
    Start,
    /// Open a role's window.
    Role {
        /// The role.
        role: Role,
        /// How the process was started.
        entry: Entry,
    },
    /// Refuse, with a message for the user.
    Refuse(String),
}

/// The argument the start screen adds after `--role customer`.
pub const FROM_START: &str = "--from-start";

/// What the customer role shows first (ST-M009-009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminStep {
    /// Explain the UAC prompt (`#c-admin`) before anything else.
    Explain,
    /// Go straight to choosing the technician (`#c-find`).
    NotNeeded,
}

/// Whether a customer process first explains the UAC prompt: only when it is
/// not elevated and was not itself started by the application with `--role`
/// — the elevated child, or a process whose relaunch was already attempted,
/// never asks again (REQ-F-002, REQ-S-011).
#[must_use]
pub fn customer_admin_step(entry: Entry, elevated: bool) -> AdminStep {
    if elevated || entry == Entry::Relaunched {
        AdminStep::NotNeeded
    } else {
        AdminStep::Explain
    }
}

/// Whether this customer process may start itself again elevated: never a
/// process the application already started with `--role`, so a relaunch never
/// relaunches again.
#[must_use]
pub fn may_relaunch(entry: Entry) -> bool {
    entry != Entry::Relaunched
}

/// Where **Back** on `#c-admin` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackTo {
    /// Start the start screen again, then close.
    StartScreen,
    /// Close this copy (a `CretLink-Support*` copy has no start screen).
    Close,
}

/// Back returns to the start screen only when the start screen opened this
/// process; a `CretLink-Support*` copy closes.
#[must_use]
pub fn back_target(entry: Entry) -> BackTo {
    match entry {
        Entry::FromStart => BackTo::StartScreen,
        Entry::Direct | Entry::Relaunched => BackTo::Close,
    }
}

/// The file-name prefix of a copy meant for the customer.
const CUSTOMER_PREFIX: &str = "cretlink-support";

/// Whether a program file name marks a customer copy.
#[must_use]
pub fn is_customer_copy(file_stem: &str) -> bool {
    file_stem.to_ascii_lowercase().starts_with(CUSTOMER_PREFIX)
}

/// Decides what this process does. `args` excludes the program name;
/// `customer_available` is whether the customer role exists here (Windows, or
/// a `dev-simulate` build); `installed` is whether this copy runs from the
/// install location (ST-M009-006).
#[must_use]
pub fn resolve(
    args: &[String],
    file_stem: &str,
    customer_available: bool,
    installed: bool,
) -> Launch {
    let customer_copy = is_customer_copy(file_stem);
    let no_customer = || {
        Launch::Refuse(
            "Get help runs only on Windows; on this computer CretLink can give help.".to_owned(),
        )
    };

    if args.first().map(String::as_str) == Some("--role") {
        let role = match args.get(1).map(String::as_str) {
            Some("technician") => Role::Technician,
            Some("customer") => Role::Customer,
            Some(other) => {
                return Launch::Refuse(format!(
                    "unknown role '{other}' (expected 'technician' or 'customer')"
                ));
            }
            None => return Launch::Refuse("--role expects 'technician' or 'customer'".to_owned()),
        };
        let from_start =
            role == Role::Customer && args.get(2).map(String::as_str) == Some(FROM_START);
        let expected = if from_start { 3 } else { 2 };
        if let Some(extra) = args.get(expected) {
            return Launch::Refuse(format!("unexpected argument '{extra}'"));
        }
        if role == Role::Customer && !customer_available {
            return no_customer();
        }
        if role == Role::Technician && customer_copy {
            return Launch::Refuse(customer_copy_message());
        }
        // A customer copy has no start screen to return to.
        let entry = if from_start && !customer_copy {
            Entry::FromStart
        } else if from_start {
            Entry::Direct
        } else {
            Entry::Relaunched
        };
        return Launch::Role { role, entry };
    }

    if !args.is_empty() {
        if customer_copy && !is_recover(args) && !is_remove_kept(args) {
            return Launch::Refuse(customer_copy_message());
        }
        return Launch::Subcommand(args.to_vec());
    }

    if customer_copy {
        if !customer_available {
            return no_customer();
        }
        return Launch::Role {
            role: Role::Customer,
            entry: Entry::Direct,
        };
    }
    if customer_available || !installed {
        Launch::Start
    } else {
        Launch::Role {
            role: Role::Technician,
            entry: Entry::Direct,
        }
    }
}

/// Whether the arguments are exactly `recover` or `recover --remove`: the only
/// subcommand a customer copy runs (ST-M009-010).
fn is_recover(args: &[String]) -> bool {
    match args {
        [verb] => verb == "recover",
        [verb, option] => verb == "recover" && option == "--remove",
        _ => false,
    }
}

/// Whether the arguments are exactly `uninstall`: in a customer copy, the
/// removal of the support program that a customer kept (ST-M009-017). It
/// shows what it removes and asks; no option skips that, and no argument
/// keeps a program.
fn is_remove_kept(args: &[String]) -> bool {
    matches!(args, [verb] if verb == "uninstall")
}

/// The program name the shared command line prints in its texts, such as the
/// recovery command it suggests: the file name of a customer copy, quoted when
/// it holds a space (`"CretLink-Support (1)"`), else `cretlink`.
#[must_use]
pub fn program_name(file_stem: &str) -> String {
    if !is_customer_copy(file_stem) {
        "cretlink".to_owned()
    } else if file_stem.contains(char::is_whitespace) {
        format!("\"{file_stem}\"")
    } else {
        file_stem.to_owned()
    }
}

fn customer_copy_message() -> String {
    "This copy of CretLink is for getting help. Start it without arguments; to list what \
     CretLink left on this computer, add 'recover' (and 'recover --remove' to remove it); to \
     remove the support program that was kept on this computer, add 'uninstall'; to give \
     help, use cretlink."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn role(role: Role, entry: Entry) -> Launch {
        Launch::Role { role, entry }
    }

    #[test]
    fn windows_without_arguments_shows_the_start_screen() {
        assert_eq!(resolve(&[], "cretlink", true, false), Launch::Start);
    }

    #[test]
    fn an_installed_copy_on_macos_and_linux_opens_the_technician_role() {
        assert_eq!(
            resolve(&[], "cretlink", false, true),
            role(Role::Technician, Entry::Direct)
        );
    }

    #[test]
    fn a_run_once_copy_on_macos_and_linux_can_still_install() {
        // The start screen then offers Give help only: install or run once.
        assert_eq!(resolve(&[], "cretlink", false, false), Launch::Start);
    }

    #[test]
    fn the_installed_copy_on_windows_still_offers_both_roles() {
        assert_eq!(resolve(&[], "cretlink", true, true), Launch::Start);
    }

    #[test]
    fn a_customer_copy_opens_the_customer_role() {
        for name in [
            "CretLink-Support",
            "cretlink-support",
            "CretLink-Support (1)",
        ] {
            assert_eq!(
                resolve(&[], name, true, false),
                role(Role::Customer, Entry::Direct),
                "{name}"
            );
        }
        assert!(matches!(
            resolve(&[], "CretLink-Support", false, false),
            Launch::Refuse(_)
        ));
    }

    #[test]
    fn a_customer_copy_refuses_subcommands_and_the_technician_role() {
        let copy = "CretLink-Support";
        assert!(matches!(
            resolve(&args(&["mcp"]), copy, true, false),
            Launch::Refuse(_)
        ));
        // It never installs, and never uninstalls without showing what goes
        // and asking (ST-M009-006, ST-M009-017), installed or not.
        for command in [
            args(&["install"]),
            args(&["uninstall", "--yes"]),
            args(&["uninstall", "-y"]),
            args(&["mcp", "install"]),
            args(&["mcp", "uninstall"]),
        ] {
            for installed in [false, true] {
                assert!(
                    matches!(resolve(&command, copy, true, installed), Launch::Refuse(_)),
                    "{command:?}"
                );
            }
        }
        assert!(matches!(
            resolve(&args(&["--role", "technician"]), copy, true, false),
            Launch::Refuse(_)
        ));
        assert_eq!(
            resolve(&args(&["--role", "customer"]), copy, true, false),
            role(Role::Customer, Entry::Relaunched)
        );
    }

    #[test]
    fn a_customer_copy_accepts_recover() {
        for name in ["CretLink-Support", "cretlink-support (1)"] {
            for command in [args(&["recover"]), args(&["recover", "--remove"])] {
                for installed in [false, true] {
                    assert_eq!(
                        resolve(&command, name, true, installed),
                        Launch::Subcommand(command.clone()),
                        "{name} {command:?}"
                    );
                }
            }
            // Only that verb, spelled exactly.
            for command in [
                args(&["recover", "--force"]),
                args(&["recover", "--remove", "--remove"]),
                args(&["--remove"]),
                args(&["version"]),
                args(&["shell"]),
            ] {
                assert!(
                    matches!(resolve(&command, name, true, false), Launch::Refuse(_)),
                    "{name} {command:?}"
                );
            }
        }
        // The ordinary program runs it like any subcommand.
        assert_eq!(
            resolve(&args(&["recover", "--remove"]), "cretlink", true, true),
            Launch::Subcommand(args(&["recover", "--remove"]))
        );
    }

    /// The kept support program is a customer copy by its name: it refuses
    /// every subcommand but its own two (ST-M009-017).
    #[test]
    fn a_customer_copy_accepts_its_own_uninstall_and_nothing_that_keeps_a_program() {
        for name in ["CretLink-Support", "cretlink-support (1)"] {
            assert_eq!(
                resolve(&args(&["uninstall"]), name, true, false),
                Launch::Subcommand(args(&["uninstall"])),
                "{name}"
            );
            // No command line keeps or installs anything, in any spelling.
            for command in [
                args(&["keep"]),
                args(&["--keep"]),
                args(&["install"]),
                args(&["install", "--yes"]),
                args(&["--install"]),
                args(&["update"]),
                args(&["uninstall", "--yes"]),
                args(&["uninstall", "uninstall"]),
                args(&["--role", "customer", "--keep"]),
                args(&["--role", "customer", "keep"]),
                args(&["--role", "customer", "--from-start", "--keep"]),
            ] {
                assert!(
                    matches!(resolve(&command, name, true, false), Launch::Refuse(_)),
                    "{name} {command:?}"
                );
            }
            // Every other subcommand of the shared command line stays refused.
            for command in [
                args(&["mcp"]),
                args(&["mcp", "status"]),
                args(&["shell"]),
                args(&["version"]),
                args(&["help"]),
                args(&["--help"]),
            ] {
                assert!(
                    matches!(resolve(&command, name, true, false), Launch::Refuse(_)),
                    "{name} {command:?}"
                );
            }
        }
        // The same words open no window that keeps a program in the ordinary
        // program either: there they are subcommands that the shared command
        // line does not know, or the technician's own uninstall.
        assert_eq!(
            resolve(&args(&["keep"]), "cretlink", true, false),
            Launch::Subcommand(args(&["keep"]))
        );
    }

    #[test]
    fn the_program_name_is_the_customer_copy_typed() {
        assert_eq!(program_name("cretlink"), "cretlink");
        assert_eq!(program_name("CretLink-Support"), "CretLink-Support");
        assert_eq!(
            program_name("CretLink-Support (1)"),
            "\"CretLink-Support (1)\""
        );
    }

    #[test]
    fn explicit_roles_mark_the_relaunch() {
        assert_eq!(
            resolve(&args(&["--role", "technician"]), "cretlink", true, false),
            role(Role::Technician, Entry::Relaunched)
        );
        assert_eq!(
            resolve(&args(&["--role", "customer"]), "cretlink", true, false),
            role(Role::Customer, Entry::Relaunched)
        );
    }

    #[test]
    fn the_customer_role_needs_windows() {
        assert!(matches!(
            resolve(&args(&["--role", "customer"]), "cretlink", false, false),
            Launch::Refuse(_)
        ));
    }

    #[test]
    fn malformed_roles_are_refused() {
        for bad in [
            args(&["--role"]),
            args(&["--role", "admin"]),
            args(&["--role", "technician", "mcp"]),
            args(&["--role", "technician", "--from-start"]),
            args(&["--role", "customer", "--from-start", "mcp"]),
            args(&["--role", "customer", "mcp"]),
        ] {
            assert!(
                matches!(resolve(&bad, "cretlink", true, false), Launch::Refuse(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn other_arguments_are_subcommands() {
        assert_eq!(
            resolve(&args(&["mcp", "install"]), "cretlink", true, false),
            Launch::Subcommand(args(&["mcp", "install"]))
        );
        assert_eq!(
            resolve(&args(&["version"]), "cretlink", false, true),
            Launch::Subcommand(args(&["version"]))
        );
        // The Windows uninstall entry runs `"<path>" uninstall`.
        assert_eq!(
            resolve(&args(&["uninstall"]), "cretlink", true, true),
            Launch::Subcommand(args(&["uninstall"]))
        );
    }

    #[test]
    fn get_help_from_the_start_screen_is_marked() {
        assert_eq!(
            resolve(
                &args(&["--role", "customer", "--from-start"]),
                "cretlink",
                true,
                false
            ),
            role(Role::Customer, Entry::FromStart)
        );
        // A customer copy has no start screen to go back to.
        assert_eq!(
            resolve(
                &args(&["--role", "customer", "--from-start"]),
                "CretLink-Support",
                true,
                false
            ),
            role(Role::Customer, Entry::Direct)
        );
        assert!(matches!(
            resolve(
                &args(&["--role", "customer", "--from-start"]),
                "cretlink",
                false,
                false
            ),
            Launch::Refuse(_)
        ));
    }

    #[test]
    fn an_unelevated_customer_first_explains_the_uac_prompt() {
        // Get help from the start screen, and a `CretLink-Support*` copy.
        for entry in [Entry::FromStart, Entry::Direct] {
            assert_eq!(customer_admin_step(entry, false), AdminStep::Explain);
            assert!(may_relaunch(entry));
        }
        // Already elevated (started as administrator): nothing to explain.
        for entry in [Entry::FromStart, Entry::Direct, Entry::Relaunched] {
            assert_eq!(customer_admin_step(entry, true), AdminStep::NotNeeded);
        }
    }

    #[test]
    fn a_relaunched_customer_never_explains_or_relaunches_again() {
        // The elevated child, and any process started with `--role`, opens
        // with its real identity — elevated or not (REQ-S-011).
        assert_eq!(
            customer_admin_step(Entry::Relaunched, false),
            AdminStep::NotNeeded
        );
        assert!(!may_relaunch(Entry::Relaunched));
    }

    #[test]
    fn back_returns_to_the_start_screen_only_from_the_start_screen() {
        assert_eq!(back_target(Entry::FromStart), BackTo::StartScreen);
        assert_eq!(back_target(Entry::Direct), BackTo::Close);
        assert_eq!(back_target(Entry::Relaunched), BackTo::Close);
    }
}

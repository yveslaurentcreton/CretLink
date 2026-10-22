//! What a `cretlink` process does, decided before any window opens
//! (ST-M009-005, ADR-0018).
//!
//! The decision is a pure function of the arguments, the program's file name
//! and the operating system, so it is tested exhaustively. Its rules:
//!
//! - A copy whose file name starts with `CretLink-Support` (any case) is for
//!   getting help only: it opens the customer role, and refuses subcommands and
//!   the technician role, so a file handed to a customer cannot be used the
//!   other way.
//! - `--role technician` and `--role customer` open that role directly; the
//!   application passes them to itself when it starts the chosen role in a new
//!   process. The customer role exists only on Windows.
//! - Any other arguments are subcommands (`mcp`, `mcp install`, `version`, …),
//!   handled by the shared command line without a window.
//! - Without arguments, Windows shows the start screen (Get help or Give help);
//!   macOS and Linux open the technician role.

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

/// What this process does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Run a subcommand of the shared command line, without a window.
    Subcommand(Vec<String>),
    /// Show the start screen.
    Start,
    /// Open a role's window. `relaunched` is set when the application started
    /// this process itself with `--role`, so it never relaunches again.
    Role {
        /// The role.
        role: Role,
        /// Whether the process was started with an explicit `--role`.
        relaunched: bool,
    },
    /// Refuse, with a message for the user.
    Refuse(String),
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
/// a `dev-simulate` build).
#[must_use]
pub fn resolve(args: &[String], file_stem: &str, customer_available: bool) -> Launch {
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
        if args.len() > 2 {
            return Launch::Refuse(format!("unexpected argument '{}'", args[2]));
        }
        if role == Role::Customer && !customer_available {
            return no_customer();
        }
        if role == Role::Technician && customer_copy {
            return Launch::Refuse(customer_copy_message());
        }
        return Launch::Role {
            role,
            relaunched: true,
        };
    }

    if !args.is_empty() {
        if customer_copy {
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
            relaunched: false,
        };
    }
    if customer_available {
        Launch::Start
    } else {
        Launch::Role {
            role: Role::Technician,
            relaunched: false,
        }
    }
}

fn customer_copy_message() -> String {
    "This copy of CretLink is for getting help. Start it without arguments; to give help, use \
     cretlink."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn role(role: Role, relaunched: bool) -> Launch {
        Launch::Role { role, relaunched }
    }

    #[test]
    fn windows_without_arguments_shows_the_start_screen() {
        assert_eq!(resolve(&[], "cretlink", true), Launch::Start);
    }

    #[test]
    fn macos_and_linux_open_the_technician_role() {
        assert_eq!(
            resolve(&[], "cretlink", false),
            role(Role::Technician, false)
        );
    }

    #[test]
    fn a_customer_copy_opens_the_customer_role() {
        for name in [
            "CretLink-Support",
            "cretlink-support",
            "CretLink-Support (1)",
        ] {
            assert_eq!(
                resolve(&[], name, true),
                role(Role::Customer, false),
                "{name}"
            );
        }
        assert!(matches!(
            resolve(&[], "CretLink-Support", false),
            Launch::Refuse(_)
        ));
    }

    #[test]
    fn a_customer_copy_refuses_subcommands_and_the_technician_role() {
        let copy = "CretLink-Support";
        assert!(matches!(
            resolve(&args(&["mcp"]), copy, true),
            Launch::Refuse(_)
        ));
        assert!(matches!(
            resolve(&args(&["--role", "technician"]), copy, true),
            Launch::Refuse(_)
        ));
        assert_eq!(
            resolve(&args(&["--role", "customer"]), copy, true),
            role(Role::Customer, true)
        );
    }

    #[test]
    fn explicit_roles_mark_the_relaunch() {
        assert_eq!(
            resolve(&args(&["--role", "technician"]), "cretlink", true),
            role(Role::Technician, true)
        );
        assert_eq!(
            resolve(&args(&["--role", "customer"]), "cretlink", true),
            role(Role::Customer, true)
        );
    }

    #[test]
    fn the_customer_role_needs_windows() {
        assert!(matches!(
            resolve(&args(&["--role", "customer"]), "cretlink", false),
            Launch::Refuse(_)
        ));
    }

    #[test]
    fn malformed_roles_are_refused() {
        for bad in [
            args(&["--role"]),
            args(&["--role", "admin"]),
            args(&["--role", "technician", "mcp"]),
        ] {
            assert!(
                matches!(resolve(&bad, "cretlink", true), Launch::Refuse(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn other_arguments_are_subcommands() {
        assert_eq!(
            resolve(&args(&["mcp", "install"]), "cretlink", true),
            Launch::Subcommand(args(&["mcp", "install"]))
        );
        assert_eq!(
            resolve(&args(&["version"]), "cretlink", false),
            Launch::Subcommand(args(&["version"]))
        );
    }
}

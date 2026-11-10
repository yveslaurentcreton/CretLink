//! The name of this computer's operating system, as the foot of the
//! technician's navigation shows it next to the host name (ST-M009-008): the
//! product name and its release, without a build number and without the
//! processor architecture — `Windows 11 Pro 25H2`, `macOS 15.1`,
//! `Debian GNU/Linux 12 (bookworm)`.
//!
//! Each system's own record is read and no program is run. What cannot be
//! read is not guessed: the bare family name is shown instead (REQ-N-008).
//!
//! - Windows: the registry facts the customer side reports too, through
//!   `cretlink_support_agent::sysinfo::os_name`; else `Windows`.
//! - macOS: `ProductVersion` of the system's `SystemVersion.plist`; else
//!   `macOS`.
//! - Linux: `PRETTY_NAME` of os-release(5); else `Linux`.
//!
//! The choice between the systems is a `cfg!` value, not a `#[cfg]` item, so
//! every branch and both parsers compile and are tested on every platform.
//! Only the registry read behind `sysinfo::os_name` is Windows-only code.

use cretlink_support_agent::sysinfo;

/// macOS keeps its version in this small XML property list.
const MACOS_SYSTEM_VERSION: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// os-release(5): `/etc/os-release` is the system's file and
/// `/usr/lib/os-release` the vendor's copy, read only when the first cannot
/// be read.
const LINUX_OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];

/// The characters an os-release value escapes with a backslash.
const OS_RELEASE_ESCAPED: [char; 5] = ['\\', '$', '"', '\'', '`'];

/// This computer's operating system by name and release.
pub(super) fn name() -> String {
    if cfg!(windows) {
        sysinfo::os_name()
    } else if cfg!(target_os = "macos") {
        let system_version = std::fs::read_to_string(MACOS_SYSTEM_VERSION).ok();
        macos_name(system_version.as_deref())
    } else if cfg!(target_os = "linux") {
        let os_release = LINUX_OS_RELEASE
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok());
        linux_name(os_release.as_deref())
    } else {
        std::env::consts::OS.to_owned()
    }
}

/// `macOS` and the product version of a `SystemVersion.plist`, or just
/// `macOS` without a readable version.
fn macos_name(system_version: Option<&str>) -> String {
    match system_version.and_then(product_version) {
        Some(version) => format!("macOS {version}"),
        None => "macOS".to_owned(),
    }
}

/// The string under the `ProductVersion` key of an XML property list, when it
/// is a dotted number such as `15.1` or `14.6.1`. Anything else — another
/// value type, a binary property list, an unfinished file — is no version.
fn product_version(plist: &str) -> Option<&str> {
    let (_, after_key) = plist.split_once("<key>ProductVersion</key>")?;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let (version, _) = value.split_once("</string>")?;
    let version = version.trim();
    let dotted_number = version.len() <= 16
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    dotted_number.then_some(version)
}

/// The `PRETTY_NAME` of an os-release file, or just `Linux` without one.
fn linux_name(os_release: Option<&str>) -> String {
    os_release
        .and_then(pretty_name)
        .unwrap_or_else(|| "Linux".to_owned())
}

/// The `PRETTY_NAME` of an os-release file, when it has one that is not
/// empty. The file is a list of `KEY=value` lines; as in the shell that the
/// format follows, the last assignment is the one that counts.
fn pretty_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("PRETTY_NAME="))?;
    let name = unquote(value);
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// An os-release value as it was meant: without the one pair of single or
/// double quotes around it, and without the backslash in front of an escaped
/// character.
fn unquote(value: &str) -> String {
    let inner = ['"', '\'']
        .into_iter()
        .find_map(|quote| value.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(value);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        // A backslash in front of an escaped character stands for that
        // character; anywhere else it is an ordinary character.
        match chars.next_if(|next| c == '\\' && OS_RELEASE_ESCAPED.contains(next)) {
            Some(escaped) => out.push(escaped),
            None => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SystemVersion.plist` as macOS writes it, shortened to the keys
    /// around the one that is read.
    const SYSTEM_VERSION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>ProductBuildVersion</key>
	<string>24B83</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductUserVisibleVersion</key>
	<string>15.1</string>
	<key>ProductVersion</key>
	<string>15.1</string>
	<key>iOSSupportVersion</key>
	<string>18.1</string>
</dict>
</plist>
"#;

    /// An `/etc/os-release` as Debian ships it.
    const OS_RELEASE: &str = r#"PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
VERSION="12 (bookworm)"
VERSION_CODENAME=bookworm
ID=debian
"#;

    #[test]
    fn macos_is_named_with_its_product_version() {
        assert_eq!(macos_name(Some(SYSTEM_VERSION)), "macOS 15.1");
        // A patch release has three numbers, and neither the layout of the
        // file nor the key's place in it matters.
        assert_eq!(
            macos_name(Some(
                "<dict><key>ProductVersion</key>\n\t<string> 14.6.1 </string></dict>"
            )),
            "macOS 14.6.1"
        );
    }

    #[test]
    fn macos_without_a_readable_version_is_just_macos() {
        assert_eq!(macos_name(None), "macOS");
        for unreadable in [
            "",
            // A binary property list.
            "bplist00\u{0}\u{1}",
            // No such key.
            "<dict><key>ProductName</key><string>macOS</string></dict>",
            // The key's value is not a string: the next string in the file
            // belongs to another key.
            "<key>ProductVersion</key><integer>15</integer><key>Other</key><string>1.2</string>",
            // An unfinished file.
            "<key>ProductVersion</key><string>15.1",
            // Not a dotted number.
            "<key>ProductVersion</key><string></string>",
            "<key>ProductVersion</key><string>15.</string>",
            "<key>ProductVersion</key><string>15.1 beta</string>",
            "<key>ProductVersion</key><string>11111111111111111</string>",
        ] {
            assert_eq!(macos_name(Some(unreadable)), "macOS", "{unreadable:?}");
        }
    }

    #[test]
    fn linux_is_named_by_the_pretty_name_of_os_release() {
        assert_eq!(
            linux_name(Some(OS_RELEASE)),
            "Debian GNU/Linux 12 (bookworm)"
        );
        // Anywhere in the file, in single quotes, after comments and blank
        // lines, with Windows line ends.
        assert_eq!(
            linux_name(Some(
                "# a comment\r\n\r\nID=fedora\r\nPRETTY_NAME='Fedora Linux 40 (Workstation Edition)'\r\n"
            )),
            "Fedora Linux 40 (Workstation Edition)"
        );
        // Without quotes.
        assert_eq!(linux_name(Some("PRETTY_NAME=Sample\n")), "Sample");
        // The last assignment is the one that counts.
        assert_eq!(
            linux_name(Some("PRETTY_NAME=First\nPRETTY_NAME=\"Second\"\n")),
            "Second"
        );
    }

    #[test]
    fn an_os_release_value_loses_its_quotes_and_escapes() {
        assert_eq!(
            unquote(r#""Sample \"OS\" \$1 \\ \`2\` it\'s""#),
            r#"Sample "OS" $1 \ `2` it's"#
        );
        // An escaped backslash does not escape what follows it.
        assert_eq!(unquote(r"\\$"), r"\$");
        // A backslash in front of anything else stays, and so does a quote
        // without its partner.
        assert_eq!(unquote(r"a\b\"), r"a\b\");
        assert_eq!(unquote("\"Sample"), "\"Sample");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("''"), "");
    }

    #[test]
    fn linux_without_a_readable_name_is_just_linux() {
        assert_eq!(linux_name(None), "Linux");
        for unnamed in [
            "",
            "NAME=\"Debian GNU/Linux\"\nID=debian\n",
            "PRETTY_NAME=\n",
            "PRETTY_NAME=\"\"\n",
            "PRETTY_NAME=\"  \"\n",
            // A commented line, and another key that ends the same way.
            "#PRETTY_NAME=\"Old\"\nMY_PRETTY_NAME=\"Other\"\n",
            // An empty last assignment leaves no name.
            "PRETTY_NAME=First\nPRETTY_NAME=\n",
        ] {
            assert_eq!(linux_name(Some(unnamed)), "Linux", "{unnamed:?}");
        }
    }

    #[test]
    fn this_computer_is_named_not_tagged() {
        let name = name();
        assert!(!name.trim().is_empty());
        assert!(!name.contains('\n'), "{name:?}");
        // Never the compile-time tag with the architecture, as in
        // `windows (x86_64)`.
        assert_ne!(
            name,
            format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
        );
    }
}

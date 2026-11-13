//! Tests of the relay profile (ST-M007-011): every setting, the bounds, what
//! is refused, and that no message repeats what the file held.

use super::*;

/// A key as `cretlink-relay-server keygen` prints one.
const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";

fn key_text(profile: &RelayProfile) -> Option<String> {
    profile
        .key()
        .map(|key| String::from_utf8_lossy(key.as_bytes()).into_owned())
}

fn profile(text: &str) -> RelayProfile {
    parse(text)
        .unwrap_or_else(|problem| panic!("expected a profile, got: {problem}"))
        .expect("the text has a [relay] section")
}

fn problem(text: &str) -> Problem {
    match parse(text) {
        Err(problem) => problem,
        Ok(found) => panic!("expected a problem, got {found:?}"),
    }
}

/// A folder of its own for one test, removed afterwards.
struct Folder(PathBuf);

impl Folder {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cretlink-relay-profile-{test}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_profile_with_every_setting() {
    let text = format!(
        "# The relay of Sam\n\
         [relay]\n\
         address = \"relay.example.org:443\"\n\
         key = \"{KEY}\"\n\
         ca = \"relay-ca.pem\"\n\
         name = \"host-01.example.org\"\n"
    );
    let profile = profile(&text);
    assert_eq!(profile.address().host(), "relay.example.org");
    assert_eq!(profile.address().port(), 443);
    assert_eq!(profile.address().to_string(), "relay.example.org:443");
    assert_eq!(key_text(&profile).as_deref(), Some(KEY));
    assert_eq!(profile.ca(), Some("relay-ca.pem"));
    assert_eq!(profile.name(), Some("host-01.example.org"));
    // The certificate is checked against `name` when there is one.
    assert_eq!(profile.server_name(), "host-01.example.org");
}

#[test]
fn only_the_address_is_needed() {
    let profile = profile("[relay]\naddress = \"relay.example.org:9970\"\n");
    assert_eq!(profile.address().to_string(), "relay.example.org:9970");
    assert!(profile.key().is_none());
    assert_eq!(profile.ca(), None);
    assert_eq!(profile.name(), None);
    // Without `name` the certificate carries the host of the address.
    assert_eq!(profile.server_name(), "relay.example.org");
}

#[test]
fn a_file_without_a_relay_section_holds_no_profile() {
    assert!(parse("").unwrap().is_none());
    assert!(parse("# nothing yet\n\n").unwrap().is_none());
    assert!(parse("\u{feff}\r\n").unwrap().is_none());
}

#[test]
fn the_file_is_read_as_editors_write_it() {
    // A byte-order mark, Windows line ends, tabs, comments after a value and
    // text between single quotes.
    let text = "\u{feff}[ relay ]  # the relay\r\n\
                \taddress\t=\t'relay.example.org:443'   # where\r\n\
                \r\n\
                name = \"relay.example.org\"\r\n";
    let profile = profile(text);
    assert_eq!(profile.address().to_string(), "relay.example.org:443");
    assert_eq!(profile.name(), Some("relay.example.org"));
}

#[test]
fn text_in_double_quotes_takes_the_escapes_of_toml() {
    // A key may hold a quote and a backslash; both are written with a
    // backslash, or as a code point.
    let escaped = profile(
        "[relay]\naddress = \"relay.example.org:443\"\nkey = \"a\\\"b\\\\c\\u0021\\U0000007Ed\"\n",
    );
    assert_eq!(key_text(&escaped).as_deref(), Some("a\"b\\c!~d"));
    // Between single quotes nothing is an escape.
    let literal = profile("[relay]\naddress = 'relay.example.org:443'\nkey = 'a\\b'\n");
    assert_eq!(key_text(&literal).as_deref(), Some("a\\b"));
}

#[test]
fn the_address_is_a_host_and_a_port() {
    let address = |text: &str| RelayAddress::parse(text).map(|address| address.to_string());
    assert_eq!(
        address("relay.example.org:443").unwrap(),
        "relay.example.org:443"
    );
    // Two copies agree on one spelling: lower case, no trailing dot.
    assert_eq!(
        address(" Relay.Example.ORG.:443 ").unwrap(),
        "relay.example.org:443"
    );
    assert_eq!(address("192.0.2.1:9970").unwrap(), "192.0.2.1:9970");
    assert_eq!(address("[2001:db8::1]:443").unwrap(), "[2001:db8::1]:443");
    assert_eq!(
        address("relay_1.example.org:65535").unwrap(),
        "relay_1.example.org:65535"
    );

    for (text, says) in [
        ("", "is empty"),
        ("relay.example.org", "needs a port"),
        ("relay.example.org:", "from 1 to 65535"),
        ("relay.example.org:0", "from 1 to 65535"),
        ("relay.example.org:65536", "from 1 to 65535"),
        ("relay.example.org:+443", "from 1 to 65535"),
        ("relay.example.org: 443", "from 1 to 65535"),
        ("relay.example.org:https", "from 1 to 65535"),
        (":443", "not a host name"),
        ("relay example.org:443", "not a host name"),
        ("relay..example.org:443", "not a host name"),
        // An address in another spelling than the usual one is no name.
        ("192.168.001.001:443", "not a host name"),
        ("2001:db8::1:443", "square brackets"),
        ("[2001:db8::1]", "needs a port"),
        ("[relay.example.org]:443", "square brackets"),
        // CretLink always speaks TLS; a scheme has nothing to choose.
        ("wss://relay.example.org:443", "without wss://"),
        ("ws://relay.example.org:443", "without wss://"),
        ("relay.example.org:443/relay", "without a path"),
    ] {
        let problem = RelayAddress::parse(text).unwrap_err();
        assert_eq!(problem.field, Some(Field::Address), "{text:?}");
        assert!(problem.text.contains(says), "{text:?}: {}", problem.text);
    }
    let long = format!("{}.example.org:443", "a".repeat(300));
    assert!(
        RelayAddress::parse(&long)
            .unwrap_err()
            .text
            .contains("too long")
    );
}

#[test]
fn a_key_is_plain_text_without_a_blank_and_bounded() {
    let with_key =
        |key: &str| RelayProfile::from_fields("relay.example.org:443", Some(key), None, None);
    assert!(with_key("k").is_ok());
    assert!(with_key(&"k".repeat(MAX_KEY_BYTES)).is_ok());
    for (key, says) in [
        (String::new(), "is empty"),
        ("k".repeat(MAX_KEY_BYTES + 1), "longer than 512"),
        ("two words".to_owned(), "a blank"),
        ("tab\there".to_owned(), "a blank"),
        ("line\nbreak".to_owned(), "a blank"),
        ("clé-secrète".to_owned(), "not plain text"),
    ] {
        let problem = with_key(&key).unwrap_err();
        assert_eq!(problem.field, Some(Field::Key));
        assert!(problem.text.contains(says), "{}", problem.text);
        // Never the key itself.
        if key.len() > 3 {
            assert!(!problem.text.contains(&key));
        }
    }
}

#[test]
fn the_certificate_file_is_a_plain_name_next_to_the_profile() {
    let with_ca =
        |ca: &str| RelayProfile::from_fields("relay.example.org:443", None, Some(ca), None);
    assert_eq!(with_ca("relay-ca.pem").unwrap().ca(), Some("relay-ca.pem"));
    assert_eq!(
        with_ca("Our relay (2026).crt").unwrap().ca(),
        Some("Our relay (2026).crt")
    );
    for ca in [
        "",
        ".",
        "..",
        "../relay-ca.pem",
        "certs/relay-ca.pem",
        "certs\\relay-ca.pem",
        "/etc/ssl/relay-ca.pem",
        "C:relay-ca.pem",
        " relay-ca.pem",
        "relay-ca.pem ",
        "relay\u{0}ca.pem",
    ] {
        let problem = with_ca(ca).unwrap_err();
        assert_eq!(problem.field, Some(Field::Ca), "{ca:?}");
    }
    let long = format!("{}.pem", "a".repeat(MAX_CA_NAME_CHARS));
    assert_eq!(with_ca(&long).unwrap_err().field, Some(Field::Ca));
}

#[test]
fn the_name_in_the_certificate_is_a_host_name() {
    let with_name = |name: &str| RelayProfile::from_fields("192.0.2.1:443", None, None, Some(name));
    let profile = with_name("Relay.Example.org").unwrap();
    assert_eq!(profile.name(), Some("relay.example.org"));
    assert_eq!(profile.server_name(), "relay.example.org");
    // Where the relay is reached stays the address.
    assert_eq!(profile.address().host(), "192.0.2.1");
    for name in [
        "",
        "  ",
        "relay example",
        "https://relay.example.org",
        "relay.example.org:443",
    ] {
        assert_eq!(
            with_name(name).unwrap_err().field,
            Some(Field::Name),
            "{name:?}"
        );
    }
}

#[test]
fn an_unknown_setting_or_section_is_an_error() {
    let unknown = problem("[relay]\naddress = \"relay.example.org:443\"\ntoken = \"x\"\n");
    assert_eq!(unknown.line, Some(3));
    assert!(
        unknown.text.contains("not a setting of a relay"),
        "{}",
        unknown.text
    );
    assert!(unknown.text.contains("`address`, `key`, `ca` and `name`"));

    let section = problem("[relay]\naddress = \"relay.example.org:443\"\n\n[proxy]\n");
    assert_eq!(section.line, Some(4));
    assert!(
        section.text.contains("one section, [relay]"),
        "{}",
        section.text
    );

    // Upper case is another name.
    assert_eq!(
        problem("[Relay]\naddress = \"relay.example.org:443\"\n").line,
        Some(1)
    );
    assert_eq!(
        problem("[relay]\nAddress = \"relay.example.org:443\"\n").line,
        Some(2)
    );
}

#[test]
fn a_profile_without_an_address_is_an_error() {
    let missing = problem("# our relay\n[relay]\nkey = \"abc\"\n");
    assert_eq!(missing.line, Some(2));
    assert_eq!(missing.field, Some(Field::Address));
    assert!(missing.text.contains("no `address`"), "{}", missing.text);
}

#[test]
fn a_problem_with_how_a_setting_is_written_names_that_setting() {
    let relay = "[relay]\naddress = \"relay.example.org:443\"\n";
    for (line, field) in [
        ("key = abc", Field::Key),
        ("key = \"abc", Field::Key),
        ("ca = 'relay-ca.pem' x", Field::Ca),
        ("name = \"a\\qb\"", Field::Name),
        ("address = \"host-01.example.org:443\"", Field::Address),
    ] {
        let found = problem(&format!("{relay}{line}\n"));
        assert_eq!((found.line, found.field), (Some(3), Some(field)), "{line}");
        assert!(
            found.text.contains(&format!("`{}`", field.name())),
            "{}",
            found.text
        );
    }
    // A setting that a relay does not have is not named.
    let found = problem(&format!("{relay}token = abc\n"));
    assert_eq!(found.field, None);
    assert!(found.text.contains("this setting"), "{}", found.text);
}

#[test]
fn a_problem_names_the_line_of_its_setting() {
    let bad = problem("[relay]\n\naddress = \"relay.example.org\"\n");
    assert_eq!(bad.line, Some(3));
    assert_eq!(bad.field, Some(Field::Address));
    assert_eq!(bad.to_string(), format!("line 3: {}", bad.text));

    let bad = problem("[relay]\naddress = \"relay.example.org:443\"\nca = \"../x.pem\"\n");
    assert_eq!((bad.line, bad.field), (Some(3), Some(Field::Ca)));
}

#[test]
fn what_is_not_the_small_form_is_refused_with_its_line() {
    let relay = "[relay]\naddress = \"relay.example.org:443\"\n";
    for (line, says) in [
        (
            "address = \"relay.example.org:443\"",
            "already set on line 2",
        ),
        ("[relay]", "already on line 1"),
        ("key = abc", "must be text in double quotes"),
        ("key = 12", "must be text in double quotes"),
        ("key = true", "must be text in double quotes"),
        ("key = [\"a\"]", "must be text in double quotes"),
        ("key = { a = \"b\" }", "must be text in double quotes"),
        ("key =", "must be text in double quotes"),
        ("key = \"abc", "not closed with a double quote"),
        ("key = 'abc", "not closed with a quote"),
        ("key = \"abc\" def", "follows the closing quote"),
        ("key = \"\"\"abc\"\"\"", "on one line"),
        ("key = '''abc'''", "on one line"),
        ("key = \"a\\qb\"", "starts no escape"),
        ("key = \"a\\u12\"", "starts no escape"),
        ("key = \"a\\uD800b\"", "starts no escape"),
        ("key = \"a\\", "starts no escape"),
        ("key: \"abc\"", "not a setting"),
        ("key", "not a setting"),
        ("relay.key = \"abc\"", "not a setting"),
        ("\"key\" = \"abc\"", "not a setting"),
        ("= \"abc\"", "not a setting"),
        ("[[relay]]", "one name in square brackets"),
        ("[relay", "one name in square brackets"),
        ("[relay] x", "one name in square brackets"),
        ("[a.b]", "one name in square brackets"),
        ("[]", "one name in square brackets"),
        ("key = \"a\u{7}b\"", "not text"),
    ] {
        let found = problem(&format!("{relay}{line}\n"));
        assert_eq!(found.line, Some(3), "{line:?}");
        assert!(found.text.contains(says), "{line:?}: {}", found.text);
    }

    let before = problem("address = \"relay.example.org:443\"\n");
    assert_eq!(before.line, Some(1));
    assert!(before.text.contains("before a section"), "{}", before.text);
}

#[test]
fn the_file_is_bounded_in_every_direction() {
    let relay = "[relay]\naddress = \"relay.example.org:443\"\n";
    let long = format!(
        "{relay}name = \"{}\"\n",
        "a".repeat(document::MAX_VALUE_BYTES + 1)
    );
    assert!(problem(&long).text.contains("too long"));
    let long = format!(
        "{relay}name = '{}'\n",
        "a".repeat(document::MAX_VALUE_BYTES + 1)
    );
    assert!(problem(&long).text.contains("too long"));

    let many: String = (0..20).map(|n| format!("[s{n}]\n")).collect();
    assert!(problem(&many).text.contains("too many sections"));
    let many: String = (0..20).map(|n| format!("k{n} = \"v\"\n")).collect();
    assert!(
        problem(&format!("[relay]\n{many}"))
            .text
            .contains("too many settings")
    );
    let name = format!("[relay]\n{} = \"v\"\n", "k".repeat(40));
    assert!(problem(&name).text.contains("not a setting"));
}

#[test]
fn no_message_repeats_what_the_file_held() {
    // A person may type the key in the wrong place. Whatever is wrong with
    // the file, the key is not repeated in the message that is shown.
    let secret = "S3cr3tAdmissionKeyOfAcme";
    let files = [
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = {secret}\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{secret}\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{secret}\" \"{secret}\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey: {secret}\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\n{secret} = \"x\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\n{secret}\n"),
        format!(
            "[relay]\naddress = \"relay.example.org:443\"\n{secret} = \"a\"\n{secret} = \"b\"\n"
        ),
        format!("[relay]\naddress = \"{secret}\"\n"),
        format!("[relay]\naddress = \"{secret}:443:1\"\n"),
        format!("[relay]\naddress = \"relay.example.org:{secret}\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nca = \"../{secret}\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nname = \"{secret} {secret}\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{secret} {secret}\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{secret}\\q\"\n"),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"\"\"{secret}\"\"\"\n"),
        format!("[{secret}]\n"),
        format!("[{secret}\n"),
        format!("[relay]\n[{secret}]\n"),
        format!("{secret} = \"x\"\n"),
        format!("[relay]\nkey = \"{secret}\"\n"),
    ];
    for file in &files {
        let problem = problem(file);
        let shown = format!("{problem} {problem:?}");
        assert!(
            !shown.contains(secret),
            "the message repeats the file:\n{file}\n{shown}"
        );
    }
}

#[test]
fn debug_output_hides_the_key() {
    let profile = profile(&format!(
        "[relay]\naddress = \"relay.example.org:443\"\nkey = \"{KEY}\"\n"
    ));
    let shown = format!("{profile:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("has_key: true"), "{shown}");
    assert!(shown.contains("relay.example.org"), "{shown}");
}

#[test]
fn a_written_profile_is_read_back_the_same() {
    for (address, key, ca, name) in [
        ("relay.example.org:443", None, None, None),
        (
            "relay.example.org:443",
            Some(KEY),
            Some("relay-ca.pem"),
            Some("host-01.example.org"),
        ),
        (
            "[2001:db8::1]:9970",
            Some("a\"b\\c#d'e"),
            Some("o'brien # ca.pem"),
            None,
        ),
        ("192.0.2.1:443", None, None, Some("relay.example.org")),
    ] {
        let written = RelayProfile::from_fields(address, key, ca, name).unwrap();
        let text = written.to_section();
        let read = profile(&text);
        assert_eq!(read.address(), written.address(), "{address}");
        assert_eq!(key_text(&read).as_deref(), key);
        assert_eq!(read.ca(), ca);
        assert_eq!(read.name(), name);
    }
    assert!(
        RelayProfile::from_fields("relay.example.org:443", None, None, None)
            .unwrap()
            .to_section()
            .starts_with("[relay]\naddress = \"relay.example.org:443\"\n")
    );
}

#[test]
fn the_key_and_the_certificate_file_can_be_replaced() {
    let base =
        RelayProfile::from_fields("relay.example.org:443", Some("first-key"), None, None).unwrap();
    let kept = base.clone().with_key(None);
    assert!(kept.key().is_none());
    let other = base
        .clone()
        .with_key(Some(AdmissionToken::from_text("second-key")));
    assert_eq!(key_text(&other).as_deref(), Some("second-key"));
    assert_eq!(
        base.clone().with_ca(Some("relay-ca.pem")).unwrap().ca(),
        Some("relay-ca.pem")
    );
    assert_eq!(
        base.with_ca(Some("a/b.pem")).unwrap_err().field,
        Some(Field::Ca)
    );
}

// --- Files ---

#[test]
fn the_profile_is_looked_for_next_to_the_program() {
    let folder = Folder::new("next-to");
    let exe = folder.0.join("CretLink-Support.exe");
    assert_eq!(path_next_to(&exe), Some(folder.0.join(PROFILE_FILE)));
    // No file: no profile, and no error.
    assert!(read_next_to(&exe).unwrap().is_none());

    let file = folder.write(
        PROFILE_FILE,
        b"[relay]\naddress = \"relay.example.org:443\"\n",
    );
    let (path, profile) = read_next_to(&exe)
        .unwrap()
        .expect("the profile next to the program");
    assert_eq!(path, file);
    assert_eq!(profile.address().to_string(), "relay.example.org:443");

    // A file without a relay is no profile either.
    folder.write(PROFILE_FILE, b"# no relay here\n");
    assert!(read_next_to(&exe).unwrap().is_none());
    // A program without a folder has nothing next to it.
    assert_eq!(path_next_to(Path::new("")), None);
}

#[test]
fn a_file_that_cannot_be_used_names_itself_and_the_problem() {
    let folder = Folder::new("malformed");
    let file = folder.write(PROFILE_FILE, b"[relay]\naddress = \"relay.example.org\"\n");
    let error = read_file(&file).unwrap_err();
    assert_eq!(error.file, file);
    assert_eq!(error.problem.line, Some(2));
    assert_eq!(error.problem.field, Some(Field::Address));
    let shown = error.to_string();
    assert!(shown.contains(&file.display().to_string()), "{shown}");
    assert!(shown.contains("line 2: `address` needs a port"), "{shown}");

    // Not text.
    let file = folder.write("binary.toml", &[0xff, 0xfe, 0x00, 0x41]);
    assert!(
        read_file(&file)
            .unwrap_err()
            .problem
            .text
            .contains("not text")
    );

    // Larger than a profile may be: refused before it is read as text.
    let large = vec![b'#'; usize::try_from(MAX_PROFILE_BYTES).unwrap() + 1];
    let file = folder.write("large.toml", &large);
    assert!(
        read_file(&file)
            .unwrap_err()
            .problem
            .text
            .contains("larger than 16 KiB")
    );
    let fits = vec![b'#'; usize::try_from(MAX_PROFILE_BYTES).unwrap()];
    let file = folder.write("fits.toml", &fits);
    assert!(read_file(&file).unwrap().is_none());

    // A folder of that name is no file.
    let dir = folder.0.join("folder.toml");
    fs::create_dir(&dir).unwrap();
    assert!(
        read_file(&dir)
            .unwrap_err()
            .problem
            .text
            .contains("not a file")
    );
}

#[test]
fn the_certificate_file_is_read_from_the_folder_of_the_profile() {
    let folder = Folder::new("ca");
    let pem = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    folder.write("relay-ca.pem", pem);
    let file = folder.write(
        PROFILE_FILE,
        b"[relay]\naddress = \"relay.example.org:443\"\nca = \"relay-ca.pem\"\n",
    );
    let profile = read_file(&file).unwrap().unwrap();
    assert_eq!(read_ca(&file, &profile).unwrap().as_deref(), Some(&pem[..]));

    // A profile without `ca` asks for no file.
    let plain = RelayProfile::from_fields("relay.example.org:443", None, None, None).unwrap();
    assert_eq!(read_ca(&file, &plain).unwrap(), None);

    for (name, content, says) in [
        ("missing.pem", None, "not in the folder"),
        ("empty.pem", Some(Vec::new()), "holds no certificate"),
        (
            "key.pem",
            Some(b"-----BEGIN PRIVATE KEY-----\n".to_vec()),
            "holds no certificate",
        ),
        (
            "large.pem",
            Some(vec![b'A'; usize::try_from(MAX_CA_BYTES).unwrap() + 1]),
            "larger than 256 KiB",
        ),
    ] {
        if let Some(content) = content {
            folder.write(name, &content);
        }
        let profile = plain.clone().with_ca(Some(name)).unwrap();
        let error = read_ca(&file, &profile).unwrap_err();
        assert_eq!(error.file, file, "{name}");
        assert_eq!(error.problem.field, Some(Field::Ca), "{name}");
        assert!(
            error.problem.text.contains(says),
            "{name}: {}",
            error.problem.text
        );
        // The message names the setting, not the file it points at.
        assert!(!error.problem.text.contains(name), "{}", error.problem.text);
    }
}

#[test]
fn a_certificate_file_that_a_person_chose_is_read_with_the_same_rules() {
    let folder = Folder::new("chosen-ca");
    let pem = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    let file = folder.write("authority.crt", pem);
    assert_eq!(read_certificate_file(&file).unwrap(), pem);

    let missing = read_certificate_file(&folder.0.join("missing.crt")).unwrap_err();
    assert_eq!(missing.field, Some(Field::Ca));
    assert!(missing.text.contains("is not there"), "{}", missing.text);
    let key = folder.write("key.pem", b"-----BEGIN PRIVATE KEY-----\n");
    assert!(
        read_certificate_file(&key)
            .unwrap_err()
            .text
            .contains("holds no certificate")
    );
    let large = folder.write(
        "large.pem",
        &vec![b'A'; usize::try_from(MAX_CA_BYTES).unwrap() + 1],
    );
    assert!(
        read_certificate_file(&large)
            .unwrap_err()
            .text
            .contains("larger than 256 KiB")
    );
    assert!(
        read_certificate_file(&folder.0)
            .unwrap_err()
            .text
            .contains("not a file")
    );
}

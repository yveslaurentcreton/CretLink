//! Tests of Settings → Relay over a home of its own (ST-M007-011): what is
//! kept, what a copy that runs once refuses, what wins, and that the key
//! never comes back to the window.

use std::time::Duration;

use cretlink_relay::{AdmissionPolicy, AdmissionToken, ClientId};
use serde_json::json;

use super::*;
use crate::relay::testing::{Home, RELAY_NAME, TestRelay};

const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";

/// A form as the window sends it.
fn form(address: &str) -> RelayForm {
    RelayForm {
        address: address.to_owned(),
        key_mode: KeyMode::Keep,
        key: None,
        ca_mode: CaMode::Keep,
        ca_pick: None,
        name: None,
    }
}

fn with_key(mut form: RelayForm, key: &str) -> RelayForm {
    form.key_mode = KeyMode::Set;
    form.key = Some(key.to_owned());
    form
}

fn stored(home: &Home) -> Settings {
    settings::read(&home.layout).unwrap()
}

fn stored_key(home: &Home) -> Option<Vec<u8>> {
    stored(home)
        .relay
        .and_then(|relay| relay.key().map(|key| key.as_bytes().to_vec()))
}

/// A certificate that can be used, as the text of a PEM file.
fn certificate() -> Vec<u8> {
    let (cert, _key) = cretlink_wss::testcert::self_signed(RELAY_NAME).unwrap();
    crate::relay::testing::pem(cert.as_ref()).into_bytes()
}

#[test]
fn a_fresh_installed_copy_has_no_relay_and_can_be_given_one() {
    let home = Home::new("settings-fresh");
    let copy = home.installed();
    let view = RelaySettingsView::of(&copy);
    assert!(view.editable);
    assert_eq!(view.source, "none");
    assert_eq!(
        view.file.path,
        relay_profile::path_next_to(&copy.exe)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!((&view.file.relay, &view.file.problem), (&None, &None));
    let kept = view.stored.unwrap();
    assert_eq!(kept.path, home.layout.settings.display().to_string());
    assert_eq!((kept.relay, kept.problem), (None, None));
    // Looking wrote nothing.
    assert!(home.folder.snapshot().is_empty());
}

#[test]
fn saving_keeps_the_relay_and_the_key_never_comes_back() {
    let home = Home::new("settings-save");
    let copy = home.installed();
    let pending = PendingCa::default();
    let view = save(
        &copy,
        &with_key(form(" Relay.Example.org:443 "), KEY),
        &pending,
    )
    .unwrap();
    assert_eq!(view.source, "settings");
    assert_eq!(
        serde_json::to_value(&view).unwrap(),
        json!({
            "editable": true,
            "source": "settings",
            "file": { "path": view.file.path },
            "stored": {
                "path": home.layout.settings.display().to_string(),
                "relay": {
                    "host": "relay.example.org",
                    "address": "relay.example.org:443",
                    "keySet": true,
                },
            },
        })
    );
    // The key is in the settings file, and in nothing the window gets.
    assert_eq!(stored_key(&home).as_deref(), Some(KEY.as_bytes()));
    assert!(!format!("{view:?}").contains(KEY));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&home.layout.settings)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_form_keeps_the_key_changes_it_or_drops_it() {
    let home = Home::new("settings-key");
    let copy = home.installed();
    let pending = PendingCa::default();
    save(
        &copy,
        &with_key(form("relay.example.org:443"), KEY),
        &pending,
    )
    .unwrap();

    // Another address, the key left alone: it is still the same key.
    let view = save(&copy, &form("host-01.example.org:9970"), &pending).unwrap();
    assert_eq!(
        view.stored.unwrap().relay.unwrap().address,
        "host-01.example.org:9970"
    );
    assert_eq!(stored_key(&home).as_deref(), Some(KEY.as_bytes()));

    save(
        &copy,
        &with_key(
            form("host-01.example.org:9970"),
            "another-key-for-this-copy",
        ),
        &pending,
    )
    .unwrap();
    assert_eq!(
        stored_key(&home).as_deref(),
        Some(&b"another-key-for-this-copy"[..])
    );

    let mut none = form("host-01.example.org:9970");
    none.key_mode = KeyMode::None;
    let view = save(&copy, &none, &pending).unwrap();
    assert!(!view.stored.unwrap().relay.unwrap().key_set);
    assert_eq!(stored_key(&home), None);
}

#[test]
fn a_form_that_cannot_be_used_names_its_setting_and_changes_nothing() {
    let home = Home::new("settings-refused");
    let copy = home.installed();
    let pending = PendingCa::default();
    save(
        &copy,
        &with_key(form("relay.example.org:443"), KEY),
        &pending,
    )
    .unwrap();
    let before = home.folder.snapshot();

    let mut bad_name = form("relay.example.org:443");
    bad_name.name = Some("not a name".to_owned());
    let mut stale_ca = form("relay.example.org:443");
    stale_ca.ca_mode = CaMode::Chosen;
    stale_ca.ca_pick = Some(9999);
    for (form, stage, says) in [
        (form("relay.example.org"), "relay_address", "needs a port"),
        (
            form("wss://relay.example.org:443"),
            "relay_address",
            "always uses TLS",
        ),
        (
            with_key(form("relay.example.org:443"), ""),
            "relay_key",
            "`key` is empty",
        ),
        (
            with_key(form("relay.example.org:443"), "two words"),
            "relay_key",
            "a blank",
        ),
        (
            bad_name,
            "relay_name",
            "the name in the relay's certificate",
        ),
        (stale_ca, "relay_ca", "choose it again"),
    ] {
        let error = save(&copy, &form, &pending).unwrap_err();
        assert_eq!(error.stage, stage, "{}", error.message);
        assert!(error.message.contains(says), "{}", error.message);
        assert!(error.message.ends_with('.'), "{}", error.message);
        assert!(!error.message.contains(KEY));
        assert_eq!(home.folder.snapshot(), before, "{stage}");
    }
}

#[test]
fn a_copy_that_runs_once_keeps_no_settings_and_says_where_its_relay_goes() {
    let home = Home::new("settings-run-once");
    let copy = home.run_once();
    let pending = PendingCa::default();
    let view = RelaySettingsView::of(&copy);
    assert!(!view.editable);
    assert_eq!(view.stored, None);
    assert!(serde_json::to_value(&view).unwrap().get("stored").is_none());

    for refused in [
        save(
            &copy,
            &with_key(form("relay.example.org:443"), KEY),
            &pending,
        ),
        remove(&copy, &pending),
    ] {
        let error = refused.unwrap_err();
        assert_eq!(error.stage, "relay_run_once");
        assert!(error.message.contains("cretlink.toml next to the program"));
    }
    assert!(
        relay_to_test(&copy, Some(&form("relay.example.org:443")), &pending)
            .is_err_and(|error| error.stage == "relay_run_once")
    );
    // Nothing was written anywhere.
    assert!(home.folder.snapshot().is_empty());
}

#[test]
fn a_chosen_certificate_file_is_kept_next_to_the_settings() {
    let home = Home::new("settings-ca");
    let copy = home.installed();
    let pending = PendingCa::default();
    let pem = certificate();

    let mut chosen = form("192.0.2.1:443");
    chosen.name = Some(RELAY_NAME.to_owned());
    chosen.ca_mode = CaMode::Chosen;
    chosen.ca_pick = Some(pending.keep(pem.clone()));
    let view = save(&copy, &chosen, &pending).unwrap();
    let relay = view.stored.unwrap().relay.unwrap();
    assert_eq!(relay.ca.as_deref(), Some("relay-ca.pem"));
    assert_eq!(relay.host, RELAY_NAME);
    assert_eq!(std::fs::read(&home.layout.relay_ca).unwrap(), pem);
    // The choice was used up: its number names nothing any more.
    assert!(save(&copy, &chosen, &pending).is_err_and(|error| error.stage == "relay_ca"));

    // Saved again with the file left alone: it stays.
    let mut keep = form("192.0.2.1:9970");
    keep.name = Some(RELAY_NAME.to_owned());
    let view = save(&copy, &keep, &pending).unwrap();
    assert_eq!(
        view.stored.unwrap().relay.unwrap().ca.as_deref(),
        Some("relay-ca.pem")
    );
    assert!(home.layout.relay_ca.is_file());

    // Back to the system's authorities: the file goes with the setting.
    let mut none = keep.clone();
    none.ca_mode = CaMode::None;
    let view = save(&copy, &none, &pending).unwrap();
    assert_eq!(view.stored.unwrap().relay.unwrap().ca, None);
    assert!(!home.layout.relay_ca.exists());
}

#[test]
fn a_chosen_file_must_hold_a_certificate_that_can_be_used() {
    let home = Home::new("settings-ca-file");
    let pem = certificate();
    let good = home.folder.write("authority.pem", &pem);
    assert_eq!(read_chosen_ca(&good).unwrap(), pem);

    let not_one =
        "-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGU=\n-----END CERTIFICATE-----\n";
    for (name, content, says) in [
        (
            "key.pem",
            "-----BEGIN PRIVATE KEY-----\n",
            "holds no certificate",
        ),
        ("fake.pem", not_one, "no certificate that can be used"),
        ("empty.pem", "", "holds no certificate"),
    ] {
        let error = read_chosen_ca(&home.folder.write(name, content)).unwrap_err();
        assert_eq!(error.stage, "relay_ca", "{name}");
        assert!(error.message.contains(says), "{name}: {}", error.message);
    }
    let missing = read_chosen_ca(&home.folder.path("missing.pem")).unwrap_err();
    assert_eq!(missing.stage, "relay_ca");
    assert!(
        missing.message.contains("is not there"),
        "{}",
        missing.message
    );
}

#[test]
fn removing_takes_the_relay_and_its_certificate_file_and_leaves_the_network() {
    let home = Home::new("settings-remove");
    let copy = home.installed();
    let pending = PendingCa::default();
    let mut chosen = with_key(form("192.0.2.1:443"), KEY);
    chosen.name = Some(RELAY_NAME.to_owned());
    chosen.ca_mode = CaMode::Chosen;
    chosen.ca_pick = Some(pending.keep(certificate()));
    save(&copy, &chosen, &pending).unwrap();
    // The network to wait on is kept in the same file (ST-M009-016).
    let mut kept = stored(&home);
    kept.network_adapter = Some("Wi-Fi".to_owned());
    settings::write(&home.layout, &kept).unwrap();

    // Saving a relay leaves it alone, and so does removing the relay.
    save(&copy, &form("192.0.2.1:9970"), &pending).unwrap();
    assert_eq!(stored(&home).network_adapter.as_deref(), Some("Wi-Fi"));
    let view = remove(&copy, &pending).unwrap();
    assert_eq!(view.source, "none");
    assert_eq!(view.stored.unwrap().relay, None);
    assert!(!home.layout.relay_ca.exists());
    let left = stored(&home);
    assert!(left.relay.is_none());
    assert_eq!(left.network_adapter.as_deref(), Some("Wi-Fi"));
    assert!(
        !std::fs::read_to_string(&home.layout.settings)
            .unwrap()
            .contains(KEY)
    );

    // With nothing else in it, the file goes too.
    settings::write(&home.layout, &Settings::default()).unwrap();
    save(&copy, &with_key(form("192.0.2.1:443"), KEY), &pending).unwrap();
    remove(&copy, &pending).unwrap();
    assert!(!home.layout.settings.exists());
}

#[test]
fn settings_that_cannot_be_read_are_shown_as_such_and_removed_as_a_whole() {
    let home = Home::new("settings-unreadable");
    let copy = home.installed();
    home.folder.write(
        home.layout.settings.to_str().unwrap(),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = {KEY}\n"),
    );
    let view = RelaySettingsView::of(&copy);
    assert_eq!(view.source, "settings");
    let problem = view.stored.unwrap().problem.unwrap();
    assert_eq!(problem.field, Some("key"));
    assert_eq!(
        problem.message,
        "Line 3: the value of `key` must be text in double quotes."
    );
    assert!(!format!("{problem:?}").contains(KEY));
    // It cannot be tested, and the refusal does not repeat the key either.
    let error = relay_to_test(&copy, None, &PendingCa::default()).unwrap_err();
    assert_eq!(error.stage, "relay_profile");
    assert!(!error.message.contains(KEY), "{}", error.message);

    let view = remove(&copy, &PendingCa::default()).unwrap();
    assert_eq!(view.source, "none");
    assert!(!home.layout.settings.exists());
}

#[test]
fn a_file_next_to_the_program_wins_and_the_view_says_so() {
    let home = Home::new("settings-file-wins");
    let copy = home.installed();
    let pending = PendingCa::default();
    save(
        &copy,
        &with_key(form("relay.example.org:443"), KEY),
        &pending,
    )
    .unwrap();
    let file = relay_profile::path_next_to(&copy.exe).unwrap();
    home.folder.write(
        file.to_str().unwrap(),
        "[relay]\naddress = \"host-01.example.org:9970\"\n",
    );

    let view = RelaySettingsView::of(&copy);
    assert_eq!(view.source, "file");
    assert_eq!(
        view.file.relay.as_ref().unwrap().address,
        "host-01.example.org:9970"
    );
    // The relay of the settings is still shown, as the one that is not used.
    assert_eq!(
        view.stored.as_ref().unwrap().relay.as_ref().unwrap().host,
        "relay.example.org"
    );
    // What is tested without a form is the relay this copy uses.
    let tested = relay_to_test(&copy, None, &pending).unwrap();
    assert_eq!(tested.host(), "host-01.example.org");

    // A copy that runs once shows its file too, and nothing of the settings.
    let run_once = home.run_once();
    home.folder.write(
        relay_profile::path_next_to(&run_once.exe)
            .unwrap()
            .to_str()
            .unwrap(),
        "[relay]\naddress = \"192.0.2.1:443\"\n",
    );
    let view = RelaySettingsView::of(&run_once);
    assert_eq!(view.source, "file");
    assert_eq!(view.file.relay.unwrap().address, "192.0.2.1:443");
    assert_eq!(view.stored, None);
}

#[test]
fn without_a_relay_there_is_nothing_to_test() {
    let home = Home::new("settings-nothing");
    let error = relay_to_test(&home.installed(), None, &PendingCa::default()).unwrap_err();
    assert_eq!(error.stage, "relay_none");
}

#[tokio::test]
async fn the_form_is_tested_as_it_stands_before_anything_is_saved() {
    let policy = AdmissionPolicy::Allowlist(vec![(
        AdmissionToken::from_text(KEY),
        ClientId::new("sam-laptop"),
    )]);
    let relay = TestRelay::start(policy).await;
    let home = Home::new("settings-test");
    let copy = home.installed();
    let pending = PendingCa::default();

    let mut unsaved = with_key(form(&relay.addr.to_string()), KEY);
    unsaved.name = Some(RELAY_NAME.to_owned());
    unsaved.ca_mode = CaMode::Chosen;
    unsaved.ca_pick = Some(pending.keep(relay.cert_pem.clone().into_bytes()));

    let step = Duration::from_secs(20);
    let to_test = relay_to_test(&copy, Some(&unsaved), &pending).unwrap();
    let result = tokio::time::timeout(step, to_test.test(Audience::Technician))
        .await
        .unwrap();
    assert!(result.ok, "{result:?}");
    // Testing saved nothing, and the chosen file is still at hand for Save.
    assert!(home.folder.snapshot().is_empty());

    // A wrong key in the form is said before it is saved.
    let wrong = with_key(unsaved.clone(), "not-the-key-of-this-copy");
    let to_test = relay_to_test(&copy, Some(&wrong), &pending).unwrap();
    let result = tokio::time::timeout(step, to_test.test(Audience::Technician))
        .await
        .unwrap();
    assert_eq!(result.kind, "key_refused");

    // Saved, the same relay is tested without a form, with the kept key and
    // the kept certificate file.
    save(&copy, &unsaved, &pending).unwrap();
    assert_eq!(
        std::fs::read_to_string(&home.layout.relay_ca).unwrap(),
        relay.cert_pem
    );
    let to_test = relay_to_test(&copy, None, &pending).unwrap();
    let result = tokio::time::timeout(step, to_test.test(Audience::Technician))
        .await
        .unwrap();
    assert!(result.ok, "{result:?}");
    // And a form that keeps both tests the same.
    let mut keep = form(&relay.addr.to_string());
    keep.name = Some(RELAY_NAME.to_owned());
    let to_test = relay_to_test(&copy, Some(&keep), &pending).unwrap();
    let result = tokio::time::timeout(step, to_test.test(Audience::Technician))
        .await
        .unwrap();
    assert!(result.ok, "{result:?}");
}

#[test]
fn a_form_from_the_window_is_read_strictly_and_its_key_is_not_printed() {
    let form: RelayForm = serde_json::from_value(json!({
        "address": "relay.example.org:443",
        "keyMode": "set",
        "key": KEY,
        "caMode": "chosen",
        "caPick": 3,
        "name": "",
    }))
    .unwrap();
    assert_eq!(form.key_mode, KeyMode::Set);
    assert_eq!(form.ca_mode, CaMode::Chosen);
    assert_eq!(form.ca_pick, Some(3));
    let shown = format!("{form:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("has_key: true"), "{shown}");

    let minimal: RelayForm = serde_json::from_value(json!({
        "address": "relay.example.org:443",
        "keyMode": "keep",
        "caMode": "none",
    }))
    .unwrap();
    assert_eq!(
        (minimal.key, minimal.ca_pick, minimal.name),
        (None, None, None)
    );
    // A field the form does not have, and a mode it does not know, are refused.
    for bad in [
        json!({ "address": "a:1", "keyMode": "keep", "caMode": "keep", "ca": "/etc/x.pem" }),
        json!({ "address": "a:1", "keyMode": "show", "caMode": "keep" }),
        json!({ "keyMode": "keep", "caMode": "keep" }),
    ] {
        assert!(serde_json::from_value::<RelayForm>(bad).is_err());
    }
}

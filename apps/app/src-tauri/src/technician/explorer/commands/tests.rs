//! Tests of the commands that send, copy, rename and delete (ST-M010-002):
//! none works without a session under full control, a folder from the window
//! must be a path on a drive of the customer computer, a copy goes only where
//! the technician chose, and every action is recorded as the technician's.
//!
//! The wire accepts a path only when it is rooted on a drive, so the whole
//! way through a real session — a file sent into a folder, copied back,
//! renamed and deleted — runs on Windows. Everywhere, the commands are driven
//! through a session for what is refused and for the end of the session; the
//! queue itself is tested against a customer computer held in memory
//! (`transfers/tests.rs`, `changes/tests.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cretlink_protocol::FileKind;
use cretlink_shared::{Actor, Outcome};

use super::super::tests::{failure, live};
use super::*;

const DOCUMENTS: &str = "C:\\Users\\ann\\Documents";

fn nobody() -> Arc<dyn Fn(RemovalProgress) + Send + Sync> {
    Arc::new(|_| {})
}

fn file_item(name: &str) -> DeleteItem {
    DeleteItem {
        name: name.to_owned(),
        kind: FileKind::File,
    }
}

fn one(name: &str) -> Vec<Selected> {
    vec![Selected {
        name: name.to_owned(),
        kind: EntryKind::File,
    }]
}

// --- No authority of its own ---

/// Outside a session under full control every command refuses before
/// anything is asked, kept or queued.
#[tokio::test]
async fn without_a_session_every_command_fails_closed() {
    let state = AppState::live_idle();
    let offer = state
        .transfers
        .offer(vec![PathBuf::from("notes.txt")])
        .offer;
    let closed = "no_consented_session";

    assert_eq!(
        failure(send_check(&state, offer, DOCUMENTS).await).0,
        closed
    );
    assert_eq!(
        failure(send(&state, offer, DOCUMENTS, &HashMap::new())).0,
        closed
    );
    assert_eq!(
        failure(download_root(
            &state,
            Some(PathBuf::from("Downloads")),
            None
        ))
        .0,
        closed
    );
    assert_eq!(
        failure(download(
            &state,
            DOCUMENTS,
            &one("report.pdf"),
            std::path::Path::new("Downloads"),
            &HashMap::new()
        ))
        .0,
        closed
    );
    assert_eq!(
        failure(rename(&state, DOCUMENTS, "a.txt", "b.txt").await).0,
        closed
    );
    assert_eq!(
        failure(delete(&state, DOCUMENTS, &[file_item("a.txt")], 1, nobody()).await).0,
        closed
    );
    assert!(state.transfers.snapshot().is_empty(), "nothing was queued");
    assert_eq!(state.transfers.must_start(), None);
}

/// A session without an open audit trail is no session for these commands
/// either: nothing the technician changes may miss the trail (REQ-S-017).
#[tokio::test]
async fn without_an_audit_trail_every_command_fails_closed() {
    let mut live = live().await;
    *live.state.audit.lock().unwrap() = None;
    let closed = "no_consented_session";
    assert_eq!(
        failure(rename(&live.state, DOCUMENTS, "a.txt", "b.txt").await).0,
        closed
    );
    assert_eq!(
        failure(delete(&live.state, DOCUMENTS, &[file_item("a.txt")], 1, nobody()).await).0,
        closed
    );
    assert_eq!(
        failure(send_check(&live.state, 1, DOCUMENTS).await).0,
        closed
    );
    assert_eq!(
        failure(download_root(
            &live.state,
            Some(PathBuf::from("Downloads")),
            None
        ))
        .0,
        closed
    );
    assert!(live.recorded().is_empty());
    live.service.stop();
}

// --- What the window can name ---

#[test]
fn a_folder_from_the_window_is_a_path_on_a_drive_of_the_customer_computer() {
    for (text, folder) in [
        ("C:\\Users\\ann\\Documents", "C:\\Users\\ann\\Documents"),
        ("C:\\Users\\ann\\Documents\\", "C:\\Users\\ann\\Documents"),
        ("c:/users/ann", "c:\\users\\ann"),
        ("  D:\\Tools  ", "D:\\Tools"),
        ("C:\\", "C:\\"),
        ("E:/", "E:\\"),
    ] {
        assert_eq!(
            remote_folder(text).map_err(|error| error.message),
            Ok(folder.to_owned())
        );
    }
    for text in [
        "",
        "Documents",
        "..\\Windows",
        "C:",
        "C:\\Users\\..\\Windows",
        "\\\\server\\share",
        "\\\\?\\C:\\Windows",
        "/home/ann",
        "C:\\Users\\ann\\NUL",
        "C:\\file.txt:stream",
    ] {
        let (stage, message) = failure(remote_folder(text));
        assert_eq!(stage, "invalid_path", "{text:?}");
        assert!(
            message.starts_with("this is not a folder on a drive of the customer computer"),
            "{text:?}: {message}"
        );
    }
}

/// A copy goes to `Downloads\CretLink\<target>` or to a folder the technician
/// chose in the system's dialog, and nowhere the window could name itself.
#[tokio::test]
async fn a_copy_goes_to_the_default_folder_or_to_one_the_technician_chose() {
    let live = live().await;
    let downloads = PathBuf::from("home").join("Downloads");
    assert_eq!(
        download_root(&live.state, Some(downloads.clone()), None).map_err(|error| error.stage),
        Ok(downloads.join("CretLink").join("CUSTOMER-PC"))
    );
    // An account without a Downloads folder: the technician chooses one.
    let (stage, message) = failure(download_root(&live.state, None, None));
    assert_eq!(stage, "local");
    assert!(message.contains("Download to…"), "{message}");

    let chosen = live.state.transfers.choose(PathBuf::from("chosen"));
    assert_eq!(
        download_root(&live.state, Some(downloads.clone()), Some(chosen))
            .map_err(|error| error.stage),
        Ok(PathBuf::from("chosen"))
    );
    // A number nobody chose a folder under names no folder.
    let (stage, _) = failure(download_root(
        &live.state,
        Some(downloads),
        Some(chosen + 50),
    ));
    assert_eq!(stage, "refused");
    live.service.stop();
}

/// A selection is names of the open folder: a name that is a path is refused
/// whole, before anything is queued, and so is an empty selection.
#[tokio::test]
async fn a_selection_whose_name_is_a_path_is_refused_whole() {
    let live = live().await;
    let root = std::path::Path::new("Downloads");
    for name in ["..\\..\\secret.txt", "sub\\a.txt", "a/b", "C:\\x.txt", ""] {
        let mut names = one("fine.txt");
        names.extend(one(name));
        let (stage, _) = failure(download(
            &live.state,
            DOCUMENTS,
            &names,
            root,
            &HashMap::new(),
        ));
        assert_eq!(stage, "invalid_path", "{name:?}");
    }
    let (stage, _) = failure(download(&live.state, DOCUMENTS, &[], root, &HashMap::new()));
    assert_eq!(stage, "refused");
    let (stage, _) = failure(download(
        &live.state,
        "Documents",
        &one("fine.txt"),
        root,
        &HashMap::new(),
    ));
    assert_eq!(
        stage, "invalid_path",
        "the folder must be a path on a drive"
    );
    assert!(live.state.transfers.snapshot().is_empty());
    live.service.stop();
}

/// Through a session: a rename and a delete that the customer computer
/// refuses say so, are recorded as the technician's, and change nothing; a
/// new name that is a path is refused before anything is asked or recorded.
#[tokio::test]
async fn a_refused_change_says_why_and_is_recorded() {
    let mut live = live().await;
    // No such folder on the customer computer of this test.
    let folder = "Q:\\cretlink-no-such-folder";

    let (stage, message) = failure(rename(&live.state, folder, "a.txt", "..\\b.txt").await);
    assert_eq!(stage, "invalid_path");
    assert_eq!(
        message,
        "a name cannot hold any of \\ / : * ? \" < > | or a control character"
    );
    assert!(live.recorded().is_empty(), "nothing was asked or recorded");

    let (stage, _) = failure(rename(&live.state, folder, "a.txt", "b.txt").await);
    assert_ne!(stage, "no_consented_session");
    let records = live.recorded();
    let actions: Vec<&str> = records.iter().map(|entry| entry.action).collect();
    assert_eq!(actions, ["console_move_start", "console_move"]);
    assert!(records.iter().all(|entry| entry.actor == Actor::Console));
    assert_eq!(records[1].outcome, Outcome::Failed);

    // A delete answers with what could not be removed, not with an error.
    let report = delete(&live.state, folder, &[file_item("a.txt")], 4, nobody())
        .await
        .expect("a delete that removed nothing is a report");
    assert_eq!((report.selected, report.removed), (1, 0));
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].path, format!("{folder}\\a.txt"));
    assert!(!report.cancelled && report.ended.is_none());
    let records = live.recorded();
    assert_eq!(
        records.iter().map(|entry| entry.action).collect::<Vec<_>>(),
        ["console_delete_start", "console_delete"]
    );
    assert_eq!(records[1].outcome, Outcome::Failed);

    // Nothing is selected, or more than one delete takes.
    assert_eq!(
        failure(delete(&live.state, folder, &[], 5, nobody()).await).0,
        "refused"
    );
    let many: Vec<DeleteItem> = (0..=MAX_DELETE_ITEMS)
        .map(|n| file_item(&format!("f{n}")))
        .collect();
    let (stage, message) = failure(delete(&live.state, folder, &many, 6, nobody()).await);
    assert_eq!(stage, "refused");
    assert!(message.contains("at most 10000"), "{message}");
    assert!(live.recorded().is_empty(), "a refused delete asks nothing");

    // A folder that cannot be looked at: the send is not queued.
    let offer = live
        .state
        .transfers
        .offer(vec![PathBuf::from("notes.txt")])
        .offer;
    let (stage, _) = failure(send_check(&live.state, offer, folder).await);
    assert_ne!(stage, "no_consented_session");
    let records = live.recorded();
    assert_eq!(
        records.last().map(|entry| entry.action),
        Some("console_fs_list")
    );
    assert_eq!(
        records.last().map(|entry| entry.outcome),
        Some(Outcome::Failed)
    );
    assert_eq!(
        failure(send(&live.state, offer, folder, &HashMap::new())).0,
        "refused",
        "an offer that was not checked against the folder is not queued"
    );
    assert!(live.state.transfers.snapshot().is_empty());
    live.service.stop();
}

/// A stop ends the session for these commands too: each fails closed, a
/// delete that runs is cancelled by its number only while it runs, and what
/// waits in the queue is stopped, with the reason.
#[tokio::test]
async fn after_a_stop_every_command_fails_closed_and_the_queue_stops() {
    let live = live().await;
    // Something waits in the queue when the session ends.
    let dir = std::env::temp_dir().join(format!("cretlink-app-commands-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (_, entries) = live
        .state
        .transfers
        .queue_download(
            DOCUMENTS,
            &[("report.pdf".to_owned(), EntryKind::File)],
            &dir,
            &HashMap::new(),
        )
        .unwrap();
    assert_eq!(entries.len(), 1);

    live.service.stop();
    let closed = "no_consented_session";
    assert_eq!(
        failure(rename(&live.state, DOCUMENTS, "a.txt", "b.txt").await).0,
        closed
    );
    let offer = live
        .state
        .transfers
        .offer(vec![PathBuf::from("notes.txt")])
        .offer;
    assert_eq!(
        failure(send_check(&live.state, offer, DOCUMENTS).await).0,
        closed
    );
    // A delete answers with a report: it removed nothing, tried nothing more,
    // and says that the session ended.
    let report = delete(
        &live.state,
        DOCUMENTS,
        &[file_item("a.txt"), file_item("b.txt")],
        1,
        nobody(),
    )
    .await
    .expect("a delete the session's end stopped is a report");
    assert_eq!((report.removed, report.not_tried), (0, 2));
    assert_eq!(
        report.ended.as_deref(),
        Some("The session ended before this was complete.")
    );

    // The worker finds the session gone and stops what waits.
    let state = live.state.clone();
    let epoch = live.state.transfers.must_start().expect("an entry waits");
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let emit: Emit = {
        let delivered = Arc::clone(&delivered);
        Arc::new(move |views: Vec<TransferView>| delivered.lock().unwrap().extend(views))
    };
    transfers::run(
        live.state.transfers.clone(),
        epoch,
        move || state.files().map(LiveFiles::new),
        emit,
    )
    .await;
    let rows = live.state.transfers.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        serde_json::to_value(&rows[0]).unwrap()["state"],
        serde_json::json!("stopped")
    );
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("The session ended before this was complete.")
    );
    assert!(!dir.join("report.pdf").exists());
    // A cancel of a delete that does not run does nothing.
    live.state.removals.cancel(99);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole way through a session, on Windows: a file and a folder are
/// sent into a real folder, a name that is taken is kept beside, a file is
/// copied back, renamed and deleted, and a folder is deleted with its
/// content. Every step is recorded as the technician's.
#[cfg(windows)]
#[tokio::test]
async fn files_are_sent_copied_back_renamed_and_deleted_through_a_session() {
    let mut live = live().await;
    let base = std::env::temp_dir().join(format!("cretlink-app-change-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("customer").join("Documents")).unwrap();
    std::fs::create_dir_all(base.join("technician").join("Project").join("src")).unwrap();
    // The folders by their real names: a runner may give them by a short name.
    let real = std::fs::canonicalize(&base).expect("the real path");
    let real = real.display().to_string();
    let base = PathBuf::from(real.strip_prefix("\\\\?\\").unwrap_or(&real));
    let documents = base
        .join("customer")
        .join("Documents")
        .display()
        .to_string();
    let local = base.join("technician");
    std::fs::write(local.join("notes.txt"), b"notes").unwrap();
    std::fs::write(local.join("Project").join("readme.txt"), b"readme").unwrap();
    std::fs::write(
        local.join("Project").join("src").join("main.rs"),
        b"fn main() {}",
    )
    .unwrap();
    std::fs::write(format!("{documents}\\notes.txt"), b"theirs").unwrap();

    let carry = |state: AppState| async move {
        if let Some(epoch) = state.transfers.must_start() {
            let live = state.clone();
            transfers::run(
                state.transfers.clone(),
                epoch,
                move || live.files().map(LiveFiles::new),
                Arc::new(|_| {}),
            )
            .await;
        }
    };
    let states = |state: &AppState| -> Vec<(String, String)> {
        state
            .transfers
            .snapshot()
            .iter()
            .map(|view| {
                (
                    view.name.clone(),
                    serde_json::to_value(view.state)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect()
    };

    // Send: a name that is taken is asked about, and kept beside.
    let offer = live
        .state
        .transfers
        .offer(vec![local.join("notes.txt"), local.join("Project")])
        .offer;
    let check = send_check(&live.state, offer, &documents)
        .await
        .expect("the folder is looked at");
    assert_eq!(check.conflicts.len(), 1);
    assert_eq!(check.conflicts[0].name, "notes.txt");
    let choices: HashMap<String, Choice> = [("notes.txt".to_owned(), Choice::KeepBoth)].into();
    let (queued, _) = send(&live.state, offer, &documents, &choices).expect("queued");
    assert_eq!(queued.queued, 3);
    carry(live.state.clone()).await;
    assert_eq!(
        states(&live.state),
        [
            ("notes (2).txt".to_owned(), "done".to_owned()),
            ("Project\\readme.txt".to_owned(), "done".to_owned()),
            ("Project\\src\\main.rs".to_owned(), "done".to_owned()),
        ]
    );
    assert_eq!(
        std::fs::read(format!("{documents}\\notes.txt")).unwrap(),
        b"theirs"
    );
    assert_eq!(
        std::fs::read(format!("{documents}\\notes (2).txt")).unwrap(),
        b"notes"
    );
    assert_eq!(
        std::fs::read(format!("{documents}\\Project\\src\\main.rs")).unwrap(),
        b"fn main() {}"
    );

    // Copy back: a file and the folder, below the chosen folder.
    let root = base.join("copies");
    let names = vec![
        Selected {
            name: "notes (2).txt".to_owned(),
            kind: EntryKind::File,
        },
        Selected {
            name: "Project".to_owned(),
            kind: EntryKind::Folder,
        },
    ];
    download(&live.state, &documents, &names, &root, &HashMap::new()).expect("queued");
    carry(live.state.clone()).await;
    assert_eq!(std::fs::read(root.join("notes (2).txt")).unwrap(), b"notes");
    assert_eq!(
        std::fs::read(root.join("Project").join("src").join("main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert!(
        states(&live.state).iter().all(|(_, state)| state == "done"),
        "{:?}",
        states(&live.state)
    );

    // Rename, then delete a file and the folder with its content.
    rename(&live.state, &documents, "notes (2).txt", "kept.txt")
        .await
        .expect("renamed");
    assert!(std::path::Path::new(&format!("{documents}\\kept.txt")).exists());
    let (stage, _) = failure(rename(&live.state, &documents, "kept.txt", "notes.txt").await);
    assert_eq!(stage, "already_exists");
    let report = delete(
        &live.state,
        &documents,
        &[
            file_item("kept.txt"),
            DeleteItem {
                name: "Project".to_owned(),
                kind: FileKind::Dir,
            },
        ],
        1,
        nobody(),
    )
    .await
    .expect("deleted");
    assert_eq!((report.selected, report.removed), (2, 2));
    assert_eq!((report.removed_files, report.removed_dirs), (3, 2));
    assert!(report.failed.is_empty());
    let left: Vec<String> = std::fs::read_dir(&documents)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["notes.txt"]);

    let recorded: Vec<&str> = live.recorded().iter().map(|entry| entry.action).collect();
    for action in [
        "console_send",
        "console_make_dir",
        "console_download",
        "console_move",
        "console_delete",
        "console_remove_tree",
    ] {
        assert!(recorded.contains(&action), "{action}: {recorded:?}");
    }
    live.service.stop();
    let _ = std::fs::remove_dir_all(&base);
}

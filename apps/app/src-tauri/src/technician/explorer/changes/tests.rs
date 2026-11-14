//! Tests of renaming and deleting (ST-M010-002), against a customer computer
//! held in memory ([`Fake`]): a rename stays in its folder and never
//! replaces; a delete removes each item by what it is, names what it could
//! not remove with the reason, and ends honestly when it is cancelled or the
//! session ends.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use super::super::fake::{Fake, Node};
use super::*;

const DOCUMENTS: &str = "C:\\Users\\ann\\Documents";

fn at(name: &str) -> String {
    format!("{DOCUMENTS}\\{name}")
}

fn item(name: &str, kind: FileKind) -> DeleteItem {
    DeleteItem {
        name: name.to_owned(),
        kind,
    }
}

/// The steps a delete told the window, in order.
#[derive(Clone, Default)]
struct Told(Arc<Mutex<Vec<RemovalProgress>>>);

impl Told {
    fn sink(&self) -> Arc<dyn Fn(RemovalProgress) + Send + Sync> {
        let into = Arc::clone(&self.0);
        Arc::new(move |step| into.lock().unwrap().push(step))
    }

    fn all(&self) -> Vec<RemovalProgress> {
        self.0.lock().unwrap().clone()
    }
}

async fn remove(fake: &Fake, items: &[DeleteItem]) -> DeleteReport {
    delete(
        fake,
        1,
        DOCUMENTS,
        items,
        &CancelToken::new(),
        Told::default().sink(),
    )
    .await
}

// --- Rename ---

#[tokio::test]
async fn a_rename_gives_one_entry_another_name_in_its_folder() {
    let fake = Fake::new();
    fake.put(&at("report.txt"), Node::File(b"report".to_vec()));
    fake.folder(&at("Photos"));
    fake.put(&at("Photos\\a.jpg"), Node::File(b"a".to_vec()));

    rename(&fake, DOCUMENTS, "report.txt", "report 2026.txt")
        .await
        .expect("the file is renamed");
    assert_eq!(fake.at(&at("report.txt")), None);
    assert_eq!(
        fake.at(&at("report 2026.txt")),
        Some(Node::File(b"report".to_vec()))
    );

    // A folder keeps what it holds.
    rename(&fake, DOCUMENTS, "Photos", "Pictures")
        .await
        .unwrap();
    assert_eq!(
        fake.at(&at("Pictures\\a.jpg")),
        Some(Node::File(b"a".to_vec()))
    );
    assert_eq!(
        fake.asked(),
        [
            format!("rename {}", at("report.txt")),
            format!("rename {}", at("Photos"))
        ]
    );
}

/// A name that is taken is refused by the customer computer, and both
/// entries stay as they are.
#[tokio::test]
async fn a_rename_onto_a_name_that_is_taken_changes_nothing() {
    let fake = Fake::new();
    fake.put(&at("a.txt"), Node::File(b"first".to_vec()));
    fake.put(&at("b.txt"), Node::File(b"second".to_vec()));
    match rename(&fake, DOCUMENTS, "a.txt", "b.txt").await {
        Err(TargetError::Remote {
            code: ErrorCode::AlreadyExists,
            stage: ErrorStage::Execution,
            ..
        }) => {}
        other => panic!("expected a refusal of the customer computer, got {other:?}"),
    }
    assert_eq!(fake.at(&at("a.txt")), Some(Node::File(b"first".to_vec())));
    assert_eq!(fake.at(&at("b.txt")), Some(Node::File(b"second".to_vec())));
}

/// A new name is one name: a text that would move the entry elsewhere, or
/// that no name may be, is refused before anything is asked.
#[tokio::test]
async fn a_new_name_that_is_a_path_is_refused_before_anything_is_asked() {
    let fake = Fake::new();
    fake.put(&at("a.txt"), Node::File(b"a".to_vec()));
    for to in [
        "..\\a.txt",
        "sub\\a.txt",
        "sub/a.txt",
        "C:\\elsewhere.txt",
        "a.txt:stream",
        "",
        "   ",
        ".",
        "..",
        "what?.txt",
        "a*.txt",
        "a|b",
        "trailing.",
        "trailing ",
        "line\nbreak",
    ] {
        match rename(&fake, DOCUMENTS, "a.txt", to).await {
            Err(TargetError::Remote {
                code: ErrorCode::InvalidPath,
                stage: ErrorStage::Validation,
                ..
            }) => {}
            other => panic!("{to:?}: expected a refusal made here, got {other:?}"),
        }
    }
    // The name it has is no new name, and an entry is named by one name too.
    assert!(rename(&fake, DOCUMENTS, "a.txt", "a.txt").await.is_err());
    assert!(
        rename(&fake, DOCUMENTS, "..\\other\\a.txt", "b.txt")
            .await
            .is_err()
    );
    assert!(fake.asked().is_empty(), "{:?}", fake.asked());
    assert_eq!(fake.at(&at("a.txt")), Some(Node::File(b"a".to_vec())));
}

// --- Delete ---

/// Each item is removed by what it is: a file and a link as that one entry,
/// a folder with everything in it. What a link points at stays.
#[tokio::test]
async fn a_delete_removes_each_item_by_what_it_is() {
    let fake = Fake::new();
    fake.put(&at("a.txt"), Node::File(b"a".to_vec()));
    fake.folder(&at("Old"));
    fake.folder(&at("Old\\sub"));
    fake.put(&at("Old\\x.txt"), Node::File(b"x".to_vec()));
    fake.put(&at("Old\\sub\\y.txt"), Node::File(b"y".to_vec()));
    // A link, with what it points at kept beside it in the fake's own books.
    fake.folder(&at("shortcut"));
    fake.put(&at("shortcut\\behind.txt"), Node::File(b"behind".to_vec()));
    fake.put(&at("shortcut"), Node::Link);
    fake.put(&at("keep.txt"), Node::File(b"keep".to_vec()));

    let told = Told::default();
    let report = delete(
        &fake,
        7,
        DOCUMENTS,
        &[
            item("a.txt", FileKind::File),
            item("shortcut", FileKind::Symlink),
            item("Old", FileKind::Dir),
        ],
        &CancelToken::new(),
        told.sink(),
    )
    .await;
    assert_eq!(
        report,
        DeleteReport {
            selected: 3,
            removed: 3,
            removed_files: 4,
            removed_dirs: 2,
            ..DeleteReport::default()
        }
    );
    assert_eq!(fake.names(DOCUMENTS), ["keep.txt"]);
    assert_eq!(
        fake.asked(),
        [
            format!("delete {}", at("a.txt")),
            // The link is deleted as the link, never removed as a folder.
            format!("delete {}", at("shortcut")),
            format!("remove_tree {}", at("Old")),
        ]
    );
    assert_eq!(
        fake.names(&at("shortcut")),
        ["behind.txt"],
        "what the link pointed at was not touched"
    );

    // The window was told each item as it began, and the counts as they grew.
    let told = told.all();
    assert!(told.iter().all(|step| step.removal == 7 && step.of == 3));
    assert_eq!(
        told.iter()
            .map(|step| (step.item.as_str(), step.at))
            .collect::<Vec<_>>()[..3],
        [("a.txt", 1), ("shortcut", 2), ("Old", 3)]
    );
    let last = told.last().unwrap();
    assert_eq!((last.removed_files, last.removed_dirs), (4, 2));
    assert_eq!(
        serde_json::to_value(last).unwrap(),
        serde_json::json!({
            "removal": 7, "item": "Old", "at": 3, "of": 3, "removedFiles": 4, "removedDirs": 2,
        })
    );
}

/// What could not be removed is named with the reason the customer computer
/// gave, and an item counts as removed only when it is gone.
#[tokio::test]
async fn what_could_not_be_removed_is_named_with_its_reason() {
    let fake = Fake::new();
    fake.put(&at("open.docx"), Node::File(b"open".to_vec()));
    fake.refuse(&at("open.docx"), ErrorCode::FileInUse);
    fake.put(&at("fine.txt"), Node::File(b"fine".to_vec()));
    fake.folder(&at("Old"));
    fake.put(&at("Old\\held.log"), Node::File(b"held".to_vec()));
    fake.refuse(&at("Old\\held.log"), ErrorCode::AccessDenied);
    fake.put(&at("Old\\free.txt"), Node::File(b"free".to_vec()));

    let report = remove(
        &fake,
        &[
            item("open.docx", FileKind::File),
            item("gone.txt", FileKind::File),
            item("fine.txt", FileKind::File),
            item("Old", FileKind::Dir),
        ],
    )
    .await;
    assert_eq!(report.selected, 4);
    assert_eq!(report.removed, 1, "only fine.txt is gone");
    assert_eq!((report.removed_files, report.removed_dirs), (2, 0));
    assert!(!report.cancelled && report.ended.is_none() && report.not_tried == 0);
    assert_eq!(
        report
            .failed
            .iter()
            .map(|left| (left.path.as_str(), left.reason.as_str()))
            .collect::<Vec<_>>(),
        [
            (at("open.docx").as_str(), "the file is in use"),
            (at("gone.txt").as_str(), "the path does not exist"),
            (at("Old\\held.log").as_str(), "access was denied"),
            (at("Old").as_str(), "the folder is not empty"),
        ]
    );
    assert_eq!(fake.names(DOCUMENTS), ["Old", "open.docx"]);
    assert_eq!(fake.names(&at("Old")), ["held.log"]);
    assert_eq!(
        serde_json::to_value(&report).unwrap()["failed"][0],
        serde_json::json!({ "path": at("open.docx"), "reason": "the file is in use" })
    );
}

/// The first entries that could not be removed are named; the rest are
/// counted, so the result stays bounded.
#[tokio::test]
async fn many_failures_are_named_up_to_a_bound_and_counted_beyond_it() {
    let fake = Fake::new();
    let mut items = Vec::new();
    for n in 0..MAX_NAMED + 7 {
        let name = format!("held-{n:02}.txt");
        fake.put(&at(&name), Node::File(Vec::new()));
        fake.refuse(&at(&name), ErrorCode::AccessDenied);
        items.push(item(&name, FileKind::File));
    }
    let report = remove(&fake, &items).await;
    assert_eq!(report.removed, 0);
    assert_eq!(report.failed.len(), MAX_NAMED);
    assert_eq!(report.failed_more, 7);
}

/// The kind the window listed only chooses the operation. When it is out of
/// date the customer computer refuses, and nothing is removed that was not
/// asked for: a folder is not deleted as a file, and neither a file nor a
/// link is removed as a folder.
#[tokio::test]
async fn a_kind_that_is_out_of_date_removes_nothing() {
    let fake = Fake::new();
    fake.folder(&at("now-a-folder"));
    fake.put(&at("now-a-folder\\inside.txt"), Node::File(b"in".to_vec()));
    fake.put(&at("now-a-file"), Node::File(b"file".to_vec()));
    fake.folder(&at("now-a-link"));
    fake.put(
        &at("now-a-link\\behind.txt"),
        Node::File(b"behind".to_vec()),
    );
    fake.put(&at("now-a-link"), Node::Link);

    let report = remove(
        &fake,
        &[
            item("now-a-folder", FileKind::File),
            item("now-a-file", FileKind::Dir),
            item("now-a-link", FileKind::Dir),
        ],
    )
    .await;
    assert_eq!(report.removed, 0);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
    assert_eq!(report.failed.len(), 3);
    assert!(
        report
            .failed
            .iter()
            .all(|left| left.reason == "the path is not the expected kind")
    );
    assert_eq!(
        fake.names(DOCUMENTS),
        ["now-a-file", "now-a-folder", "now-a-link"]
    );
    assert_eq!(fake.names(&at("now-a-folder")), ["inside.txt"]);
    assert_eq!(fake.names(&at("now-a-link")), ["behind.txt"]);
}

/// A name that is a path is not removed, and nothing is asked for it.
#[tokio::test]
async fn an_item_whose_name_is_a_path_is_not_removed() {
    let fake = Fake::new();
    fake.put(&at("a.txt"), Node::File(Vec::new()));
    let report = remove(
        &fake,
        &[
            item("..\\..\\jdoe\\secret.txt", FileKind::File),
            item("a.txt", FileKind::File),
        ],
    )
    .await;
    assert_eq!(report.removed, 1);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(fake.asked(), [format!("delete {}", at("a.txt"))]);
}

/// A delete the technician cancels ends between two steps: it says what it
/// had removed, names the folder it had begun, and tries nothing more.
#[tokio::test]
async fn a_cancelled_delete_says_what_it_removed_and_tries_nothing_more() {
    let fake = Fake::new();
    fake.put(&at("first.txt"), Node::File(Vec::new()));
    fake.folder(&at("Big"));
    for n in 0..3 {
        fake.folder(&at(&format!("Big\\sub-{n}")));
        fake.put(&at(&format!("Big\\sub-{n}\\f.txt")), Node::File(Vec::new()));
    }
    fake.put(&at("after.txt"), Node::File(Vec::new()));
    fake.put(&at("later.txt"), Node::File(Vec::new()));

    let cancel = CancelToken::new();
    let token = cancel.clone();
    // Cancelled once the first folder inside is gone.
    fake.on_progress(Box::new(move |_, removed| {
        if removed == 1 {
            token.cancel();
        }
    }));
    let report = delete(
        &fake,
        1,
        DOCUMENTS,
        &[
            item("first.txt", FileKind::File),
            item("Big", FileKind::Dir),
            item("after.txt", FileKind::File),
            item("later.txt", FileKind::File),
        ],
        &cancel,
        Told::default().sink(),
    )
    .await;
    assert!(report.cancelled);
    assert_eq!(report.ended, None);
    assert_eq!(report.removed, 1, "first.txt; the folder is not gone");
    assert_eq!((report.removed_files, report.removed_dirs), (2, 1));
    assert_eq!(report.not_tried, 2);
    assert_eq!(
        report.failed,
        [NotRemoved {
            path: at("Big"),
            reason: "its removal was cancelled before it was complete".to_owned(),
        }]
    );
    assert_eq!(fake.names(DOCUMENTS), ["Big", "after.txt", "later.txt"]);
    assert!(!fake.asked().iter().any(|asked| asked.contains("after.txt")));

    // Cancelled before it began: nothing is tried at all.
    let cancel = CancelToken::new();
    cancel.cancel();
    let before = fake.asked().len();
    let report = delete(
        &fake,
        2,
        DOCUMENTS,
        &[item("after.txt", FileKind::File)],
        &cancel,
        Told::default().sink(),
    )
    .await;
    assert!(report.cancelled);
    assert_eq!((report.removed, report.not_tried), (0, 1));
    assert_eq!(fake.asked().len(), before);
}

/// The end of the session stops a delete: it says so, counts nothing as
/// removed that the customer computer did not confirm, and tries no more.
#[tokio::test]
async fn the_end_of_the_session_stops_a_delete_and_says_so() {
    for (error, words) in [
        (TargetError::NoConsentedSession, Ended::Stopped.words()),
        (TargetError::Disconnected, Ended::Lost.words()),
    ] {
        let fake = Fake::new();
        fake.put(&at("first.txt"), Node::File(Vec::new()));
        fake.folder(&at("Big"));
        for n in 0..3 {
            fake.folder(&at(&format!("Big\\sub-{n}")));
        }
        fake.put(&at("after.txt"), Node::File(Vec::new()));
        let (computer, ended) = (fake.clone(), error.clone());
        fake.on_progress(Box::new(move |_, removed| {
            if removed == 1 {
                computer.end(ended.clone());
            }
        }));
        let report = remove(
            &fake,
            &[
                item("first.txt", FileKind::File),
                item("Big", FileKind::Dir),
                item("after.txt", FileKind::File),
            ],
        )
        .await;
        assert_eq!(report.ended.as_deref(), Some(words));
        assert!(!report.cancelled);
        assert_eq!(report.removed, 1);
        assert_eq!(report.not_tried, 2, "the folder and what came after it");
        assert_eq!(fake.at(&at("after.txt")), Some(Node::File(Vec::new())));

        // A file is stopped the same way.
        let report = remove(&fake, &[item("after.txt", FileKind::File)]).await;
        assert_eq!(report.ended.as_deref(), Some(words));
        assert_eq!((report.removed, report.not_tried), (0, 1));
    }
}

/// A revocation cancels the removal that runs: a cancel nobody asked for is
/// the end of the session, never the technician's cancel.
#[tokio::test]
async fn a_removal_the_session_cancelled_is_the_end_of_the_session() {
    struct Revoked(Fake);
    impl FileTarget for Revoked {
        async fn remove_tree(
            &self,
            _path: &str,
            _cancel: CancelToken,
            _progress: cretlink_console::ProgressSink<TreeRemovalProgress>,
        ) -> Result<cretlink_session::TreeRemovalReport, TargetError> {
            Ok(cretlink_session::TreeRemovalReport {
                outcome: JobOutcome::Cancelled,
                removed_files: 3,
                removed_dirs: 1,
                failed: Vec::new(),
                failed_more: 0,
                blocked: false,
                error: None,
            })
        }
        async fn list(
            &self,
            folder: &str,
            cancel: &CancelToken,
        ) -> Result<super::super::target::Listed, TargetError> {
            self.0.list(folder, cancel).await
        }
        async fn send(
            &self,
            send: cretlink_console::SendToFolder,
            cancel: CancelToken,
            progress: cretlink_console::ProgressSink<cretlink_session::TransferProgress>,
        ) -> Result<cretlink_mcp::UploadReport, TargetError> {
            self.0.send(send, cancel, progress).await
        }
        async fn download(
            &self,
            source: &str,
            dest: std::path::PathBuf,
            cancel: CancelToken,
            progress: cretlink_console::ProgressSink<cretlink_session::TransferProgress>,
        ) -> Result<cretlink_mcp::DownloadReport, TargetError> {
            self.0.download(source, dest, cancel, progress).await
        }
        async fn create_folder(&self, path: &str) -> Result<(), TargetError> {
            self.0.create_folder(path).await
        }
        async fn rename(&self, from: &str, to: &str) -> Result<(), TargetError> {
            self.0.rename(from, to).await
        }
        async fn delete(&self, path: &str) -> Result<(), TargetError> {
            self.0.delete(path).await
        }
        async fn part_left(&self, path: &str) -> Result<bool, TargetError> {
            self.0.part_left(path).await
        }
    }
    let report = delete(
        &Revoked(Fake::new()),
        1,
        DOCUMENTS,
        &[
            item("Big", FileKind::Dir),
            item("after.txt", FileKind::File),
        ],
        &CancelToken::new(),
        Told::default().sink(),
    )
    .await;
    assert_eq!(report.ended.as_deref(), Some(Ended::Stopped.words()));
    assert!(!report.cancelled);
    assert_eq!(
        report.removed, 0,
        "a folder that was not confirmed gone is not removed"
    );
    assert_eq!((report.removed_files, report.removed_dirs), (3, 1));
    assert_eq!(report.not_tried, 1);
    assert_eq!(report.failed.len(), 1);
}

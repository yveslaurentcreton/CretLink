//! Tests of the transfer queue (ST-M010-002), against a customer computer
//! held in memory ([`Fake`]): what is queued for a send and for a copy, a
//! name that is taken, folders file by file, what is left out and why, the
//! names of the customer computer that may not leave the chosen folder, a
//! cancel, a failure, and the end of the session.
//!
//! Nothing here waits: the fake computer answers at once, so a test that
//! awaits the queue has run it to its end.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use cretlink_protocol::ErrorCode;

use super::super::fake::{Fake, Node};
use super::*;

const DOCUMENTS: &str = "C:\\Users\\ann\\Documents";

/// A folder of this test on the technician computer, empty.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cretlink-app-transfers-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the folder");
    dir
}

fn file(dir: &Path, name: &str, len: usize) -> PathBuf {
    let path = dir.join(name);
    if let Some(above) = path.parent() {
        std::fs::create_dir_all(above).expect("create the folder");
    }
    std::fs::write(&path, vec![b'x'; len]).expect("write the file");
    path
}

/// The views a queue delivered, in order.
#[derive(Clone, Default)]
struct Delivered(Arc<Mutex<Vec<TransferView>>>);

impl Delivered {
    fn emit(&self) -> Emit {
        let into = Arc::clone(&self.0);
        Arc::new(move |views| into.lock().unwrap().extend(views))
    }

    /// The states the entry `name` went through, without repeats.
    fn states(&self, name: &str) -> Vec<TransferState> {
        let mut states: Vec<TransferState> = Vec::new();
        for view in self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|view| view.name == name)
        {
            if states.last() != Some(&view.state) {
                states.push(view.state);
            }
        }
        states
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Carries the queue to its end, as the worker of the application does.
async fn carry(transfers: &Transfers, fake: &Fake, delivered: &Delivered) {
    if let Some(epoch) = transfers.must_start() {
        let fake = fake.clone();
        run(
            transfers.clone(),
            epoch,
            move || Some(fake.clone()),
            delivered.emit(),
        )
        .await;
    }
}

/// What the window shows of the queue: each row's name, state and reason.
fn rows(transfers: &Transfers) -> Vec<(String, TransferState, Option<String>)> {
    transfers
        .snapshot()
        .into_iter()
        .map(|view| (view.name, view.state, view.reason))
        .collect()
}

fn row(transfers: &Transfers, name: &str) -> TransferView {
    transfers
        .snapshot()
        .into_iter()
        .find(|view| view.name == name)
        .unwrap_or_else(|| panic!("no row {name}: {:?}", rows(transfers)))
}

/// Offers `paths`, checks them against the documents folder and queues them
/// with `choices`.
async fn send(
    transfers: &Transfers,
    fake: &Fake,
    paths: Vec<PathBuf>,
    choices: &[(&str, Choice)],
) -> (Check, Queued) {
    let offer = transfers.offer(paths).offer;
    let check = transfers
        .check_send(fake, offer, DOCUMENTS)
        .await
        .expect("the folder is looked at");
    let choices: HashMap<String, Choice> = choices
        .iter()
        .map(|(name, choice)| ((*name).to_owned(), *choice))
        .collect();
    let (queued, _) = transfers
        .queue_send(offer, DOCUMENTS, &choices)
        .expect("the offer is queued");
    (check, queued)
}

fn selected(names: &[(&str, EntryKind)]) -> Vec<(String, EntryKind)> {
    names
        .iter()
        .map(|(name, kind)| ((*name).to_owned(), *kind))
        .collect()
}

// --- A send ---

/// A dropped file waits, runs with its progress and is done once it arrived;
/// the window never learns more of an offer than a number and names.
#[tokio::test]
async fn a_dropped_file_is_sent_and_done_only_when_it_arrived() {
    let dir = scratch("send-one");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let source = file(&dir, "setup.exe", 4500);

    let offer = transfers.offer(vec![source.clone()]);
    assert_eq!(
        serde_json::to_value(&offer).unwrap(),
        serde_json::json!({ "offer": offer.offer, "items": 1, "names": ["setup.exe"] })
    );
    let check = transfers
        .check_send(&fake, offer.offer, DOCUMENTS)
        .await
        .unwrap();
    assert!(check.conflicts.is_empty());
    let (queued, views) = transfers
        .queue_send(offer.offer, DOCUMENTS, &HashMap::new())
        .unwrap();
    assert_eq!(
        queued,
        Queued {
            queued: 1,
            left_out: 0,
            skipped: 0
        }
    );
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, TransferState::Waiting);
    assert_eq!(views[0].bytes, Some(4500));
    assert_eq!(views[0].remote, format!("{DOCUMENTS}\\setup.exe"));
    assert_eq!(views[0].local, source.display().to_string());
    assert_eq!(fake.at(&format!("{DOCUMENTS}\\setup.exe")), None);

    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        delivered.states("setup.exe"),
        [TransferState::Running, TransferState::Done]
    );
    let done = row(&transfers, "setup.exe");
    assert_eq!(
        (done.done, done.bytes, done.reason),
        (4500, Some(4500), None)
    );
    assert_eq!(
        fake.at(&format!("{DOCUMENTS}\\setup.exe")),
        Some(Node::File(vec![b'x'; 4500]))
    );
    // The offer is used up: it cannot be sent a second time.
    assert!(
        transfers
            .queue_send(offer.offer, DOCUMENTS, &HashMap::new())
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The contract of the window: an entry of the queue and a check.
#[tokio::test]
async fn an_entry_and_a_check_are_the_contract_of_the_window() {
    let dir = scratch("contract");
    let fake = Fake::new();
    fake.put(&format!("{DOCUMENTS}\\report.pdf"), Node::File(vec![1]));
    let transfers = Transfers::default();
    let source = file(&dir, "report.pdf", 3);
    let (check, _) = send(
        &transfers,
        &fake,
        vec![source.clone()],
        &[("report.pdf", Choice::Replace)],
    )
    .await;
    assert_eq!(
        serde_json::to_value(&check).unwrap(),
        serde_json::json!({
            "folder": DOCUMENTS,
            "conflicts": [{ "name": "report.pdf", "kind": "file", "existing": "file", "replace": true }],
        })
    );
    let view = row(&transfers, "report.pdf");
    assert_eq!(
        serde_json::to_value(&view).unwrap(),
        serde_json::json!({
            "id": view.id,
            "direction": "send",
            "kind": "file",
            "name": "report.pdf",
            "remote": format!("{DOCUMENTS}\\report.pdf"),
            "local": source.display().to_string(),
            "bytes": 3,
            "done": 0,
            "state": "waiting",
        })
    );
    let choice: Choice = serde_json::from_str("\"keep_both\"").unwrap();
    assert_eq!(choice, Choice::KeepBoth);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that is taken is asked about, and each choice does what it says:
/// replace, keep both under the next free name, or skip. Without a choice
/// nothing is copied and nothing is replaced.
#[tokio::test]
async fn a_name_that_is_taken_is_replaced_kept_beside_or_skipped_as_chosen() {
    let dir = scratch("send-taken");
    let fake = Fake::new();
    for name in [
        "report.pdf",
        "notes.txt",
        "notes (2).txt",
        "skip.txt",
        "unasked.txt",
    ] {
        fake.put(&format!("{DOCUMENTS}\\{name}"), Node::File(b"old".to_vec()));
    }
    fake.folder(&format!("{DOCUMENTS}\\Photos"));
    fake.put(
        &format!("{DOCUMENTS}\\is-a-file"),
        Node::File(b"old".to_vec()),
    );
    std::fs::create_dir_all(dir.join("is-a-file")).unwrap();
    std::fs::create_dir_all(dir.join("Photos")).unwrap();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let paths = vec![
        // Asked for in another letter case than the folder has it.
        file(&dir, "REPORT.pdf", 10),
        file(&dir, "notes.txt", 20),
        file(&dir, "skip.txt", 30),
        file(&dir, "unasked.txt", 40),
        file(&dir, "new.txt", 50),
        dir.join("is-a-file"),
        dir.join("Photos"),
    ];
    let (check, queued) = send(
        &transfers,
        &fake,
        paths,
        &[
            ("REPORT.pdf", Choice::Replace),
            ("notes.txt", Choice::KeepBoth),
            ("skip.txt", Choice::Skip),
            ("is-a-file", Choice::Replace),
            ("Photos", Choice::Replace),
        ],
    )
    .await;
    let asked: Vec<(&str, bool)> = check
        .conflicts
        .iter()
        .map(|conflict| (conflict.name.as_str(), conflict.replace))
        .collect();
    assert_eq!(
        asked,
        [
            ("REPORT.pdf", true),
            ("notes.txt", true),
            ("skip.txt", true),
            ("unasked.txt", true),
            // A folder cannot replace a file.
            ("is-a-file", false),
            ("Photos", true),
        ]
    );
    assert_eq!(
        queued,
        Queued {
            queued: 4,
            left_out: 2,
            skipped: 1
        }
    );

    carry(&transfers, &fake, &delivered).await;
    let rows = rows(&transfers);
    let state = |name: &str| {
        rows.iter()
            .find(|(row, _, _)| row == name)
            .map(|(_, state, reason)| (*state, reason.clone()))
    };
    assert_eq!(state("REPORT.pdf"), Some((TransferState::Done, None)));
    assert_eq!(state("notes (3).txt"), Some((TransferState::Done, None)));
    assert_eq!(state("new.txt"), Some((TransferState::Done, None)));
    assert_eq!(state("skip.txt"), None, "a skipped item has no row");
    assert_eq!(
        state("unasked.txt"),
        Some((TransferState::LeftOut, Some(NEEDS_A_CHOICE.to_owned())))
    );
    assert_eq!(
        state("is-a-file"),
        Some((TransferState::LeftOut, Some(KINDS_DIFFER.to_owned())))
    );
    // An empty folder that goes into one that is there: nothing to make.
    assert_eq!(state("Photos"), Some((TransferState::Done, None)));

    let content = |name: &str| fake.at(&format!("{DOCUMENTS}\\{name}"));
    // Replaced on request, under the name the folder had it by.
    assert_eq!(content("report.pdf"), Some(Node::File(vec![b'x'; 10])));
    assert_eq!(content("REPORT.pdf"), None);
    // Kept beside: the next free name, and what was there is untouched.
    assert_eq!(content("notes.txt"), Some(Node::File(b"old".to_vec())));
    assert_eq!(content("notes (2).txt"), Some(Node::File(b"old".to_vec())));
    assert_eq!(content("notes (3).txt"), Some(Node::File(vec![b'x'; 20])));
    assert_eq!(content("skip.txt"), Some(Node::File(b"old".to_vec())));
    assert_eq!(content("unasked.txt"), Some(Node::File(b"old".to_vec())));
    assert_eq!(content("is-a-file"), Some(Node::File(b"old".to_vec())));
    assert_eq!(content("new.txt"), Some(Node::File(vec![b'x'; 50])));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that turns out to be taken although nobody was asked — the folder
/// changed after the look — fails its transfer and replaces nothing.
#[tokio::test]
async fn a_file_that_appeared_after_the_look_is_never_replaced() {
    let dir = scratch("send-raced");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let offer = transfers.offer(vec![file(&dir, "late.txt", 10)]).offer;
    let check = transfers.check_send(&fake, offer, DOCUMENTS).await.unwrap();
    assert!(check.conflicts.is_empty());
    fake.put(
        &format!("{DOCUMENTS}\\late.txt"),
        Node::File(b"theirs".to_vec()),
    );
    transfers
        .queue_send(offer, DOCUMENTS, &HashMap::new())
        .unwrap();
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        rows(&transfers),
        [(
            "late.txt".to_owned(),
            TransferState::Failed,
            Some("The destination already exists.".to_owned())
        )]
    );
    assert_eq!(
        fake.at(&format!("{DOCUMENTS}\\late.txt")),
        Some(Node::File(b"theirs".to_vec()))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An offer is sent to the folder it was checked against, and to no other.
#[tokio::test]
async fn an_offer_is_queued_only_for_the_folder_it_was_checked_against() {
    let dir = scratch("send-unchecked");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let offer = transfers.offer(vec![file(&dir, "a.txt", 1)]).offer;
    assert!(!transfers.checked(offer, DOCUMENTS));
    assert!(
        transfers
            .queue_send(offer, DOCUMENTS, &HashMap::new())
            .is_err()
    );
    transfers.check_send(&fake, offer, DOCUMENTS).await.unwrap();
    assert!(transfers.checked(offer, DOCUMENTS));
    assert!(
        transfers
            .queue_send(offer, "C:\\Users\\ann", &HashMap::new())
            .is_err()
    );
    assert!(transfers.snapshot().is_empty());
    // An offer nobody kept is refused in words.
    match transfers.check_send(&fake, offer + 100, DOCUMENTS).await {
        Err(CheckError::Refused(Refused(why))) => assert_eq!(why, OFFER_GONE),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A dropped folder goes file by file: its folders are made on the way, an
/// empty one too, each file is a transfer of its own, and the folders that
/// were made are no rows of the queue.
#[tokio::test]
async fn a_dropped_folder_goes_file_by_file_and_its_folders_are_made() {
    let dir = scratch("send-folder");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let top = dir.join("Project");
    file(&top, "readme.txt", 10);
    file(&top.join("src"), "main.rs", 2500);
    file(&top.join("src").join("deep"), "lib.rs", 30);
    std::fs::create_dir_all(top.join("empty")).unwrap();

    let (_, queued) = send(&transfers, &fake, vec![top], &[]).await;
    assert_eq!(queued.queued, 3);
    carry(&transfers, &fake, &delivered).await;

    let mut done: Vec<(String, TransferState)> = rows(&transfers)
        .into_iter()
        .map(|(name, state, _)| (name, state))
        .collect();
    done.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        done,
        [
            ("Project\\readme.txt".to_owned(), TransferState::Done),
            ("Project\\src\\deep\\lib.rs".to_owned(), TransferState::Done),
            ("Project\\src\\main.rs".to_owned(), TransferState::Done),
        ]
    );
    let project = format!("{DOCUMENTS}\\Project");
    assert_eq!(fake.names(&project), ["empty", "readme.txt", "src"]);
    assert_eq!(
        fake.names(&format!("{project}\\empty")),
        Vec::<String>::new()
    );
    assert_eq!(fake.names(&format!("{project}\\src")), ["deep", "main.rs"]);
    assert_eq!(
        fake.at(&format!("{project}\\src\\deep\\lib.rs")),
        Some(Node::File(vec![b'x'; 30]))
    );
    // A folder is made before anything is sent into it.
    let asked = fake.asked();
    let place = |what: &str| asked.iter().position(|asked| asked == what).unwrap();
    assert!(
        place(&format!("create_folder {project}\\src"))
            < place(&format!("send {project}\\src\\main.rs"))
    );
    assert!(
        place(&format!("create_folder {project}"))
            < place(&format!("create_folder {project}\\src"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A dropped folder that holds nothing is made, and has a row that says so.
#[tokio::test]
async fn an_empty_dropped_folder_is_made_and_has_a_row() {
    let dir = scratch("send-empty-folder");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    std::fs::create_dir_all(dir.join("Empty")).unwrap();
    send(&transfers, &fake, vec![dir.join("Empty")], &[]).await;
    assert_eq!(row(&transfers, "Empty").kind, EntryKind::Folder);
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        delivered.states("Empty"),
        [TransferState::Running, TransferState::Done]
    );
    assert_eq!(fake.at(&format!("{DOCUMENTS}\\Empty")), Some(Node::Folder));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder sent into one that is there, on request: its files replace those
/// of the same name, what else is there stays, and a file that stands where a
/// folder must be made stops what would go into it, with the reason.
#[tokio::test]
async fn a_folder_goes_into_the_one_that_is_there_only_on_request() {
    let dir = scratch("send-merge");
    let fake = Fake::new();
    let photos = format!("{DOCUMENTS}\\Photos");
    fake.folder(&photos);
    fake.folder(&format!("{photos}\\2026"));
    fake.put(&format!("{photos}\\a.jpg"), Node::File(b"old".to_vec()));
    fake.put(&format!("{photos}\\keep.jpg"), Node::File(b"keep".to_vec()));
    fake.put(
        &format!("{photos}\\blocked"),
        Node::File(b"a file".to_vec()),
    );
    let top = dir.join("Photos");
    file(&top, "a.jpg", 10);
    file(&top.join("2026"), "b.jpg", 20);
    file(&top.join("blocked"), "c.jpg", 30);
    file(&top.join("new"), "d.jpg", 40);

    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let (check, _) = send(
        &transfers,
        &fake,
        vec![top.clone()],
        &[("Photos", Choice::Replace)],
    )
    .await;
    assert_eq!(check.conflicts.len(), 1);
    assert!(check.conflicts[0].replace);
    carry(&transfers, &fake, &delivered).await;

    assert_eq!(
        fake.at(&format!("{photos}\\a.jpg")),
        Some(Node::File(vec![b'x'; 10]))
    );
    assert_eq!(
        fake.at(&format!("{photos}\\keep.jpg")),
        Some(Node::File(b"keep".to_vec()))
    );
    assert_eq!(
        fake.at(&format!("{photos}\\2026\\b.jpg")),
        Some(Node::File(vec![b'x'; 20]))
    );
    assert_eq!(
        fake.at(&format!("{photos}\\new\\d.jpg")),
        Some(Node::File(vec![b'x'; 40]))
    );
    assert_eq!(
        fake.at(&format!("{photos}\\blocked")),
        Some(Node::File(b"a file".to_vec()))
    );
    // The folders that were there were not made again.
    assert!(!fake.asked().contains(&format!("create_folder {photos}")));
    assert!(
        !fake
            .asked()
            .contains(&format!("create_folder {photos}\\2026"))
    );
    assert!(
        fake.asked()
            .contains(&format!("create_folder {photos}\\new"))
    );

    // The folder that could not be made is a row now, and so is its file.
    let blocked = row(&transfers, "Photos\\blocked");
    assert_eq!(blocked.state, TransferState::Failed);
    assert_eq!(
        blocked.reason.as_deref(),
        Some("A file of that name is in the way. The folder was not made.")
    );
    let inside = row(&transfers, "Photos\\blocked\\c.jpg");
    assert_eq!(inside.state, TransferState::Failed);
    assert!(
        inside.reason.as_deref().is_some_and(|reason| reason
            .starts_with(&format!("Its folder {photos}\\blocked could not be made."))),
        "{:?}",
        inside.reason
    );

    // Without the request the folder is not gone into: kept beside instead.
    let transfers = Transfers::default();
    send(
        &transfers,
        &fake,
        vec![top],
        &[("Photos", Choice::KeepBoth)],
    )
    .await;
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        fake.at(&format!("{DOCUMENTS}\\Photos (2)\\a.jpg")),
        Some(Node::File(vec![b'x'; 10]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A link in a dropped folder is left out and says so; a link that is
/// dropped itself, too. Nothing a link leads to is sent.
#[cfg(unix)]
#[tokio::test]
async fn a_link_is_left_out_of_a_send_and_says_so() {
    let dir = scratch("send-links");
    let outside = scratch("send-links-outside");
    file(&outside, "secret.txt", 10);
    let top = dir.join("Folder");
    file(&top, "a.txt", 10);
    std::os::unix::fs::symlink(&outside, top.join("to-outside")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("dropped-link")).unwrap();

    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let (_, queued) = send(&transfers, &fake, vec![top, dir.join("dropped-link")], &[]).await;
    assert_eq!((queued.queued, queued.left_out), (1, 2));
    carry(&transfers, &fake, &delivered).await;
    let mut rows = rows(&transfers);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        [
            ("Folder\\a.txt".to_owned(), TransferState::Done, None),
            (
                "Folder\\to-outside".to_owned(),
                TransferState::LeftOut,
                Some(LINK_LEFT_OUT.to_owned())
            ),
            (
                "dropped-link".to_owned(),
                TransferState::LeftOut,
                Some(LINK_LEFT_OUT.to_owned())
            ),
        ]
    );
    assert!(fake.asked().iter().all(|asked| !asked.contains("secret")));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

/// A transfer that fails says why in the application's words, and the ones
/// after it run: a full disk, a file in use, a file of this computer that is
/// gone.
#[tokio::test]
async fn a_transfer_that_fails_says_why_and_the_next_one_runs() {
    let dir = scratch("send-fails");
    let fake = Fake::new();
    fake.refuse(
        &format!("{DOCUMENTS}\\full.bin"),
        ErrorCode::StorageExhausted,
    );
    fake.refuse(&format!("{DOCUMENTS}\\open.docx"), ErrorCode::FileInUse);
    fake.refuse(&format!("{DOCUMENTS}\\denied.txt"), ErrorCode::AccessDenied);
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let gone = file(&dir, "gone.txt", 10);
    send(
        &transfers,
        &fake,
        vec![
            file(&dir, "full.bin", 10),
            file(&dir, "open.docx", 10),
            file(&dir, "denied.txt", 10),
            gone.clone(),
            file(&dir, "fine.txt", 10),
        ],
        &[],
    )
    .await;
    std::fs::remove_file(&gone).unwrap();
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        rows(&transfers),
        [
            (
                "full.bin".to_owned(),
                TransferState::Failed,
                Some("The disk is full.".to_owned())
            ),
            (
                "open.docx".to_owned(),
                TransferState::Failed,
                Some("The file is in use.".to_owned())
            ),
            (
                "denied.txt".to_owned(),
                TransferState::Failed,
                Some("Access was denied.".to_owned())
            ),
            (
                "gone.txt".to_owned(),
                TransferState::Failed,
                Some(format!(
                    "Cannot read {} on the technician computer: the file does not exist.",
                    gone.display()
                ))
            ),
            ("fine.txt".to_owned(), TransferState::Done, None),
        ]
    );
    // A failed transfer never reads as done on its way either.
    for name in ["full.bin", "open.docx", "denied.txt", "gone.txt"] {
        assert!(
            !delivered.states(name).contains(&TransferState::Done),
            "{name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Cancel ---

/// A waiting transfer is cancelled at once and never begins. A running one
/// ends as cancelled, never as done, and leaves no part behind; when the
/// customer computer could not remove the part, its row says so.
#[tokio::test]
async fn a_cancelled_transfer_never_reads_as_done_and_says_what_it_left() {
    let dir = scratch("send-cancel");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    send(
        &transfers,
        &fake,
        vec![
            file(&dir, "big.bin", 9000),
            file(&dir, "second.bin", 9000),
            file(&dir, "third.bin", 10),
        ],
        &[],
    )
    .await;
    let id_of = |name: &str| row(&transfers, name).id;
    // The third is cancelled while it waits.
    let changed = transfers.cancel(id_of("third.bin"));
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].state, TransferState::Cancelled);

    // The first is cancelled on its way, the second too; before the second
    // the customer computer stops removing parts.
    let (queue, computer, big, second) = (
        transfers.clone(),
        fake.clone(),
        id_of("big.bin"),
        id_of("second.bin"),
    );
    fake.on_progress(Box::new(move |path, done| {
        if done == 3000 {
            if path.ends_with("second.bin") {
                computer.keep_parts();
                assert!(
                    queue.cancel(second).is_empty(),
                    "the worker says how it ended"
                );
            } else {
                assert!(queue.cancel(big).is_empty());
            }
        }
    }));
    carry(&transfers, &fake, &delivered).await;

    assert_eq!(
        delivered.states("big.bin"),
        [TransferState::Running, TransferState::Cancelled]
    );
    let big = row(&transfers, "big.bin");
    assert_eq!(big.reason, None, "its part is gone: nothing to say");
    assert!(big.done >= 3000 && big.done < 9000, "{}", big.done);
    assert!(!fake.has_part(&format!("{DOCUMENTS}\\big.bin")));
    assert_eq!(fake.at(&format!("{DOCUMENTS}\\big.bin")), None);

    let second = row(&transfers, "second.bin");
    assert_eq!(second.state, TransferState::Cancelled);
    assert!(
        second
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("Its unfinished part is still in the folder")),
        "{:?}",
        second.reason
    );
    assert!(fake.has_part(&format!("{DOCUMENTS}\\second.bin")));

    assert_eq!(delivered.states("third.bin"), Vec::<TransferState>::new());
    assert_eq!(row(&transfers, "third.bin").state, TransferState::Cancelled);
    assert!(!fake.asked().iter().any(|asked| asked.contains("third.bin")));
    // A cancel of what has ended changes nothing.
    assert!(transfers.cancel(big.id).is_empty());
    assert!(transfers.cancel(987_654).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cancel all: the running transfer and every waiting one, folders that
/// were still to be made included; nothing more is asked afterwards.
#[tokio::test]
async fn cancelling_all_ends_the_running_and_every_waiting_transfer() {
    let dir = scratch("send-cancel-all");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let top = dir.join("Many");
    for n in 0..5 {
        file(&top.join(format!("sub-{n}")), "f.bin", 5000);
    }
    send(&transfers, &fake, vec![top], &[]).await;
    let queue = transfers.clone();
    fake.on_progress(Box::new(move |_, done| {
        if done == 2000 {
            let changed = queue.cancel_all();
            // The waiting files are cancelled at once; the running one and
            // the folders that are no rows are not among what changed.
            assert_eq!(changed.len(), 4, "{changed:?}");
        }
    }));
    carry(&transfers, &fake, &delivered).await;
    let states: Vec<TransferState> = rows(&transfers)
        .into_iter()
        .map(|(_, state, _)| state)
        .collect();
    assert_eq!(states, [TransferState::Cancelled; 5]);
    assert_eq!(
        fake.asked()
            .iter()
            .filter(|asked| asked.starts_with("send "))
            .count(),
        1,
        "only the transfer that ran was asked for"
    );
    // The worker ended, and a new send starts a new one.
    send(&transfers, &fake, vec![file(&dir, "after.txt", 10)], &[]).await;
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(row(&transfers, "after.txt").state, TransferState::Done);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- The end of the session ---

/// A stop or a revocation ends the running transfer and every waiting one:
/// each says so, none is done, and nothing more is asked.
#[tokio::test]
async fn the_end_of_the_session_stops_the_running_and_every_waiting_transfer() {
    for (error, words) in [
        (TargetError::NoConsentedSession, Ended::Stopped.words()),
        (TargetError::Disconnected, Ended::Lost.words()),
    ] {
        let dir = scratch("send-ended");
        let fake = Fake::new();
        let transfers = Transfers::default();
        let delivered = Delivered::default();
        send(
            &transfers,
            &fake,
            vec![
                file(&dir, "first.bin", 10),
                file(&dir, "running.bin", 9000),
                file(&dir, "waiting.bin", 10),
            ],
            &[],
        )
        .await;
        let (computer, ended) = (fake.clone(), error.clone());
        fake.on_progress(Box::new(move |path, done| {
            if path.ends_with("running.bin") && done == 2000 {
                computer.end(ended.clone());
            }
        }));
        carry(&transfers, &fake, &delivered).await;
        assert_eq!(
            rows(&transfers),
            [
                ("first.bin".to_owned(), TransferState::Done, None),
                (
                    "running.bin".to_owned(),
                    TransferState::Stopped,
                    Some(words.to_owned())
                ),
                (
                    "waiting.bin".to_owned(),
                    TransferState::Stopped,
                    Some(words.to_owned())
                ),
            ]
        );
        assert!(
            !fake
                .asked()
                .iter()
                .any(|asked| asked.contains("waiting.bin"))
        );
        assert_eq!(transfers.must_start(), None, "nothing waits any more");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A revocation cancels the transfer that runs: a cancel nobody asked for is
/// the end of the session, never the technician's cancel.
#[tokio::test]
async fn a_transfer_the_session_cancelled_is_stopped_not_cancelled() {
    let dir = scratch("send-revoked");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    send(
        &transfers,
        &fake,
        vec![
            file(&dir, "running.bin", 9000),
            file(&dir, "waiting.bin", 10),
        ],
        &[],
    )
    .await;
    // What the session's stop does: it cancels the token of the transfer.
    let queue = transfers.clone();
    fake.on_progress(Box::new(move |_, done| {
        if done == 2000 {
            let guard = queue.lock();
            guard.entries[0].cancel.cancel();
        }
    }));
    carry(&transfers, &fake, &delivered).await;
    let states: Vec<TransferState> = rows(&transfers)
        .into_iter()
        .map(|(_, state, _)| state)
        .collect();
    assert_eq!(states, [TransferState::Stopped, TransferState::Stopped]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a session under full control nothing runs: the queue stops where
/// it stands.
#[tokio::test]
async fn without_a_session_the_queue_stops_before_anything_is_asked() {
    let dir = scratch("send-no-session");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    send(&transfers, &fake, vec![file(&dir, "a.txt", 10)], &[]).await;
    let before = fake.asked().len();
    let epoch = transfers.must_start().unwrap();
    run(transfers.clone(), epoch, || None::<Fake>, delivered.emit()).await;
    assert_eq!(row(&transfers, "a.txt").state, TransferState::Stopped);
    assert_eq!(fake.asked().len(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new session starts with an empty queue, and a number of the session
/// before it reaches nothing.
#[tokio::test]
async fn a_new_session_starts_with_an_empty_queue() {
    let dir = scratch("reset");
    let fake = Fake::new();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    send(&transfers, &fake, vec![file(&dir, "a.txt", 10)], &[]).await;
    let old = row(&transfers, "a.txt").id;
    let offer = transfers.offer(vec![dir.join("a.txt")]).offer;
    let chosen = transfers.choose(dir.clone());
    let epoch = transfers.must_start().unwrap();

    transfers.reset();
    assert!(transfers.snapshot().is_empty());
    assert!(transfers.cancel(old).is_empty());
    assert_eq!(transfers.chosen(chosen), None);
    assert!(transfers.check_send(&fake, offer, DOCUMENTS).await.is_err());
    // The worker of the session before finds nothing of its own.
    run(
        transfers.clone(),
        epoch,
        move || Some(Fake::new()),
        delivered.emit(),
    )
    .await;
    assert_eq!(delivered.count(), 0);
    // Numbers go on: a new entry never answers to an old number.
    send(&transfers, &fake, vec![file(&dir, "b.txt", 10)], &[]).await;
    assert!(row(&transfers, "b.txt").id > old);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- A copy to this computer ---

/// A selected file is copied below the chosen folder, which is made when the
/// first file goes there, and its folder can be shown once it arrived.
#[tokio::test]
async fn a_selected_file_is_copied_into_the_chosen_folder() {
    let dir = scratch("download-one");
    let root = dir.join("Downloads").join("CretLink").join("CUSTOMER-PC");
    let fake = Fake::new();
    fake.put(
        &format!("{DOCUMENTS}\\report.pdf"),
        Node::File(vec![7; 3200]),
    );
    let transfers = Transfers::default();
    let delivered = Delivered::default();

    let names = selected(&[("report.pdf", EntryKind::File)]);
    assert!(check_download(&root, &names).conflicts.is_empty());
    assert!(!root.exists(), "a look makes no folder");
    let (queued, views) = transfers
        .queue_download(DOCUMENTS, &names, &root, &HashMap::new())
        .unwrap();
    assert_eq!(queued.queued, 1);
    assert_eq!(views[0].direction, Direction::Download);
    assert_eq!(
        views[0].local,
        root.join("report.pdf").display().to_string()
    );
    assert!(!root.exists(), "nothing is made before the copy begins");
    assert_eq!(transfers.folder_of(views[0].id), None, "it has not arrived");

    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        delivered.states("report.pdf"),
        [TransferState::Running, TransferState::Done]
    );
    assert_eq!(
        std::fs::read(root.join("report.pdf")).unwrap(),
        vec![7; 3200]
    );
    let done = row(&transfers, "report.pdf");
    assert_eq!((done.bytes, done.done), (Some(3200), 3200));
    assert_eq!(transfers.folder_of(done.id), Some(root.clone()));
    // What it is came from a listing this side asked for.
    assert_eq!(fake.asked()[0], format!("list {DOCUMENTS}"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A selected folder is walked through its listings: its folders are made
/// here, an empty one too, its files are transfers of their own, and a link
/// in it is left out and says so. The folder's own row gives way to its
/// files.
#[tokio::test]
async fn a_selected_folder_is_copied_file_by_file_and_a_link_is_left_out() {
    let dir = scratch("download-folder");
    let root = dir.join("to");
    let fake = Fake::new();
    let photos = format!("{DOCUMENTS}\\Photos");
    fake.folder(&photos);
    fake.folder(&format!("{photos}\\2026"));
    fake.folder(&format!("{photos}\\empty"));
    fake.put(&format!("{photos}\\a.jpg"), Node::File(vec![1; 10]));
    fake.put(&format!("{photos}\\2026\\b.jpg"), Node::File(vec![2; 2500]));
    fake.put(&format!("{photos}\\shortcut"), Node::Link);
    fake.put(&format!("{photos}\\device"), Node::Other);
    // Behind the link lies something that must never be asked for.
    fake.folder(&format!("{photos}\\shortcut"));
    fake.put(
        &format!("{photos}\\shortcut\\secret.txt"),
        Node::File(vec![9; 5]),
    );
    fake.put(&format!("{photos}\\shortcut"), Node::Link);

    let transfers = Transfers::default();
    let delivered = Delivered::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[("Photos", EntryKind::Folder)]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    carry(&transfers, &fake, &delivered).await;

    assert_eq!(
        delivered.states("Photos"),
        [TransferState::Running, TransferState::Listed],
        "the folder's row gives way to its files"
    );
    let mut rows = rows(&transfers);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        [
            ("Photos\\2026\\b.jpg".to_owned(), TransferState::Done, None),
            ("Photos\\a.jpg".to_owned(), TransferState::Done, None),
            (
                "Photos\\device".to_owned(),
                TransferState::LeftOut,
                Some(NOT_A_FILE_OR_FOLDER.to_owned())
            ),
            (
                "Photos\\shortcut".to_owned(),
                TransferState::LeftOut,
                Some(LINK_LEFT_OUT.to_owned())
            ),
        ]
    );
    assert_eq!(
        std::fs::read(root.join("Photos").join("a.jpg")).unwrap(),
        vec![1; 10]
    );
    assert_eq!(
        std::fs::read(root.join("Photos").join("2026").join("b.jpg")).unwrap(),
        vec![2; 2500]
    );
    assert!(root.join("Photos").join("empty").is_dir());
    assert!(!root.join("Photos").join("shortcut").exists());
    assert!(
        !fake.asked().iter().any(|asked| asked.contains("shortcut")),
        "nothing behind the link was asked for: {:?}",
        fake.asked()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the window says a name is does not decide what is done with it: a
/// link that the window calls a folder is not walked, and a folder that it
/// calls a file is copied as the folder it is.
#[tokio::test]
async fn what_a_selected_name_is_comes_from_the_listing_not_from_the_window() {
    let dir = scratch("download-kinds");
    let root = dir.join("to");
    let fake = Fake::new();
    fake.put(&format!("{DOCUMENTS}\\link"), Node::Link);
    fake.folder(&format!("{DOCUMENTS}\\folder"));
    fake.put(
        &format!("{DOCUMENTS}\\folder\\in.txt"),
        Node::File(vec![1; 4]),
    );
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[
                ("link", EntryKind::Folder),
                ("folder", EntryKind::File),
                ("gone.txt", EntryKind::File),
            ]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        rows(&transfers),
        [
            (
                "link".to_owned(),
                TransferState::LeftOut,
                Some(LINK_LEFT_OUT.to_owned())
            ),
            (
                "gone.txt".to_owned(),
                TransferState::Failed,
                Some("It is no longer in the folder.".to_owned())
            ),
            ("folder\\in.txt".to_owned(), TransferState::Done, None),
        ]
    );
    assert_eq!(
        fake.asked()
            .iter()
            .filter(|asked| *asked == &format!("list {DOCUMENTS}"))
            .count(),
        1,
        "the open folder is listed once for the whole selection"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing a customer computer names lands outside the chosen folder: a name
/// with a separator, a step up, a drive, a device or a stream is left out
/// and says why, at the top of a selection and inside a folder.
#[tokio::test]
async fn names_that_try_to_leave_the_chosen_folder_are_left_out() {
    let dir = scratch("download-hostile");
    let root = dir.join("chosen").join("CUSTOMER-PC");
    let fake = Fake::new();
    let hostile = [
        "..",
        "..\\..\\evil.txt",
        "../../evil.txt",
        "sub\\evil.txt",
        "/abs.txt",
        "C:\\Windows\\evil.txt",
        "C:evil.txt",
        "evil.txt:stream",
        "NUL",
        "con.txt",
        "trailing.",
        "evil.cretlink-part",
    ];
    let folder = format!("{DOCUMENTS}\\Sent");
    fake.folder(&folder);
    {
        // The fake computer lists what a real one could not hold.
        let mut top = selected(&[]);
        for name in hostile {
            fake.put_raw(DOCUMENTS, name, Node::File(vec![6; 5]));
            fake.put_raw(&folder, name, Node::File(vec![6; 5]));
            top.push((name.to_owned(), EntryKind::File));
        }
        // A folder with a hostile name, and a file in it.
        fake.put_raw(&folder, "..\\up", Node::Folder);
        fake.put(&format!("{folder}\\fine.txt"), Node::File(vec![1; 5]));
        fake.put(&format!("{DOCUMENTS}\\fine.txt"), Node::File(vec![1; 5]));
        top.push(("fine.txt".to_owned(), EntryKind::File));
        top.push(("Sent".to_owned(), EntryKind::Folder));

        let transfers = Transfers::default();
        let delivered = Delivered::default();
        let (queued, _) = transfers
            .queue_download(DOCUMENTS, &top, &root, &HashMap::new())
            .unwrap();
        assert_eq!(queued.left_out, hostile.len() as u64);
        assert_eq!(queued.queued, 2);
        carry(&transfers, &fake, &delivered).await;

        let rows = rows(&transfers);
        for name in hostile {
            for shown in [name.to_owned(), format!("Sent\\{name}")] {
                let (_, state, reason) = rows
                    .iter()
                    .find(|(row, _, _)| *row == shown)
                    .unwrap_or_else(|| panic!("no row {shown}"));
                assert_eq!(*state, TransferState::LeftOut, "{shown}");
                assert!(
                    reason
                        .as_deref()
                        .is_some_and(|reason| reason.starts_with("Its name ")
                            && reason.ends_with("It was not copied.")),
                    "{shown}: {reason:?}"
                );
            }
        }
        assert_eq!(
            rows.iter()
                .filter(|(_, state, _)| *state == TransferState::Done)
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["fine.txt", "Sent\\fine.txt"]
        );
    }

    // What lies on this computer afterwards: the two files, in the chosen
    // folder, and nothing beside or above it.
    let mut found = Vec::new();
    let mut folders = vec![dir.clone()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    assert_eq!(
        found,
        [root.join("Sent").join("fine.txt"), root.join("fine.txt")]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A copy never writes where it was not led by plain names, whatever queued
/// it.
#[tokio::test]
async fn a_copy_whose_place_is_outside_the_chosen_folder_is_refused() {
    let dir = scratch("download-outside");
    let root = dir.join("chosen");
    std::fs::create_dir_all(&root).unwrap();
    let fake = Fake::new();
    fake.put(&format!("{DOCUMENTS}\\a.txt"), Node::File(vec![1; 5]));
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let (_, views) = transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[("a.txt", EntryKind::File)]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    for outside in [
        dir.join("elsewhere.txt"),
        root.join("..").join("up.txt"),
        root.clone(),
    ] {
        let step = download_file(
            &transfers,
            &fake,
            &delivered.emit(),
            views[0].id,
            CancelToken::new(),
            &format!("{DOCUMENTS}\\a.txt"),
            outside.clone(),
            &root,
            true,
        )
        .await;
        assert!(matches!(step, Step::Next));
        assert_eq!(
            row(&transfers, "a.txt").state,
            TransferState::Failed,
            "{outside:?}"
        );
        assert!(!dir.join("elsewhere.txt").exists() && !dir.join("up.txt").exists());
    }
    assert!(
        !fake
            .asked()
            .iter()
            .any(|asked| asked.starts_with("download"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that is taken on this computer asks the same way: replace, keep
/// both, skip; without a choice nothing is copied, and a file that appeared
/// after the look is not replaced.
#[tokio::test]
async fn a_name_that_is_taken_here_is_replaced_kept_beside_or_skipped_as_chosen() {
    let dir = scratch("download-taken");
    let root = dir.join("to");
    for name in [
        "replace.txt",
        "both.txt",
        "both (2).txt",
        "skip.txt",
        "unasked.txt",
    ] {
        file(&root, name, 3);
    }
    file(&root.join("Folder"), "old.txt", 3);
    file(&root, "is-a-file", 3);
    let fake = Fake::new();
    for name in [
        "replace.txt",
        "both.txt",
        "skip.txt",
        "unasked.txt",
        "late.txt",
    ] {
        fake.put(&format!("{DOCUMENTS}\\{name}"), Node::File(vec![5; 8]));
    }
    fake.folder(&format!("{DOCUMENTS}\\Folder"));
    fake.put(
        &format!("{DOCUMENTS}\\Folder\\old.txt"),
        Node::File(vec![5; 8]),
    );
    fake.put(
        &format!("{DOCUMENTS}\\Folder\\new.txt"),
        Node::File(vec![5; 8]),
    );
    fake.folder(&format!("{DOCUMENTS}\\is-a-file"));

    let names = selected(&[
        ("replace.txt", EntryKind::File),
        ("both.txt", EntryKind::File),
        ("skip.txt", EntryKind::File),
        ("unasked.txt", EntryKind::File),
        ("late.txt", EntryKind::File),
        ("Folder", EntryKind::Folder),
        ("is-a-file", EntryKind::Folder),
    ]);
    let check = check_download(&root, &names);
    assert_eq!(check.folder, root.display().to_string());
    assert_eq!(
        check
            .conflicts
            .iter()
            .map(|conflict| (conflict.name.as_str(), conflict.replace))
            .collect::<Vec<_>>(),
        [
            ("replace.txt", true),
            ("both.txt", true),
            ("skip.txt", true),
            ("unasked.txt", true),
            ("Folder", true),
            ("is-a-file", false),
        ]
    );
    let choices: HashMap<String, Choice> = [
        ("replace.txt", Choice::Replace),
        ("both.txt", Choice::KeepBoth),
        ("skip.txt", Choice::Skip),
        ("Folder", Choice::Replace),
        ("is-a-file", Choice::Replace),
    ]
    .into_iter()
    .map(|(name, choice)| (name.to_owned(), choice))
    .collect();
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    let (queued, _) = transfers
        .queue_download(DOCUMENTS, &names, &root, &choices)
        .unwrap();
    assert_eq!((queued.skipped, queued.left_out), (1, 1));
    // It appears on this computer after the look.
    file(&root, "late.txt", 3);
    carry(&transfers, &fake, &delivered).await;

    let read = |name: &str| std::fs::read(root.join(name)).unwrap();
    assert_eq!(read("replace.txt"), vec![5; 8]);
    assert_eq!(read("both.txt"), vec![b'x'; 3]);
    assert_eq!(read("both (2).txt"), vec![b'x'; 3]);
    assert_eq!(read("both (3).txt"), vec![5; 8]);
    assert_eq!(read("skip.txt"), vec![b'x'; 3]);
    assert_eq!(read("unasked.txt"), vec![b'x'; 3]);
    assert_eq!(read("late.txt"), vec![b'x'; 3], "never replaced unasked");
    assert_eq!(
        std::fs::read(root.join("Folder").join("old.txt")).unwrap(),
        vec![5; 8]
    );
    assert_eq!(
        std::fs::read(root.join("Folder").join("new.txt")).unwrap(),
        vec![5; 8]
    );
    assert_eq!(read("is-a-file"), vec![b'x'; 3]);

    let rows = rows(&transfers);
    let state = |name: &str| {
        rows.iter()
            .find(|(row, _, _)| row == name)
            .map(|(_, state, reason)| (*state, reason.clone()))
    };
    assert_eq!(
        state("unasked.txt"),
        Some((TransferState::LeftOut, Some(NEEDS_A_CHOICE.to_owned())))
    );
    let late = state("late.txt").unwrap();
    assert_eq!(late.0, TransferState::Failed);
    assert!(
        late.1
            .as_deref()
            .is_some_and(|reason| reason.ends_with("is already there. It was not replaced.")),
        "{late:?}"
    );
    let blocked = state("is-a-file").unwrap();
    assert_eq!(blocked.0, TransferState::Failed);
    assert!(
        blocked
            .1
            .as_deref()
            .is_some_and(|reason| reason.contains("is in the way and is not a folder")),
        "{blocked:?}"
    );
    assert_eq!(state("skip.txt"), None);
    // The copy that was kept beside names where it went.
    assert_eq!(
        row(&transfers, "both.txt").local,
        root.join("both (3).txt").display().to_string()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cancelled copy is cancelled, never done; its part is removed from this
/// computer, and when it cannot be, the row says so. The end of the session
/// stops a copy the same way.
#[tokio::test]
async fn a_cancelled_or_stopped_copy_leaves_no_part_on_this_computer() {
    let dir = scratch("download-cancel");
    let root = dir.join("to");
    let fake = Fake::new();
    for name in ["big.bin", "second.bin", "third.bin"] {
        fake.put(&format!("{DOCUMENTS}\\{name}"), Node::File(vec![3; 9000]));
    }
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[
                ("big.bin", EntryKind::File),
                ("second.bin", EntryKind::File),
                ("third.bin", EntryKind::File),
            ]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    let (queue, computer) = (transfers.clone(), fake.clone());
    let ids: Vec<u64> = transfers.snapshot().iter().map(|view| view.id).collect();
    fake.on_progress(Box::new(move |path, done| {
        if done != 3000 {
            return;
        }
        if path.ends_with("big.bin") {
            queue.cancel(ids[0]);
        } else if path.ends_with("second.bin") {
            computer.end(TargetError::NoConsentedSession);
        }
    }));
    carry(&transfers, &fake, &delivered).await;
    assert_eq!(
        rows(&transfers),
        [
            ("big.bin".to_owned(), TransferState::Cancelled, None),
            (
                "second.bin".to_owned(),
                TransferState::Stopped,
                Some(Ended::Stopped.words().to_owned())
            ),
            (
                "third.bin".to_owned(),
                TransferState::Stopped,
                Some(Ended::Stopped.words().to_owned())
            ),
        ]
    );
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "neither a file nor a part is left"
    );

    // A part that cannot be removed is named.
    let blocked = root.join("stuck.bin");
    std::fs::create_dir_all(download_part_path(&blocked)).unwrap();
    let why = remove_part(&blocked)
        .await
        .expect("a folder is not removed as a file");
    assert!(
        why.starts_with(&format!(
            "Its unfinished part {} could not be removed:",
            download_part_path(&blocked).display()
        )),
        "{why}"
    );
    assert_eq!(remove_part(&root.join("never-began.bin")).await, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder that cannot be listed to its end is never shown as complete: a
/// listing that was cut, a subfolder that is refused, more files than one
/// copy queues. What was listed is queued, and the folder's row says what is
/// missing.
#[tokio::test]
async fn a_folder_that_is_not_walked_to_its_end_says_so() {
    let dir = scratch("download-bounds");
    let root = dir.join("to");
    let fake = Fake::new();
    let big = format!("{DOCUMENTS}\\Big");
    fake.folder(&big);
    fake.folder(&format!("{big}\\denied"));
    fake.refuse(&format!("{big}\\denied"), ErrorCode::AccessDenied);
    fake.folder(&format!("{big}\\cut"));
    fake.cut(&format!("{big}\\cut"));
    fake.put(&format!("{big}\\cut\\seen.txt"), Node::File(vec![1; 3]));
    fake.put(&format!("{big}\\a.txt"), Node::File(vec![1; 3]));

    let transfers = Transfers::default();
    let delivered = Delivered::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[("Big", EntryKind::Folder)]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    carry(&transfers, &fake, &delivered).await;
    let folder = row(&transfers, "Big");
    assert_eq!(folder.state, TransferState::Failed);
    let reason = folder.reason.unwrap();
    assert!(
        reason.starts_with("Not all of this folder was queued."),
        "{reason}"
    );
    assert!(
        reason.contains("And 1 more of its folders are not complete."),
        "{reason}"
    );
    // What was listed still arrives.
    assert_eq!(row(&transfers, "Big\\a.txt").state, TransferState::Done);
    assert_eq!(
        row(&transfers, "Big\\cut\\seen.txt").state,
        TransferState::Done
    );

    // More files than one copy queues.
    let many = format!("{DOCUMENTS}\\Many");
    fake.folder(&many);
    for n in 0..=MAX_BATCH_ENTRIES {
        fake.put_raw(&many, &format!("f{n:05}"), Node::File(Vec::new()));
    }
    let transfers = Transfers::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[("Many", EntryKind::Folder)]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    let epoch = transfers.must_start().unwrap();
    // Only the walk: the session ends before the first of the files, so that
    // none of them is written here.
    let walker = fake.clone();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let live = move || {
        (asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0).then(|| walker.clone())
    };
    run(transfers.clone(), epoch, live, delivered.emit()).await;
    let folder = row(&transfers, "Many");
    assert_eq!(folder.state, TransferState::Failed);
    assert!(
        folder
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("more than the 10000 files one copy queues")),
        "{:?}",
        folder.reason
    );
    assert_eq!(transfers.snapshot().len(), MAX_BATCH_ENTRIES + 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder whose folders go deeper than the walk goes says so.
#[tokio::test]
async fn a_folder_deeper_than_the_walk_goes_says_so() {
    let dir = scratch("download-depth");
    let root = dir.join("to");
    let fake = Fake::new();
    let mut at = format!("{DOCUMENTS}\\Deep");
    fake.folder(&at);
    for level in 0..=MAX_WALK_DEPTH {
        at = format!("{at}\\{level}");
        fake.folder(&at);
    }
    fake.put(&format!("{at}\\bottom.txt"), Node::File(vec![1]));
    let transfers = Transfers::default();
    let delivered = Delivered::default();
    transfers
        .queue_download(
            DOCUMENTS,
            &selected(&[("Deep", EntryKind::Folder)]),
            &root,
            &HashMap::new(),
        )
        .unwrap();
    carry(&transfers, &fake, &delivered).await;
    let folder = row(&transfers, "Deep");
    assert_eq!(folder.state, TransferState::Failed);
    assert!(
        folder
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("deeper than 32 levels")),
        "{:?}",
        folder.reason
    );
    assert!(
        !fake
            .asked()
            .iter()
            .any(|asked| asked.contains("bottom.txt"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- What the window is told ---

/// The window is told a transfer's progress at its end and each time a good
/// piece was added, not for every step.
#[tokio::test]
async fn progress_is_told_in_pieces() {
    let dir = scratch("progress");
    let fake = Fake::new();
    let transfers = Transfers::default();
    send(&transfers, &fake, vec![file(&dir, "a.bin", 10)], &[]).await;
    let id = row(&transfers, "a.bin").id;
    let step = |transferred, total| TransferProgress { transferred, total };
    // Nothing is told for an entry that does not run.
    assert_eq!(transfers.progress(id, step(1, 100)), None);
    transfers.update(id, |entry| entry.view.state = TransferState::Running);
    let total = 100 * 1024 * 1024;
    let mut told = Vec::new();
    for transferred in (0..=total).step_by(32 * 1024) {
        if let Some(view) = transfers.progress(id, step(transferred, total)) {
            assert_eq!(view.bytes, Some(total));
            told.push(view.done);
        }
    }
    assert_eq!(told.last(), Some(&total), "the end is always told");
    assert!(told.len() <= 51, "a fiftieth at a time: {}", told.len());
    assert!(told.len() >= 49, "{}", told.len());
    // A small file is told at its end.
    assert!(transfers.progress(id, step(10, 10)).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Many rows are delivered in pieces.
#[test]
fn many_rows_are_delivered_in_pieces() {
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let emit: Emit = {
        let delivered = Arc::clone(&delivered);
        Arc::new(move |views: Vec<TransferView>| delivered.lock().unwrap().push(views.len()))
    };
    let view = |id| TransferView {
        id,
        direction: Direction::Send,
        kind: EntryKind::File,
        name: format!("f{id}"),
        remote: String::new(),
        local: String::new(),
        bytes: None,
        done: 0,
        state: TransferState::Waiting,
        reason: None,
    };
    deliver(&emit, (0..1201).map(view).collect());
    deliver(&emit, Vec::new());
    assert_eq!(*delivered.lock().unwrap(), [500, 500, 201]);
}

/// The folders a technician chose are kept by number, a bounded number of
/// them, and an offer that was never sent gives way to newer ones.
#[test]
fn chosen_folders_and_offers_are_kept_by_number_and_bounded() {
    let transfers = Transfers::default();
    let first = transfers.choose(PathBuf::from("first"));
    assert_eq!(transfers.chosen(first), Some(PathBuf::from("first")));
    assert_eq!(transfers.chosen(first + 1), None);
    for n in 0..MAX_CHOSEN {
        transfers.choose(PathBuf::from(format!("later-{n}")));
    }
    assert_eq!(transfers.chosen(first), None, "the oldest gave way");

    let old = transfers.offer(vec![PathBuf::from("old.txt")]).offer;
    for n in 0..MAX_OFFERS {
        transfers.offer(vec![PathBuf::from(format!("new-{n}.txt"))]);
    }
    assert!(!transfers.lock().offers.iter().any(|kept| kept.id == old));
    assert_eq!(transfers.lock().offers.len(), MAX_OFFERS);
    // The window is told a few names, never more.
    let many: Vec<PathBuf> = (0..40).map(|n| PathBuf::from(format!("f{n}"))).collect();
    let offer = transfers.offer(many);
    assert_eq!((offer.items, offer.names.len()), (40, OFFER_NAMES));
}

#[test]
fn a_reason_is_a_sentence() {
    assert_eq!(sentence("the file is in use"), "The file is in use.");
    assert_eq!(sentence("Already a sentence."), "Already a sentence.");
    assert_eq!(sentence(""), "");
    assert_eq!(join("C:\\", "a.txt"), "C:\\a.txt");
    assert_eq!(join("C:\\Users", "ann"), "C:\\Users\\ann");
}

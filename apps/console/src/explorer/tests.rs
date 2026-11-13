//! Tests for finding and listing a folder (ST-M010-002) against a target held
//! in memory.
//!
//! The target answers the two requests the explorer makes the way an agent
//! does: a listing ordered by name, in pages, continued after a name. Unlike a
//! Windows computer it takes a path only in the spelling its own listings
//! give, so a request that named a folder as somebody typed it fails here.
//! Every request is kept, so a test can also say how many it took.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use cretlink_protocol::DriveKind;

use super::*;

/// One request the explorer made.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Drives,
    Page {
        folder: String,
        after: Option<String>,
    },
}

/// A target held in memory.
struct Target {
    drives: Vec<DriveEntry>,
    /// Every folder by its real path, with its entries ordered by name.
    folders: BTreeMap<String, Vec<FileEntry>>,
    /// Folders the target refuses to list.
    refusals: BTreeMap<String, ErrorCode>,
    /// The most entries of one page.
    page_len: usize,
    asked: Mutex<Vec<Asked>>,
    /// How many times a folder still answers with an empty page that says
    /// more follows.
    empty_pages: Mutex<BTreeMap<String, u32>>,
    /// A folder whose page requests fail from the given one on.
    failing: Option<(String, usize, TargetError)>,
    /// Cancels the token after this many page requests.
    cancel_after: Option<(usize, CancelToken)>,
}

fn entry(name: &str, kind: FileKind) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        kind,
        len: if kind == FileKind::File { 1024 } else { 0 },
        modified_ms: Some(1_800_000_000_000),
        readonly: false,
    }
}

fn dir(name: &str) -> FileEntry {
    entry(name, FileKind::Dir)
}

fn file(name: &str) -> FileEntry {
    entry(name, FileKind::File)
}

fn link(name: &str) -> FileEntry {
    entry(name, FileKind::Symlink)
}

fn drive(root: &str) -> DriveEntry {
    DriveEntry {
        root: root.to_owned(),
        kind: DriveKind::Fixed,
        label: None,
        total_bytes: None,
        free_bytes: None,
    }
}

impl Target {
    fn empty() -> Self {
        Self {
            drives: Vec::new(),
            folders: BTreeMap::new(),
            refusals: BTreeMap::new(),
            page_len: 256,
            asked: Mutex::new(Vec::new()),
            empty_pages: Mutex::new(BTreeMap::new()),
            failing: None,
            cancel_after: None,
        }
    }

    /// A customer computer with two drives and the folders of one account.
    fn windows() -> Self {
        let mut target = Self::empty();
        target.drives = vec![drive("C:\\"), drive("D:\\")];
        target.folder(
            "C:\\",
            vec![
                link("Documents and Settings"),
                dir("Program Files"),
                dir("Users"),
                dir("Windows"),
                file("pagefile.sys"),
            ],
        );
        target.folder(
            "C:\\Users",
            vec![dir("ann"), file("desktop.ini"), dir("jdoe")],
        );
        target.folder(
            "C:\\Users\\ann",
            vec![dir("Documents"), dir("Downloads"), file("NTUSER.DAT")],
        );
        target.folder(
            "C:\\Users\\ann\\Documents",
            vec![dir("Invoices"), file("notes.txt"), file("report.pdf")],
        );
        target.folder("C:\\Users\\ann\\Documents\\Invoices", Vec::new());
        target.folder("C:\\Users\\ann\\Downloads", vec![file("setup.exe")]);
        target.folder("C:\\Program Files", vec![dir("CretLink")]);
        target.folder("C:\\Program Files\\CretLink", vec![file("cretlink.exe")]);
        target.folder("C:\\Windows", vec![dir("System32")]);
        target.folder("D:\\", Vec::new());
        target
            .refusals
            .insert("C:\\Users\\jdoe".to_owned(), ErrorCode::AccessDenied);
        target
    }

    /// Adds a folder with `entries`, in the order a listing gives them.
    fn folder(&mut self, path: &str, mut entries: Vec<FileEntry>) {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.folders.insert(path.to_owned(), entries);
    }

    /// Adds a folder with `count` files and, among them, the folder `drivers`.
    fn large_folder(&mut self, path: &str, count: usize) {
        let mut entries: Vec<FileEntry> = (0..count)
            .map(|n| file(&format!("file-{n:06}.dll")))
            .collect();
        entries.push(dir("drivers"));
        self.folder(path, entries);
        self.folder(&join(path, "drivers"), vec![file("etc.sys")]);
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    /// The page requests made for `folder`.
    fn pages_of(&self, folder: &str) -> usize {
        self.asked()
            .iter()
            .filter(|asked| matches!(asked, Asked::Page { folder: of, .. } if of == folder))
            .count()
    }

    fn forget_asked(&self) {
        self.asked.lock().unwrap().clear();
    }
}

#[async_trait]
impl FolderSource for Target {
    async fn drives(&self) -> Result<Vec<DriveEntry>, TargetError> {
        self.asked.lock().unwrap().push(Asked::Drives);
        Ok(self.drives.clone())
    }

    async fn page(&self, folder: &RemotePath, after: Option<&str>) -> Result<DirPage, TargetError> {
        let RemotePath::Absolute { path } = folder else {
            panic!("the explorer names a folder by its path: {folder:?}");
        };
        let request = {
            let mut asked = self.asked.lock().unwrap();
            asked.push(Asked::Page {
                folder: path.clone(),
                after: after.map(str::to_owned),
            });
            asked
                .iter()
                .filter(|asked| matches!(asked, Asked::Page { .. }))
                .count()
        };
        if let Some((count, cancel)) = &self.cancel_after
            && request >= *count
        {
            cancel.cancel();
        }
        if let Some((failing, from, error)) = &self.failing
            && failing == path
            && self.pages_of(path) >= *from
        {
            return Err(error.clone());
        }
        if let Some(code) = self.refusals.get(path) {
            return Err(TargetError::Remote {
                code: *code,
                message: "failed to open the directory".to_owned(),
                stage: ErrorStage::Execution,
            });
        }
        let Some(entries) = self.folders.get(path) else {
            return Err(TargetError::Remote {
                code: ErrorCode::PathNotFound,
                message: "the path does not exist".to_owned(),
                stage: ErrorStage::Execution,
            });
        };
        if let Some(left) = self.empty_pages.lock().unwrap().get_mut(path)
            && *left > 0
        {
            *left -= 1;
            return Ok(DirPage {
                entries: Vec::new(),
                end: false,
            });
        }
        let rest: Vec<&FileEntry> = entries
            .iter()
            .filter(|entry| after.is_none_or(|after| entry.name.as_str() > after))
            .collect();
        Ok(DirPage {
            entries: rest
                .iter()
                .take(self.page_len)
                .map(|entry| (*entry).clone())
                .collect(),
            end: rest.len() <= self.page_len,
        })
    }
}

fn memory() -> Mutex<Browsed> {
    Mutex::new(Browsed::default())
}

async fn located(target: &Target, memory: &Mutex<Browsed>, asked: &str) -> Located {
    locate(target, memory, asked, &CancelToken::new())
        .await
        .unwrap_or_else(|error| panic!("{asked} should be found: {error:?}"))
        .expect("not cancelled")
}

async fn refusal(target: &Target, asked: &str) -> (ErrorCode, String, ErrorStage) {
    match locate(target, &memory(), asked, &CancelToken::new()).await {
        Err(TargetError::Remote {
            code,
            message,
            stage,
        }) => (code, message, stage),
        other => panic!("{asked} should be refused, got {other:?}"),
    }
}

/// Everything one listing delivered.
#[derive(Debug, Default)]
struct Delivered {
    opened: Option<(RemotePath, Option<String>)>,
    names: Vec<String>,
    pages: Vec<usize>,
    ended: bool,
}

impl Delivered {
    fn take(&mut self, event: FolderEvent) {
        assert!(!self.ended, "nothing follows the last page");
        let (entries, end) = match event {
            FolderEvent::Opened {
                folder,
                select,
                entries,
                end,
            } => {
                assert!(self.opened.is_none(), "a folder opens once");
                self.opened = Some((folder, select));
                (entries, end)
            }
            FolderEvent::Page { entries, end } => {
                assert!(self.opened.is_some(), "the folder opens first");
                (entries, end)
            }
        };
        self.pages.push(entries.len());
        self.names
            .extend(entries.into_iter().map(|entry| entry.name));
        self.ended = end;
    }
}

async fn opened(
    target: &Target,
    memory: &Mutex<Browsed>,
    asked: &str,
    cancel: &CancelToken,
) -> (Result<FolderReport, TargetError>, Delivered) {
    let mut delivered = Delivered::default();
    let result = {
        let mut sink = |event| delivered.take(event);
        open(target, memory, asked, cancel, &mut sink).await
    };
    (result, delivered)
}

fn absolute(path: &str) -> RemotePath {
    RemotePath::Absolute {
        path: path.to_owned(),
    }
}

// --- A path as a screen spells it ---

#[test]
fn a_path_is_taken_apart_into_its_drive_and_its_names() {
    assert_eq!(
        spell("C:\\Users\\ann").unwrap(),
        (
            "C:\\".to_owned(),
            vec!["Users".to_owned(), "ann".to_owned()]
        )
    );
    // A forward slash, a trailing separator and a doubled one change nothing.
    assert_eq!(
        spell("c:/Users//ann/").unwrap(),
        (
            "c:\\".to_owned(),
            vec!["Users".to_owned(), "ann".to_owned()]
        )
    );
    // A drive alone is its root; so is the root.
    assert_eq!(spell("D:").unwrap(), ("D:\\".to_owned(), Vec::new()));
    assert_eq!(spell("D:\\").unwrap(), ("D:\\".to_owned(), Vec::new()));
    // A path that was copied carries quotation marks and spaces around it.
    assert_eq!(
        spell("  \"C:\\Program Files\\CretLink\" ").unwrap(),
        (
            "C:\\".to_owned(),
            vec!["Program Files".to_owned(), "CretLink".to_owned()]
        )
    );
}

#[test]
fn a_text_that_is_no_remote_path_is_refused_as_it_is_spelled() {
    for bad in [
        "",
        "   ",
        "Users\\ann",
        "\\Users\\ann",
        "C:Users",
        "C:\\Users\\..\\Windows",
        "\\\\server\\share\\folder",
        "\\\\?\\C:\\Users",
        "/home/ann",
        "C:\\Users\\ann:stream",
        "C:\\Temp\\CON",
        "C:\\Users\\a\u{7}b",
    ] {
        match spell(bad) {
            Err(TargetError::Remote {
                code: ErrorCode::InvalidPath,
                stage: ErrorStage::Validation,
                ..
            }) => {}
            other => panic!("{bad:?} should be refused as an invalid path, got {other:?}"),
        }
    }
    let long = format!("C:\\{}", "a".repeat(5000));
    assert!(matches!(
        spell(&long),
        Err(TargetError::Remote {
            code: ErrorCode::LimitExceeded,
            stage: ErrorStage::Validation,
            ..
        })
    ));
}

/// A spelling that is refused asks nothing of the target.
#[tokio::test]
async fn a_refused_spelling_reaches_nobody() {
    let target = Target::windows();
    let (code, _, stage) = refusal(&target, "C:\\Users\\..\\Windows").await;
    assert_eq!(
        (code, stage),
        (ErrorCode::InvalidPath, ErrorStage::Validation)
    );
    assert!(target.asked().is_empty(), "{:?}", target.asked());
}

// --- The real location ---

/// A path typed in another letter case, with forward slashes, leads to the
/// names the target lists, and every request names a folder by those.
#[tokio::test]
async fn a_typed_path_is_led_to_the_names_the_target_lists() {
    let target = Target::windows();
    let memory = memory();
    let found = located(&target, &memory, "c:/USERS/Ann/documents").await;
    assert_eq!(
        found,
        Located {
            folder: "C:\\Users\\ann\\Documents".to_owned(),
            select: None,
        }
    );
    // The drive list first, then a folder above each name, by its real name.
    let asked = target.asked();
    assert_eq!(asked[0], Asked::Drives);
    let folders: Vec<&str> = asked[1..]
        .iter()
        .map(|asked| match asked {
            Asked::Page { folder, .. } => folder.as_str(),
            Asked::Drives => panic!("the drives are asked for once"),
        })
        .collect();
    for folder in &folders {
        assert!(
            ["C:\\", "C:\\Users", "C:\\Users\\ann"].contains(folder),
            "{folder} was asked for"
        );
    }
    assert!(folders.contains(&"C:\\Users\\ann"));
}

/// The root of a drive, a drive alone and a drive in a small letter all lead
/// to the root as the drive list gives it.
#[tokio::test]
async fn a_drive_is_named_as_the_drive_list_names_it() {
    let target = Target::windows();
    for asked in ["D:\\", "d:", "d:/"] {
        assert_eq!(
            located(&target, &memory(), asked).await.folder,
            "D:\\",
            "{asked}"
        );
    }
}

/// A name spelled as the folder lists it is found with one request, however
/// large the folder is: the listing is asked for from just before the name.
#[tokio::test]
async fn a_name_spelled_as_listed_takes_one_request_in_a_large_folder() {
    let mut target = Target::windows();
    target.large_folder("C:\\Windows\\System32", 5000);
    let memory = memory();
    // The folders above are known after this.
    located(&target, &memory, "C:\\Windows\\System32").await;
    target.forget_asked();

    let found = located(&target, &memory, "C:\\Windows\\System32\\drivers").await;
    assert_eq!(found.folder, "C:\\Windows\\System32\\drivers");
    assert_eq!(
        target.asked(),
        [Asked::Page {
            folder: "C:\\Windows\\System32".to_owned(),
            after: Some("driver".to_owned()),
        }]
    );
}

/// A name in another letter case is found by reading the folder from its
/// start, and the search stops at the name: the pages behind it are not
/// asked for.
#[tokio::test]
async fn another_letter_case_reads_the_folder_until_the_name() {
    let mut target = Target::windows();
    target.large_folder("C:\\Windows\\System32", 5000);
    // Two more folders: one that a listing gives before every file, and one
    // that it gives after them all.
    let mut entries = target.folders["C:\\Windows\\System32"].clone();
    entries.push(dir("Config"));
    entries.push(dir("winevt"));
    let count = entries.len();
    target.folder("C:\\Windows\\System32", entries);
    target.folder("C:\\Windows\\System32\\Config", Vec::new());
    target.folder("C:\\Windows\\System32\\winevt", Vec::new());
    let memory = memory();
    located(&target, &memory, "C:\\Windows\\System32").await;
    target.forget_asked();

    let found = located(&target, &memory, "C:\\Windows\\System32\\CONFIG").await;
    assert_eq!(found.folder, "C:\\Windows\\System32\\Config");
    // One request for the spelling as typed, one for the first page.
    assert_eq!(target.pages_of("C:\\Windows\\System32"), 2);

    // The last name of the folder is found too, after every page before it.
    target.forget_asked();
    let found = located(&target, &memory, "C:\\Windows\\System32\\WINEVT").await;
    assert_eq!(found.folder, "C:\\Windows\\System32\\winevt");
    assert_eq!(
        target.pages_of("C:\\Windows\\System32"),
        1 + count.div_ceil(256)
    );
}

/// A name the folder above does not list is not found: a name that does not
/// exist, and a second spelling of one that does, which no listing shows.
#[tokio::test]
async fn a_name_the_folder_does_not_list_is_not_found() {
    let target = Target::windows();
    let (code, message, stage) = refusal(&target, "C:\\Usres\\ann").await;
    assert_eq!(code, ErrorCode::PathNotFound);
    assert_eq!(stage, ErrorStage::Execution);
    assert_eq!(message, "C:\\ lists no entry named Usres");

    let (code, message, _) = refusal(&target, "C:\\PROGRA~1\\CretLink").await;
    assert_eq!(code, ErrorCode::PathNotFound);
    assert_eq!(message, "C:\\ lists no entry named PROGRA~1");

    // A name without a letter has no other letter case to look for.
    let target = Target::windows();
    let (code, _, _) = refusal(&target, "C:\\Program Files\\2026").await;
    assert_eq!(code, ErrorCode::PathNotFound);
    assert_eq!(target.pages_of("C:\\Program Files"), 1);
}

/// A link is shown and never followed: a path that ends in one, or that
/// leads through one, is refused and nothing behind it is asked for.
#[tokio::test]
async fn a_link_is_refused_and_nothing_behind_it_is_asked_for() {
    let target = Target::windows();
    let (code, message, _) = refusal(&target, "C:\\Documents and Settings").await;
    assert_eq!(code, ErrorCode::InvalidPath);
    assert_eq!(
        message,
        "C:\\Documents and Settings is a link, and a link is shown, not followed"
    );

    let target = Target::windows();
    let (code, message, _) = refusal(&target, "C:\\documents and settings\\ann\\Desktop").await;
    assert_eq!(code, ErrorCode::InvalidPath);
    assert!(
        message.starts_with("C:\\Documents and Settings is a link"),
        "{message}"
    );
    for asked in target.asked() {
        if let Asked::Page { folder, .. } = asked {
            assert_eq!(folder, "C:\\", "{folder} lies behind the link");
        }
    }
}

/// A path to a file opens the folder that holds it and names the file. A
/// file with further names behind it is refused.
#[tokio::test]
async fn a_path_to_a_file_leads_to_its_folder() {
    let target = Target::windows();
    assert_eq!(
        located(&target, &memory(), "c:\\users\\ann\\documents\\REPORT.PDF").await,
        Located {
            folder: "C:\\Users\\ann\\Documents".to_owned(),
            select: Some("report.pdf".to_owned()),
        }
    );
    let (code, message, _) = refusal(&target, "C:\\pagefile.sys\\inside").await;
    assert_eq!(code, ErrorCode::WrongPathKind);
    assert_eq!(message, "C:\\pagefile.sys is not a folder");
}

/// A drive the target does not list is not found. The drive list is read
/// again for a drive that is not remembered, since a drive can be put in
/// during a session, and only then.
#[tokio::test]
async fn a_drive_is_looked_up_in_the_drive_list() {
    let mut target = Target::windows();
    let memory = memory();
    assert_eq!(located(&target, &memory, "C:\\").await.folder, "C:\\");
    assert_eq!(located(&target, &memory, "D:\\").await.folder, "D:\\");
    assert_eq!(target.asked(), [Asked::Drives], "one drive list for both");

    match locate(&target, &memory, "E:\\Backup", &CancelToken::new()).await {
        Err(TargetError::Remote { code, message, .. }) => {
            assert_eq!(code, ErrorCode::PathNotFound);
            assert_eq!(message, "the customer computer lists no drive E:");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(target.asked(), [Asked::Drives, Asked::Drives]);

    // A stick is put in: the next look at that drive finds it.
    target.drives.push(drive("E:\\"));
    target.folder("E:\\", vec![dir("Backup")]);
    target.folder("E:\\Backup", Vec::new());
    assert_eq!(
        located(&target, &memory, "e:\\backup").await.folder,
        "E:\\Backup"
    );
}

/// A target without drive letters lists its root file system as its one
/// drive, which the wire's path rules do not take: it is refused as it is
/// spelled, and nothing is asked.
#[tokio::test]
async fn a_root_that_is_no_drive_letter_is_refused() {
    let mut target = Target::empty();
    target.drives = vec![drive("/")];
    let (code, _, stage) = refusal(&target, "/").await;
    assert_eq!(
        (code, stage),
        (ErrorCode::InvalidPath, ErrorStage::Validation)
    );
    assert!(target.asked().is_empty());
}

// --- What is remembered ---

/// A step into a subfolder of the folder that was just listed, a step up and
/// a step back each cost the listing and nothing else.
#[tokio::test]
async fn a_step_down_up_or_back_costs_only_the_listing() {
    let target = Target::windows();
    let memory = memory();
    let cancel = CancelToken::new();
    let (result, _) = opened(&target, &memory, "C:\\Users", &cancel).await;
    assert_eq!(result.unwrap().end, ListingEnd::Complete);

    for step in ["C:\\Users\\ann", "C:\\Users", "C:\\Users\\ann", "C:\\"] {
        target.forget_asked();
        let (result, delivered) = opened(&target, &memory, step, &cancel).await;
        assert_eq!(result.unwrap().folder, Some(absolute(step)));
        assert_eq!(delivered.opened.unwrap().0, absolute(step));
        assert_eq!(
            target.asked(),
            [Asked::Page {
                folder: step.to_owned(),
                after: None,
            }],
            "{step}"
        );
    }
}

/// A subfolder is remembered under the spelling the listing gave. The same
/// folder typed in another letter case is looked up, and led to that
/// spelling.
#[tokio::test]
async fn only_the_spelling_the_target_gave_is_remembered() {
    let target = Target::windows();
    let memory = memory();
    let cancel = CancelToken::new();
    opened(&target, &memory, "C:\\Users", &cancel)
        .await
        .0
        .unwrap();
    target.forget_asked();
    let (result, _) = opened(&target, &memory, "C:\\Users\\ANN", &cancel).await;
    assert_eq!(result.unwrap().folder, Some(absolute("C:\\Users\\ann")));
    assert!(target.pages_of("C:\\Users") >= 1, "ANN was looked up");
    assert_eq!(target.pages_of("C:\\Users\\ANN"), 0);
}

/// The memory is bounded: a folder that was pushed out is looked up again,
/// and found.
#[tokio::test]
async fn a_folder_that_was_forgotten_is_looked_up_again() {
    let mut target = Target::empty();
    target.drives = vec![drive("C:\\")];
    let names: Vec<String> = (0..MAX_KNOWN_FOLDERS + 20)
        .map(|n| format!("folder-{n:04}"))
        .collect();
    target.folder("C:\\", names.iter().map(|name| dir(name)).collect());
    for name in &names {
        target.folder(&join("C:\\", name), Vec::new());
    }
    let memory = memory();
    for name in &names {
        located(&target, &memory, &join("C:\\", name)).await;
    }
    {
        let memory = memory.lock().unwrap();
        assert_eq!(memory.folders.len(), MAX_KNOWN_FOLDERS);
        assert!(!memory.folders.contains(&"C:\\folder-0000".to_owned()));
        assert!(
            memory
                .folders
                .contains(&format!("C:\\{}", names[names.len() - 1]))
        );
    }
    target.forget_asked();
    assert_eq!(
        located(&target, &memory, "C:\\folder-0000").await.folder,
        "C:\\folder-0000"
    );
    assert_eq!(target.pages_of("C:\\"), 1, "looked up again");
}

/// The subfolders of the folder listed last are remembered up to a bound;
/// beyond it a subfolder is looked up, and still found.
#[tokio::test]
async fn the_subfolders_of_a_listing_are_remembered_up_to_a_bound() {
    let mut memory = Browsed::default();
    memory.listing("C:\\Big");
    let page: Vec<FileEntry> = (0..MAX_LISTED_SUBFOLDERS + 5)
        .map(|n| dir(&format!("d{n:06}")))
        .collect();
    memory.listed("C:\\Big", &page);
    assert!(memory.lists("C:\\Big", "d000000"));
    assert!(!memory.lists("C:\\Big", &format!("d{:06}", MAX_LISTED_SUBFOLDERS + 4)));
    // A page of another listing than the one that runs is not taken.
    memory.listed("C:\\Other", &[dir("x")]);
    assert!(!memory.lists("C:\\Other", "x"));
    // A file of that name is no subfolder.
    memory.listing("C:\\Small");
    memory.listed("C:\\Small", &[file("notes.txt"), dir("sub")]);
    assert!(memory.lists("C:\\Small", "sub"));
    assert!(!memory.lists("C:\\Small", "notes.txt"));
    assert!(!memory.lists("C:\\Big", "d000000"), "only the last listing");
}

// --- A listing in pages ---

/// A folder of thousands of entries arrives in pages of whatever length the
/// target gives them, each name once and in order, and the first page comes
/// with the folder's real location.
#[tokio::test]
async fn a_large_folder_is_listed_in_pages_until_its_end() {
    for page_len in [1, 7, 256] {
        let mut target = Target::windows();
        target.page_len = page_len;
        let count = if page_len == 1 { 40 } else { 3000 };
        target.large_folder("C:\\Windows\\System32", count);
        let (result, delivered) = opened(
            &target,
            &memory(),
            "c:\\windows\\system32",
            &CancelToken::new(),
        )
        .await;
        let report = result.unwrap();
        let real = absolute("C:\\Windows\\System32");
        assert_eq!(
            report,
            FolderReport {
                folder: Some(real.clone()),
                select: None,
                entries: count as u64 + 1,
                end: ListingEnd::Complete,
            }
        );
        assert_eq!(delivered.opened, Some((real, None)));
        assert!(delivered.ended);
        let listed: Vec<&String> = delivered.names.iter().collect();
        let expected: Vec<&String> = target.folders["C:\\Windows\\System32"]
            .iter()
            .map(|entry| &entry.name)
            .collect();
        assert_eq!(listed, expected, "every name once, in the target's order");
        assert_eq!(delivered.pages.len(), (count + 1).div_ceil(page_len));
        assert!(delivered.pages.iter().all(|len| *len <= page_len));
    }
}

/// An empty folder opens with one page that is empty and the last.
#[tokio::test]
async fn an_empty_folder_opens_with_an_empty_last_page() {
    let target = Target::windows();
    let (result, delivered) = opened(&target, &memory(), "D:\\", &CancelToken::new()).await;
    assert_eq!(
        result.unwrap(),
        FolderReport {
            folder: Some(absolute("D:\\")),
            select: None,
            entries: 0,
            end: ListingEnd::Complete,
        }
    );
    assert_eq!(delivered.pages, [0]);
    assert!(delivered.ended);
}

/// A path to a file opens its folder, and the first page names the file.
#[tokio::test]
async fn the_folder_of_a_file_opens_with_the_file_named() {
    let target = Target::windows();
    let (result, delivered) = opened(
        &target,
        &memory(),
        "C:\\Users\\ann\\Documents\\notes.txt",
        &CancelToken::new(),
    )
    .await;
    let report = result.unwrap();
    assert_eq!(report.select.as_deref(), Some("notes.txt"));
    assert_eq!(
        delivered.opened,
        Some((
            absolute("C:\\Users\\ann\\Documents"),
            Some("notes.txt".to_owned())
        ))
    );
    assert_eq!(delivered.names, ["Invoices", "notes.txt", "report.pdf"]);
}

/// A folder the target refuses to list delivers nothing: the caller stays
/// where it was. The refusal is the target's own.
#[tokio::test]
async fn a_folder_the_target_refuses_delivers_nothing() {
    let target = Target::windows();
    let (result, delivered) =
        opened(&target, &memory(), "C:\\Users\\jdoe", &CancelToken::new()).await;
    assert!(
        matches!(
            result,
            Err(TargetError::Remote {
                code: ErrorCode::AccessDenied,
                ..
            })
        ),
        "{result:?}"
    );
    assert!(delivered.opened.is_none());
    assert!(delivered.names.is_empty());

    for missing in ["C:\\Nowhere", "C:\\Users\\ann\\Documents\\gone\\deeper"] {
        let (result, delivered) = opened(&target, &memory(), missing, &CancelToken::new()).await;
        assert!(
            matches!(
                result,
                Err(TargetError::Remote {
                    code: ErrorCode::PathNotFound,
                    ..
                })
            ),
            "{missing}: {result:?}"
        );
        assert!(delivered.opened.is_none(), "{missing}");
    }
}

/// A listing that fails on its way keeps what it delivered and is an error:
/// it never ends as the whole folder.
#[tokio::test]
async fn a_listing_that_fails_on_its_way_is_an_error_with_what_arrived() {
    for error in [TargetError::Disconnected, TargetError::NoConsentedSession] {
        let mut target = Target::windows();
        target.page_len = 100;
        target.large_folder("C:\\Windows\\System32", 1000);
        target.failing = Some(("C:\\Windows\\System32".to_owned(), 4, error.clone()));
        let memory = memory();
        located(&target, &memory, "C:\\Windows").await;
        target.forget_asked();
        let (result, delivered) = opened(
            &target,
            &memory,
            "C:\\Windows\\System32",
            &CancelToken::new(),
        )
        .await;
        assert_eq!(result, Err(error));
        assert_eq!(delivered.pages, [100, 100, 100]);
        assert!(!delivered.ended, "the folder was not listed to its end");
    }
}

/// Cancelling ends a listing between two pages, with what arrived, and the
/// search for a folder between two requests, with nothing.
#[tokio::test]
async fn a_cancelled_listing_stops_between_two_requests() {
    let cancel = CancelToken::new();
    let mut target = Target::windows();
    target.page_len = 100;
    target.large_folder("C:\\Windows\\System32", 1000);
    let memory = memory();
    // The folder is known, so only its pages are asked for below.
    located(&target, &memory, "C:\\Windows\\System32").await;
    target.forget_asked();
    target.cancel_after = Some((3, cancel.clone()));
    let (result, delivered) = opened(&target, &memory, "C:\\Windows\\System32", &cancel).await;
    assert_eq!(
        result.unwrap(),
        FolderReport {
            folder: Some(absolute("C:\\Windows\\System32")),
            select: None,
            entries: 300,
            end: ListingEnd::Cancelled,
        }
    );
    assert_eq!(delivered.pages, [100, 100, 100]);
    assert!(!delivered.ended);
    assert_eq!(
        target.pages_of("C:\\Windows\\System32"),
        3,
        "no fourth page"
    );

    // Cancelled before the folder was found: nothing was opened.
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let target = Target::windows();
    let (result, delivered) = opened(
        &target,
        &Mutex::new(Browsed::default()),
        "C:\\Users\\ann",
        &cancelled,
    )
    .await;
    assert_eq!(
        result.unwrap(),
        FolderReport {
            folder: None,
            select: None,
            entries: 0,
            end: ListingEnd::Cancelled,
        }
    );
    assert!(delivered.opened.is_none());
    assert!(target.asked().is_empty());
}

/// A folder with more entries than the explorer holds is listed up to the
/// bound, and the end says that more exist.
#[tokio::test]
async fn a_listing_stops_at_its_bound_and_says_that_more_exist() {
    let mut target = Target::empty();
    target.drives = vec![drive("C:\\")];
    let count = usize::try_from(MAX_FOLDER_ENTRIES).unwrap() + 1000;
    let entries: Vec<FileEntry> = (0..count).map(|n| file(&format!("f{n:07}"))).collect();
    target.folder("C:\\", entries);
    let (result, delivered) = opened(&target, &memory(), "C:\\", &CancelToken::new()).await;
    let report = result.unwrap();
    assert_eq!(report.end, ListingEnd::Limit);
    assert!(report.entries >= MAX_FOLDER_ENTRIES);
    assert!(
        report.entries < MAX_FOLDER_ENTRIES + 256,
        "at most one page more"
    );
    assert_eq!(delivered.names.len() as u64, report.entries);
    assert!(!delivered.ended, "the last page delivered is not the end");
}

/// A page that is empty and says more follows is asked for again; a folder
/// that keeps answering so is given up, never listed as empty.
#[tokio::test]
async fn an_empty_page_that_is_not_the_last_is_asked_for_again() {
    let target = Target::windows();
    target
        .empty_pages
        .lock()
        .unwrap()
        .insert("C:\\Users\\ann".to_owned(), MAX_EMPTY_PAGES);
    let memory = memory();
    located(&target, &memory, "C:\\Users").await;
    // Found through the listing above it, which is not the folder that
    // answers with empty pages.
    let cancel = CancelToken::new();
    opened(&target, &memory, "C:\\Users", &cancel)
        .await
        .0
        .unwrap();
    let (result, delivered) = opened(&target, &memory, "C:\\Users\\ann", &cancel).await;
    assert_eq!(result.unwrap().end, ListingEnd::Complete);
    assert_eq!(delivered.names, ["Documents", "Downloads", "NTUSER.DAT"]);

    target
        .empty_pages
        .lock()
        .unwrap()
        .insert("C:\\Users\\ann".to_owned(), MAX_EMPTY_PAGES + 1);
    let (result, delivered) = opened(&target, &memory, "C:\\Users\\ann", &cancel).await;
    assert_eq!(result, Err(TargetError::Unexpected));
    assert!(
        delivered.opened.is_none(),
        "an unlisted folder is not shown as empty"
    );
}

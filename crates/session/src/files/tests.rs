//! Tests of the folder-removal engine against a scripted agent
//! (ST-M010-001): what the engine asks, in which order, and what it makes of
//! the answers, without a session or a file system.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::{Ready, ready};

use cretlink_protocol::{B64Bytes, ErrorCode, OpResult, Operation, RemotePath, TreeState};

use super::{MAX_TREE_REMOVAL_STEPS, TreeRemovalProgress, run_removal};
use crate::{CancelToken, JobOutcome, SessionError};

/// The folder every test removes.
fn folder() -> RemotePath {
    RemotePath::Absolute {
        path: "C:\\Users\\ann\\Downloads\\old-setup".to_owned(),
    }
}

/// What one request of the engine asked: whether it ends the removal, and
/// the folder identity it gave back.
type Asked = (bool, Option<Vec<u8>>);

/// The answer of a step that removed `files` files and stands in `state`,
/// naming the folder `id` when one is given.
fn answer(state: TreeState, files: u64, id: Option<&[u8]>) -> Result<OpResult, SessionError> {
    Ok(OpResult::TreeRemoval {
        state,
        removed_files: files,
        removed_dirs: 0,
        failed: Vec::new(),
        failed_more: 0,
        folder_id: id.map(|id| B64Bytes::new(id.to_vec())),
    })
}

/// An agent that answers the requests of a removal with `answers`, in order,
/// and notes what each request asked.
struct Scripted {
    answers: RefCell<VecDeque<Result<OpResult, SessionError>>>,
    asked: RefCell<Vec<Asked>>,
}

impl Scripted {
    fn new(answers: impl IntoIterator<Item = Result<OpResult, SessionError>>) -> Self {
        Self {
            answers: RefCell::new(answers.into_iter().collect()),
            asked: RefCell::new(Vec::new()),
        }
    }

    /// Answers one request.
    fn ask(&self, request: Operation) -> Ready<Result<OpResult, SessionError>> {
        let Operation::RemoveTree {
            path,
            stop,
            folder_id,
        } = request
        else {
            panic!("a removal asks nothing but removal steps");
        };
        assert_eq!(path, folder(), "every step is about the same path");
        self.asked
            .borrow_mut()
            .push((stop, folder_id.map(B64Bytes::into_inner)));
        let next = self.answers.borrow_mut().pop_front();
        ready(next.unwrap_or_else(|| answer(TreeState::Stopped, 0, None)))
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.borrow().clone()
    }
}

/// One removal is about one folder: every step after the first gives back
/// the identity that the step before it answered with, so the agent goes on
/// with the folder it began with.
#[tokio::test]
async fn every_step_names_the_folder_the_step_before_answered_with() {
    let agent = Scripted::new([
        answer(TreeState::More, 4096, Some(b"folder-a")),
        answer(TreeState::More, 4096, Some(b"folder-a")),
        answer(TreeState::Removed, 12, None),
    ]);
    let mut progress: Vec<TreeRemovalProgress> = Vec::new();

    let report = run_removal(
        |step| agent.ask(step),
        folder(),
        &CancelToken::new(),
        |seen| progress.push(seen),
    )
    .await
    .expect("the removal runs");

    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.removed_files, 4096 + 4096 + 12);
    assert_eq!(progress.len(), 3, "one snapshot for each answered step");
    assert_eq!(
        agent.asked(),
        vec![
            (false, None),
            (false, Some(b"folder-a".to_vec())),
            (false, Some(b"folder-a".to_vec())),
        ],
        "the first step names no folder; every later one names the folder of the answer before"
    );
}

/// A step that the agent refuses — the folder at the path is no longer the
/// one the removal began with — ends the removal as failed, with the agent's
/// reason and with what the earlier steps had removed. The agent has ended
/// the removal on its side by refusing, so it is not told again.
#[tokio::test]
async fn a_refused_step_ends_the_removal_as_failed_with_what_was_removed() {
    let agent = Scripted::new([
        answer(TreeState::More, 40, Some(b"folder-a")),
        Err(SessionError::Remote {
            code: ErrorCode::InvalidPath,
            message: "the folder is not the one this removal began with".to_owned(),
        }),
    ]);

    let report = run_removal(
        |step| agent.ask(step),
        folder(),
        &CancelToken::new(),
        |_| {},
    )
    .await
    .expect("a refused removal is a report");

    assert_eq!(report.outcome, JobOutcome::Failed, "never a success");
    assert_eq!(report.removed_files, 40);
    assert_eq!(
        report.error.as_ref().map(|error| error.code),
        Some(ErrorCode::InvalidPath)
    );
    assert_eq!(
        agent.asked(),
        vec![(false, None), (false, Some(b"folder-a".to_vec()))]
    );
}

/// A removal that is cancelled after a step left more to do tells the agent
/// that it ended, so the customer does not see it as still running.
#[tokio::test]
async fn a_cancelled_removal_tells_the_agent_that_it_ended() {
    let agent = Scripted::new([answer(TreeState::More, 7, Some(b"folder-a"))]);
    let cancel = CancelToken::new();

    let report = run_removal(
        |step| agent.ask(step),
        folder(),
        &cancel,
        |_| cancel.cancel(),
    )
    .await
    .expect("a cancelled removal is a report");

    assert_eq!(report.outcome, JobOutcome::Cancelled);
    assert_eq!(report.removed_files, 7);
    assert_eq!(agent.asked(), vec![(false, None), (true, None)]);
}

/// An answer that contradicts itself is an error, never a result: more to do
/// without a folder to go on with, or more to do with nothing removed.
#[tokio::test]
async fn an_answer_that_says_more_without_progress_or_without_a_folder_is_not_a_result() {
    for contradiction in [
        answer(TreeState::More, 5, None),
        answer(TreeState::More, 0, Some(b"folder-a")),
    ] {
        let agent = Scripted::new([contradiction]);
        let error = run_removal(
            |step| agent.ask(step),
            folder(),
            &CancelToken::new(),
            |_| {},
        )
        .await
        .expect_err("a contradiction is not a report");
        assert!(
            matches!(error, SessionError::Unexpected { .. }),
            "{error:?}"
        );
        assert_eq!(
            agent.asked(),
            vec![(false, None)],
            "and nothing more is asked"
        );
    }
}

/// A removal ends, whatever the agent answers. An agent that says "more"
/// after every step — its folder is refilled as fast as it is emptied — is
/// asked a bounded number of times; then the removal ends as failed with the
/// reason, with what it had removed, and the agent is told that it ended.
#[tokio::test]
async fn a_removal_that_never_ends_is_given_up_after_a_bounded_number_of_steps() {
    use std::cell::Cell;

    let steps = Cell::new(0u64);
    let told_that_it_ended = Cell::new(false);
    // An agent that always removes one file and always says that more remain.
    let endless = |request: Operation| {
        let Operation::RemoveTree { stop, .. } = request else {
            panic!("a removal asks nothing but removal steps");
        };
        if stop {
            assert!(!told_that_it_ended.replace(true), "told once");
            return ready(answer(TreeState::Stopped, 0, None));
        }
        assert!(
            !told_that_it_ended.get(),
            "no step is asked for after the removal was ended"
        );
        steps.set(steps.get() + 1);
        ready(answer(TreeState::More, 1, Some(b"folder-a")))
    };

    let report = run_removal(endless, folder(), &CancelToken::new(), |_| {})
        .await
        .expect("a removal that is given up is a report");

    assert_eq!(steps.get(), MAX_TREE_REMOVAL_STEPS, "and not one more");
    assert_eq!(report.outcome, JobOutcome::Failed, "never a success");
    assert_eq!(report.removed_files, MAX_TREE_REMOVAL_STEPS);
    let error = report.error.expect("the removal says why it ended");
    assert_eq!(error.code, ErrorCode::LimitExceeded);
    assert!(
        error.message.contains(&MAX_TREE_REMOVAL_STEPS.to_string()),
        "{}",
        error.message
    );
    assert!(
        told_that_it_ended.get(),
        "the agent is told, so the customer does not see the removal as still running"
    );
}

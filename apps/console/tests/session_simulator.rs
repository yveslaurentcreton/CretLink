//! Simulator-backed integration tests for the console [`SessionClient`]
//! (ST-M001-003).
//!
//! Each test runs [`cretlink_simulator::Simulator`] on one half of a
//! `tokio::io::duplex` pair and a [`cretlink_console::SessionClient`] on the
//! other, proving that every scripted scenario maps to the correct
//! client-visible outcome (REQ-N-008) and that every session reports the
//! simulated [`TargetIdentity`] — hostname `SIMULATED-TARGET`,
//! `simulated == true` — so simulated results can never be mistaken for a
//! real customer computer (REQ-F-016).
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code, and the
//! clippy in-test exemptions do not reach top-level helper functions in an
//! integration-test crate.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use cretlink_console::{ExecRequest, SessionClient, SessionError};
use cretlink_protocol::{
    ErrorCode, EventKind, JobId, JobOutcome, OpResult, Operation, OutputStream, Shell,
};
use cretlink_simulator::Simulator;

/// Output cap for collected exec runs; ample for every scripted scenario.
const CAP: usize = 64 * 1024;

/// Serves `simulator` on one duplex half and connects a [`SessionClient`] to
/// the other.
async fn connect_with(simulator: Simulator) -> Result<SessionClient, SessionError> {
    let (sim_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = simulator.serve(sim_io).await;
    });
    SessionClient::connect(client_io, vec!["exec".to_string()]).await
}

/// Connects to a correctly-behaving simulator.
async fn connect() -> SessionClient {
    connect_with(Simulator::new())
        .await
        .expect("handshake against the simulator succeeds")
}

fn exec(command: &str) -> ExecRequest {
    ExecRequest::new(Shell::PowerShell, command)
}

/// Starts a `sim:hang` job through the raw request primitive (so the test can
/// drive cancel/stdin while it runs) and returns its job id.
async fn start_hang(client: &SessionClient) -> JobId {
    let started = client
        .request(Operation::Exec {
            shell: Shell::PowerShell,
            command: "sim:hang".to_string(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
        })
        .await
        .expect("sim:hang starts");
    match started {
        OpResult::ExecStarted { job, .. } => job,
        other => panic!("expected ExecStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_reports_simulated_identity() {
    let client = connect().await;

    // The HelloAck identity (REQ-F-016).
    assert!(client.target().simulated);
    assert_eq!(client.target().hostname, "SIMULATED-TARGET");
    assert_eq!(client.target().os_version, "Simulated Windows");
    assert_eq!(client.target().arch, "x86_64");
    assert!(!client.target().elevated);

    // SystemInfo reports the same identity under the same session.
    let (target, reboot_pending) = client.system_info().await.expect("system_info");
    assert_eq!(&target, client.target());
    assert!(!reboot_pending);
}

/// Two `system_info` calls on the console session client observe a flip of
/// the simulator's live pending-reboot flag — proof the console reports a
/// per-request roundtrip, never a cached value (ST-M003-003).
#[tokio::test]
async fn system_info_reports_the_live_reboot_flag() {
    let simulator = Simulator::new();
    let reboot_flag = simulator.reboot_flag();
    let client = connect_with(simulator)
        .await
        .expect("handshake against the simulator succeeds");

    let (_, first) = client.system_info().await.expect("first system_info");
    assert!(!first);

    reboot_flag.store(true, std::sync::atomic::Ordering::SeqCst);

    let (_, second) = client.system_info().await.expect("second system_info");
    assert!(
        second,
        "the flip must be visible: system_info is live, not a cache"
    );
}

#[tokio::test]
async fn sim_ok_completes_with_stdout() {
    let client = connect().await;
    let result = client
        .exec_collect(exec("sim:ok hello"), CAP)
        .await
        .expect("sim:ok collects");
    assert_eq!(result.outcome, JobOutcome::Completed);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout, b"hello\n");
    assert!(result.stderr.is_empty());
    assert!(!result.truncated);
    assert!(client.target().simulated);
}

#[tokio::test]
async fn sim_fail_reports_failed_with_exit_code() {
    let client = connect().await;
    let result = client
        .exec_collect(exec("sim:fail 7"), CAP)
        .await
        .expect("sim:fail collects");
    assert_eq!(result.outcome, JobOutcome::Failed);
    assert_eq!(result.exit_code, Some(7));
    assert!(!result.stderr.is_empty());
    assert!(result.stdout.is_empty());
}

#[tokio::test(start_paused = true)]
async fn sim_delay_completes_after_the_delay() {
    let client = connect().await;
    let before = tokio::time::Instant::now();
    let result = client
        .exec_collect(exec("sim:delay 5000"), CAP)
        .await
        .expect("sim:delay collects");
    assert_eq!(result.outcome, JobOutcome::Completed);
    assert_eq!(result.exit_code, Some(0));
    assert!(
        before.elapsed() >= Duration::from_millis(5000),
        "the delayed exit must not fire early (elapsed {:?})",
        before.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn sim_timeout_honours_the_requested_deadline() {
    let client = connect().await;
    let mut request = exec("sim:timeout");
    request.timeout_ms = Some(30_000);
    let before = tokio::time::Instant::now();
    let result = client
        .exec_collect(request, CAP)
        .await
        .expect("sim:timeout collects");
    assert_eq!(result.outcome, JobOutcome::TimedOut);
    assert_eq!(result.exit_code, None);
    assert!(
        before.elapsed() >= Duration::from_millis(30_000),
        "the timeout must not fire before the deadline (elapsed {:?})",
        before.elapsed()
    );
}

#[tokio::test]
async fn sim_hang_plus_cancel_reports_cancelled() {
    let client = connect().await;
    let mut events = client.subscribe();
    let job = start_hang(&client).await;

    client.cancel(job).await.expect("cancel is acknowledged");

    loop {
        match events.recv().await.expect("event stream stays open") {
            EventKind::JobExited {
                job: j,
                outcome,
                exit_code,
                ..
            } if j == job => {
                assert_eq!(outcome, JobOutcome::Cancelled);
                assert_eq!(exit_code, None);
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn cancel_of_an_unknown_job_surfaces_the_refusal() {
    let client = connect().await;
    // Stop paths fail closed: a cancellation the agent refuses must surface
    // as an error, never be silently swallowed as success.
    let error = client
        .cancel(JobId(9999))
        .await
        .expect_err("cancelling an unknown job must fail");
    assert!(
        matches!(
            error,
            SessionError::Remote {
                code: ErrorCode::UnknownJob,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn stdin_to_a_hung_job_is_echoed_on_stdout() {
    let client = connect().await;
    let mut events = client.subscribe();
    let job = start_hang(&client).await;

    client
        .write_stdin(job, b"interactive-ping".to_vec())
        .await
        .expect("write_stdin is acknowledged");

    let echoed = loop {
        match events.recv().await.expect("event stream stays open") {
            EventKind::JobOutput {
                job: j,
                stream: OutputStream::Stdout,
                data,
            } if j == job => break data,
            _ => {}
        }
    };
    assert_eq!(echoed.as_bytes(), b"interactive-ping");
}

#[tokio::test]
async fn sim_disconnect_reports_unknown_outcome() {
    let client = connect().await;
    let result = client
        .exec_collect(exec("sim:disconnect"), CAP)
        .await
        .expect("the started job resolves");
    // The connection dropped mid-job: the outcome is honestly unknown, never
    // a fabricated success (REQ-N-008).
    assert_eq!(result.outcome, JobOutcome::Unknown);
    assert_eq!(result.exit_code, None);

    // The lost session also fails later requests honestly.
    let error = client.system_info().await.expect_err("session is gone");
    assert!(matches!(error, SessionError::Disconnected), "got {error:?}");
}

#[tokio::test]
async fn sim_malformed_surfaces_a_malformed_message_error() {
    let client = connect().await;
    let error = client
        .exec_collect(exec("sim:malformed"), CAP)
        .await
        .expect_err("a length-valid but non-JSON frame must be rejected");
    assert!(
        matches!(
            error,
            SessionError::Remote {
                code: ErrorCode::MalformedMessage,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn unscripted_command_is_rejected_as_unsupported() {
    let client = connect().await;
    let error = client
        .exec_collect(exec("format c:"), CAP)
        .await
        .expect_err("only sim:* commands are scripted");
    assert!(
        matches!(
            error,
            SessionError::Remote {
                code: ErrorCode::Unsupported,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn list_jobs_reflects_running_and_finished_jobs() {
    let client = connect().await;
    let hanging_job = start_hang(&client).await;
    let finished = client
        .exec_collect(exec("sim:ok listed"), CAP)
        .await
        .expect("sim:ok collects");

    let jobs = client.list_jobs().await.expect("list_jobs");
    let hanging = jobs
        .iter()
        .find(|summary| summary.job == hanging_job)
        .expect("the hung job is listed");
    assert!(hanging.running);
    assert_eq!(hanging.command_preview, "sim:hang");

    let done = jobs
        .iter()
        .find(|summary| summary.job == finished.job)
        .expect("the finished job is listed");
    assert!(!done.running);
}

#[tokio::test]
async fn wrong_version_hello_ack_is_rejected() {
    let error = connect_with(Simulator::new().with_wrong_version())
        .await
        .expect_err("a different major version must not connect (REQ-N-012)");
    assert!(
        matches!(error, SessionError::IncompatibleVersion { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn oversized_hello_ack_frame_is_rejected() {
    let error = connect_with(Simulator::new().with_oversized_frame())
        .await
        .expect_err("a frame prefix over the cap must be rejected before allocation");
    assert!(
        matches!(error, SessionError::Oversized { .. }),
        "got {error:?}"
    );
}

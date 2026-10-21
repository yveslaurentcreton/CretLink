# CretLink manual tests

This document holds the **maintainer-run, hardware-in-the-loop** half of
CretLink's verification: the privileged failure, revocation, and cleanup
scenarios that cannot run in untrusted CI because they need a real elevated
Windows agent, a genuine UAC prompt, and a real network link (REQ-N-010,
REQ-N-011). It complements the automated adversarial suite, which is green on
every CI platform and is referenced by test name below.

Each scenario lists steps and an expected outcome. When a maintainer runs a
scenario on hardware, they record the date, host, and actual outcome in the
**Run log** at the bottom.

The reference agent is `cretlink-support-agent` on the customer computer; the
technician side is the console plus the local MCP bridge. Treat every path,
process, and file named here as belonging to the **remote target**, never the
technician machine.

## Running-job policy on stop

This is the policy the manual scenarios below exercise. It is implemented in the
session runtime and the execution broker (`crates/session/src/runtime.rs`,
`crates/exec/src/lib.rs`, `crates/exec/src/broker.rs`) and is the contract the
M003 milestone exit criterion refers to.

- **Revoke or stop cancels running work, then drains.** A technician stop, a
  customer revoke, or an expiry runs `Broker::cancel_all()` — every tracked job
  is cancelled — and then the session drains the channel for a bounded window
  (`TEARDOWN_DRAIN`, 2 s) so the peer reads the terminal `Revoke` and reports
  the honest reason rather than a bare disconnect. New tool calls are refused
  the instant control goes inactive; the stop path fails closed at the authority
  level even if a process cannot be killed (REQ-S-004, REQ-S-009, REQ-F-018).
- **Cancellation reaps the whole process tree.** On Windows each job is bound to
  a tracked Job Object; a cancel, timeout, or revoke terminates the entire tree
  via `TerminateJobObject`, not just the direct child (REQ-S-014).
- **An unkillable process is reported `Unknown`, never success.** If a job is
  still believed running when the session tears down, or a process is terminated
  by something other than the broker, its outcome is `JobOutcome::Unknown` — a
  crash, disconnect, or external kill is never dressed up as a clean exit
  (REQ-N-008).
- **The agent never orphans a job tree — graceful exit or hard kill.** Two
  independent mechanisms hold (REQ-S-009, REQ-S-014). Graceful: when the
  single-session agent's `run()` returns — the session ended, or the customer
  pressed Ctrl-C to revoke — the broker is dropped and `Spawned`'s explicit
  `Drop` → `kill_tree` → `TerminateJobObject` reaps any survivors. Hard kill: a
  `TerminateProcess` (e.g. `taskkill /f`) skips every Rust destructor, so the
  piped-mode Job Object now carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
  (ST-M006-007) — the kernel reaps the tree when the OS closes the agent's last
  job handle, matching the ConPTY backend. The hard-kill path is proven by the
  automated containment test
  `windows_hard_kill_reaps_the_whole_tree_via_kill_on_close`; see MT-3b for the
  hardware re-confirmation.
- **A job never outlives its session.** There is no separate SYSTEM helper in
  M003: the "privileged helper" is the in-process elevated broker of the
  single-session agent. When the session ends the agent process ends, so no job,
  broker, or access path survives it (REQ-S-012). A fresh session requires a
  full new pairing plus visible consent; a closed session is not re-joinable and
  reconnection restores nothing (REQ-S-007, REQ-S-009).
- **Disconnect and agent-exit behaviour.** A hard disconnect (Wi-Fi drop, cable
  pull, or killed agent) surfaces on the console as a terminal `Revoked` with
  reason `Error` — an honest non-success outcome — and any job still believed
  running is reported `Unknown`. The agent, on any exit reason (revoke,
  disconnect, protocol fault), autonomously sweeps its `OnSessionEnd` staging
  before finishing, and the next agent start sweeps at shutdown strength and
  retries any removal that failed, so a crash that skipped teardown is recovered
  (REQ-F-019, REQ-N-008).

### Automated coverage of this policy

The policy above is exercised in CI by, among others:

- `revoke_mid_job_reports_an_honest_outcome_never_success`,
  `revoke_mid_upload_is_honest_and_sweeps_staging`, and
  `simultaneous_bilateral_revoke_terminates_both_without_hang`
  (`crates/session/tests/e2e.rs`) — revoke races against a running job, against
  a mid-flight transfer, and against the other endpoint;
- `exec_cancel_terminates_the_job_as_cancelled` and the ConPTY containment
  tests (ST-M003-008) — cancellation reaps the tracked tree with zero
  survivors;
- `console_revoke_tears_down_both_fail_closed` (which also proves a closed
  session is **not re-joinable**), `agent_revoke_tears_down_both_fail_closed`,
  `disconnect_mid_session_is_honest_never_success`, and
  `disconnect_mid_upload_is_honest_never_success` — bilateral revoke, an honest
  mid-session disconnect, and an honest disconnect landing mid-transfer;
- `agent_run_loop_ends_after_a_single_session`
  (`crates/session/tests/e2e.rs`) — the agent's run loop ends after its one
  session and the console accepts no second connection, the automated half of the
  "the agent process ends after its session" sub-claim;
- `cleanup_failure_is_reported_then_retried_at_next_start`
  (`crates/exec/src/fs/tests.rs`) — a real removal failure is reported honestly
  and retried by the next startup sweep;
- `staging_path_traversal_is_rejected_at_the_mcp_surface`
  (`apps/console/tests/mcp_bridge_e2e.rs`) — a staging traversal over the MCP
  surface is refused as `invalid_path`.

The scenarios below verify the same policy against **real elevated hardware and
a real UAC prompt**, which CI deliberately never grants.

## Windows manual scenarios

Prerequisites: two Windows machines (or one customer VM and one technician
host) on the same LAN; the technician runs the console and the MCP bridge; the
customer runs `cretlink-support-agent`. Complete pairing and grant full-control
consent before each scenario unless the scenario says otherwise.

### MT-1 — UAC decline and accept

1. On the customer machine, launch the agent so it requests elevation.
2. **Decline** the UAC prompt.
   - Expected: the agent does not obtain elevation; no session reaches full
     control; the console shows no active elevated session; nothing runs
     (REQ-F-002, REQ-S-011, REQ-S-009).
3. Launch the agent again and **accept** the UAC prompt; complete pairing and
   consent.
   - Expected: `remote_system_info` reports `elevated = true` with the real host
     identity, and a subsequent `remote_exec` of `whoami /groups` shows the
     elevated administrator context (REQ-S-011).

### MT-2 — Revoke during an elevated job, with a survivor check

1. In an active elevated session, start a long-running job, for example
   `remote_exec` of `powershell -Command "Start-Sleep -Seconds 120"`, and note
   its job id and the child process id (`remote_list_jobs`, Task Manager, or
   `tasklist`).
2. Revoke from **either** endpoint (technician stop, or the customer presses
   Ctrl-C in the agent window).
   - Expected: the console reports the job's terminal outcome as `Cancelled` (or
     `Unknown` if the tree could not be confirmed killed), never `Completed`;
     the session ends `Revoked`; new tool calls fail closed.
3. Survivor check on the customer machine: confirm the job's process **tree** is
   gone. Use `tasklist` (the PID and its children are absent) and, if the Job
   Object handle is still inspectable, `QueryInformationJobObject`
   (`JobObjectBasicProcessIdList`) reporting zero active process ids.
   - Expected: no surviving process from the cancelled job; `TerminateJobObject`
     reaped the whole tree (REQ-S-014).

### MT-3 — Agent exit mid-job (graceful stop and hard kill)

This scenario separates two very different exits. Before ST-M006-007 they gave
two different containment outcomes; that story closed the gap, so both now reap
the tree — MT-3a through Rust destructors and MT-3b through the kernel's
kill-on-close Job Object.

**MT-3a — Graceful agent exit (the reaping property to verify).**

1. Start a long-running elevated job as in MT-2 and note the child PID and its
   tree.
2. Exit the agent **gracefully** so its `run()` returns and Rust destructors
   run: press Ctrl-C in the agent window (it revokes the session), or close the
   window through the normal close path.
   - Expected: the console surfaces a terminal non-success outcome — `Revoked`,
     and the running job reported `Cancelled` or `Unknown` — never a silent
     `Completed` (REQ-N-008).
3. Survivor check on the customer machine (`tasklist`, and
   `QueryInformationJobObject` if inspectable): confirm the job's whole process
   tree is gone.
   - Expected: no survivors. Dropping the broker runs `Spawned`'s explicit
     `Drop` → `kill_tree` → `TerminateJobObject`, which reaps the tree as the
     fail-closed net (REQ-S-009, REQ-S-014).

**MT-3b — Hard kill of the agent (a verified containment guarantee since
ST-M006-007).**

1. Start a long-running elevated job as in MT-2 and note the child PID and its
   tree.
2. **Hard-kill** the agent process so destructors are skipped:
   `taskkill /f /im cretlink-support-agent.exe`, or End Task in Task Manager
   (both call `TerminateProcess`).
   - Expected (console): an honest terminal outcome as in MT-3a — `Revoked`
     with reason `Error`, and the in-flight job reported `Unknown`, never
     `Completed` (REQ-N-008). The **honesty** property holds under a hard kill.
3. Survivor check on the customer machine (`tasklist`, and
   `QueryInformationJobObject` if inspectable).
   - Expected: **no survivors**. Since ST-M006-007 the piped-mode job runs under
     CretLink's own Job Object carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (see
     `crates/exec/src/platform.rs`), so when the OS closes the agent's last job
     handle on `TerminateProcess`, the kernel reaps the whole tree — no Rust
     destructor required. This matches the ConPTY backend (ST-M003-008), which
     already carried the flag.
   - **Verification status:** the mechanism is proven by the automated
     containment test
     `windows_hard_kill_reaps_the_whole_tree_via_kill_on_close` in
     `crates/exec/src/broker/tests.rs`, which reproduces a destructor-skipping
     hard kill in-process (forget the child, close only the job handle) and
     asserts the grandchild dies. This manual MT-3b is the on-hardware
     re-confirmation across the two real machines; the earlier Run-log entry that
     observed a survivor recorded the **pre-ST-M006-007** behaviour and is
     superseded.

### MT-4 — Residual check after a session

1. Complete a session that stages an artifact (`remote_upload` with
   `on_session_end`) and runs one or more jobs; then end the session normally.
2. On the customer machine, verify **nothing CretLink placed survives**:
   - no `cretlink-support-agent` process running (`tasklist`);
   - no CretLink service and no auto-start entry (`sc query`, `Get-Service`,
     the `Run`/`RunOnce` registry keys, and the Startup folder);
   - the session's staging tree under
     `%ProgramData%\CretLink\staging\<session-id>` is gone (its `on_session_end`
     artifact removed).
   - Expected: no active access path remains; any residual is reported by the
     agent, not left silently (REQ-F-001, REQ-F-019, REQ-S-012).

### MT-5 — Locked-file cleanup failure and retry

1. In an active session, `remote_upload` an artifact with `on_session_end`.
2. On the customer machine, hold the staged file open **without share-delete**
   so its removal is refused — for example open it in a program that keeps an
   exclusive handle, or run
   `powershell -Command "$f=[IO.File]::Open('C:\\ProgramData\\CretLink\\staging\\<session-id>\\<name>','Open','Read','None'); Read-Host 'holding'"`.
3. End the session while the handle is held.
   - Expected: the agent's cleanup reports the artifact as a **failed** removal
     with an honest reason (in use), never as success; the console shows the
     residual prominently and the manifest record is kept (REQ-F-019,
     REQ-N-008).
4. Release the handle and start the agent again.
   - Expected: the startup sweep **retries** the removal and succeeds, the
     artifact and its spent session tree are gone, and the retry is reported
     honestly. (This is the hardware analogue of
     `cleanup_failure_is_reported_then_retried_at_next_start`.)

### MT-6 — Network disconnect mid-job

1. Start a long-running elevated job in an active session.
2. Break the network between the two machines mid-job (disable the technician
   NIC, pull the cable, or drop Wi-Fi).
   - Expected: the console surfaces an honest terminal outcome — `Revoked` with
     reason `Error`, and the in-flight job reported `Unknown` — never
     `Completed`; the console fails closed and refuses new work (REQ-N-008,
     REQ-S-009).
3. On the customer machine, confirm the agent tore its session down and did not
   leave a running job or a standing access path; a later reconnect is a **new**
   pairing that requires fresh consent (REQ-S-007, REQ-S-009).

### MT-7 — `cretlink-shell` host-terminal profile smoke (ST-M008-002)

This exercises the standalone `cretlink-shell` command-line client as a real
host-terminal profile against a consented session — the interactive ergonomics
and raw-mode host that the automated engine tests cannot cover (the security
duplex itself is proven headlessly by `apps/shell-cli/tests/engine.rs` and the
real-IPC fail-closed by `apps/shell-cli/tests/ipc_closed.rs`).

Prerequisites as above, plus **Windows Terminal** and/or **WezTerm** installed
on the technician machine. The technician runs the console and the local MCP
bridge; a full-control session is consented on the customer machine.

1. Add a profile that launches the client against the running console's
   same-user IPC endpoint, for example a Windows Terminal profile with
   `commandline` set to `cretlink-shell.exe` (optionally
   `cretlink-shell.exe --shell cmd` or `--endpoint <name>`), and an equivalent
   WezTerm `spawn`/`prog` entry. Open the profile.
   - Expected: the client prints its target-and-session identity banner on
     stderr (hostname, session id), then drops into the remote pseudo-console.
     stdout carries only the remote terminal's VT output.
2. Run an interactive program on the target, for example `powershell` then a
   full-screen tool, and type into it.
   - Expected: keystrokes stream to the remote terminal and its output renders
     locally in real time (raw mode, VT in/out); responsive streamed output
     (REQ-N-005).
3. Resize the host terminal window.
   - Expected: the remote terminal reflows to the new size (a `TermResize` is
     forwarded); no restart, no lost session.
4. Press **Ctrl-C** inside the remote program.
   - Expected: the interrupt reaches the **remote** program (it is not consumed
     locally); the local client keeps running.
5. Press **Ctrl-]** (the detach hotkey, `0x1d`).
   - Expected: the client detaches and exits cleanly, printing a `detached`
     summary on stderr; the remote terminal keeps running for other consumers
     (the console panel or a re-attach), matching the tmux/telnet-escape model.
6. No-consent / revoke checks:
   - With **no** session consented, open the profile.
     - Expected: the client fails closed with a no-consent message and a
       non-zero exit, opening nothing (REQ-S-009, REQ-S-023).
   - Re-attach, then **revoke or stop** from either endpoint while attached.
     - Expected: the client ends with an honest `revoked` (or `stopped`) summary
       and a non-zero exit — never a fabricated clean close (REQ-N-008); a
       re-open after the revoke fails closed.
7. No-secret check: confirm the client copies **no** provider token, login
   state, or session key to the customer machine — it authenticates purely by
   the kernel-enforced same-user IPC and borrows the routed session's authority
   (REQ-S-015). Inspect the target for any CretLink-written credential material;
   expect none.

### MT-8 — Reboot-resume: arm, survive a reboot, reconnect once (ST-M006-001/002/003)

This exercises the one part of reboot-resume that CI cannot grant: registering a
customer-side OS auto-start under a real UAC prompt, surviving a genuine reboot,
and reconnecting exactly once. The cryptographic ticket, its fail-closed matrix,
single-use consumption, at-rest sealing (through the test sealer), and residual
inventory are proven headlessly by the ST-M006-002 fail-closed matrix and the
ST-M006-003 inventory/fault-injection tests; this scenario confirms the
end-to-end behaviour on hardware per
ADR-0014.

> **Not yet runnable.** The agent does not yet register the auto-start entry or
> run the proof during a real post-reboot reconnect, so this scenario cannot
> pass until that wiring exists. It stays here as the acceptance procedure.

Prerequisites as above; a full-control session is consented on the customer
machine before arming.

1. In the active consented session, **arm reboot-resume** through the attended
   local flow on the agent window.
   - Expected: the agent's disclosure names the expiry, single-use behaviour, and
     the cleanup that follows; **both** endpoints then show that reboot-resume
     authority exists and its expiry (REQ-N-013 through REQ-N-015). Arming
     requested from the MCP surface alone, with no attended grant, is refused
     (REQ-S-001 through REQ-S-004).
2. Confirm arming created exactly the sanctioned persistence and nothing more:
   one per-user OS auto-start entry (scheduled task or `RunOnce`) and one sealed
   ticket file; no CretLink service, no listener, and no control-bearing state
   (`sc query`, `Get-ScheduledTask`, the `Run`/`RunOnce` keys).
3. **Reboot** the customer machine.
   - Expected: after login the OS auto-start relaunches the agent once; it
     re-dials and runs a **fresh** Noise XX handshake, then proves continuity
     with the `K_rr` challenge-response. The console recognises and pre-selects
     the returning target.
4. Complete the **attended re-consent** on the agent.
   - Expected: only after the human re-consents does the session reach full
     control again; reconnection never restored control silently (REQ-F-006,
     REQ-S-009). The ticket and the OS auto-start entry are now **consumed and
     removed** — a second reboot does **not** reconnect (single-use).
5. Fail-closed spot checks on hardware (the headless matrix already proves these;
   confirm one or two live):
   - Arm, then let the expiry elapse **before** rebooting: after reboot the
     reconnect is refused and the ticket is reaped (expired).
   - Arm, copy the sealed ticket file to a second machine, and attempt a
     reconnect from it: refused (target-bound, single-use, sealed per-user).
6. Residual check (the MT-4 discipline, extended): after a consumed reconnect, or
   after an armed-but-expired ticket, run the **recovery command** and confirm it
   reports and removes every CretLink residual — staged trees, the ticket file,
   and the OS auto-start entry — without hiding a failure (REQ-F-019, REQ-S-012,
   REQ-N-008). A cleanup that cannot remove something reports it prominently and
   keeps it for the next startup sweep.

### MT-9 — Desktop observation and control: honest states on real hardware (ST-M005-006)

This exercises the one half of the desktop op-set that only a real attended
Windows desktop can prove: that a **live capture** is honest across the states a
customer machine actually enters — lock screen, secure desktop, session switch,
display and focus changes — and that synthesized input is **paced, attributed,
and fails closed** on the same handoffs. The capability gate, the backend
fail-closed behaviour off Windows, the honest `DesktopState` frame invariant
(every non-`Active` state carries no image), the malformed/oversized action
taxonomy, input pacing, the `DesktopActivity` signal, and fail-closed behaviour
across a mid-session network interruption are already proven headlessly on every
CI platform by, respectively, `desktop_without_capability_is_unsupported`,
`desktop_enabled_but_backend_is_unsupported`, `round_trip_response_desktop_frame`
(protocol), the `desktop_*_are_rejected` protocol tests,
`a_synthesized_input_flood_is_paced_not_dropped`,
`desktop_activity_wire_tag_is_snake_case` (protocol), and
`desktop_op_after_network_interruption_fails_closed`. The live-capture honesty
itself is guarded by the `#[ignore]`d `live_windows_capture_is_honest`, which the
maintainer runs here on hardware.

Prerequisites as above; a full-control session with the `desktop.v1` capability
is consented on the customer machine. The console originates every desktop op;
the customer agent services it locally and is the authority on what the desktop
was doing.

1. **Live-capture honesty on an Active desktop.** With the customer at their
   normal interactive desktop, run the live smoke test on the customer machine:
   `cargo test -p cretlink-session --test e2e -- --ignored live_windows_capture_is_honest`.
   - Expected: it passes — an `Active` desktop returns real PNG bytes with
     positive dimensions; the test never synthesizes input, so it cannot disturb
     the desktop (REQ-N-008).
2. **Lock screen.** Capture, then lock the customer machine (Win+L) and capture
   again.
   - Expected: the locked capture returns `DesktopState::Locked` with **no
     image** — never a stale or fabricated frame; an input action aimed at the
     locked desktop is refused, not silently accepted (REQ-N-008, REQ-S-009).
3. **Secure desktop / UAC.** Trigger a UAC elevation prompt (or Ctrl-Alt-Del) so
   the Secure Attention Sequence takes over, and capture.
   - Expected: the state is reported as `Secure` (or `NoInputDesktop`) with no
     image, and no input can be driven into the credential surface — the secure
     desktop is out of reach *by design*, and CretLink reports it rather than
     driving it (REQ-S-013, REQ-N-008). The attending human, not CretLink,
     completes the prompt.
4. **Session switch / disconnect.** Switch users (or start an RDP session that
   disconnects the console session) and capture.
   - Expected: the state reflects the handoff (`Switched` / `Disconnected`) with
     no image; input queued around the switch never lands on the wrong desktop
     (REQ-S-004, REQ-S-009).
5. **Display and focus changes.** Change the display layout (unplug a monitor,
   change resolution/scaling) and switch the foreground window, capturing across
   each change.
   - Expected: each `Active` frame reflects the **current** geometry and content
     with an honest timestamp and dimensions; a stale frame is never presented as
     live (REQ-N-008).
6. **Rapid input is paced, visible, and attributable.** Drive a burst of pointer
   and keyboard actions faster than the input governor's ceiling and watch the
   customer screen.
   - Expected: every action lands (none silently dropped) but is paced to the
     governor's sustained rate; each is an ordinary, visible pointer/keyboard
     action attributable to a specific tool call — no keystroke-logging or
     credential-scraping primitive exists to invoke (REQ-N-006, REQ-S-013,
     REQ-S-017).
7. **Consent visibility.** Throughout, confirm the customer can tell when the
   screen is being **observed** and when input is under **control**, distinguished
   without relying on colour alone, and that the disclosure was shown before any
   screen content left the customer machine (REQ-N-013, REQ-N-014, REQ-S-016).
   A graphical indicator is not yet implemented; the CLI surfaces print a
   colour-independent Observe/Control line.
8. **No-secret / no-covert check.** Confirm the session copied **no** provider
   token, login state, or session key to the customer machine, and that the
   desktop surface exposed no hidden capture or keylogging path — only the
   discrete, audited ops actually invoked (REQ-S-013, REQ-S-015).

### MT-E2E — Full attended-LAN acceptance walk-through (ST-M006-006)

The scenarios above are targeted failure and edge slices; this one is the single
contiguous **happy-path acceptance run** the M006 exit criterion names ("a fresh
supported environment completes the documented end-to-end support workflow using
official artifacts"). Run it start-to-finish, in order, from a **fresh
environment** on each supported platform class, against **official downloaded
artifacts** whose checksums (and, when signed, Authenticode signature) were first
verified per [`verifying-artifacts.md`](verifying-artifacts.md). Record the run in
the sign-off in [`release-signoff.md`](release-signoff.md), not only the Run log
below.

Prerequisites: a customer machine and a technician machine on the same LAN, both
freshly prepared (no prior CretLink state); official artifacts for each side
downloaded and integrity-verified. Do **not** pre-pair or pre-consent — this
scenario exercises those steps.

1. **Launch.** On the customer machine start `cretlink-support-agent`; on the
   technician machine start the console and the `cretlink-mcp` bridge under the
   MCP client, all as the same OS user (REQ-F-013).
   - Expected: the agent shows its process and connection state openly — no
     stealth, no hidden window or tray state (REQ-S-002).
2. **UAC.** Accept the agent's elevation prompt.
   - Expected: elevation is visible and truthful; a later `remote_system_info`
     will report `elevated = true` for the real target (REQ-F-002, REQ-S-011).
3. **Discovery, then the manual fallback.** Let the console discover the agent on
   the LAN and note it; then **also** exercise the manual-address path by
   entering the target address directly.
   - Expected: discovery is only a hint that carries no authority; the manual
     fallback follows the identical pairing + code path and never bypasses
     authentication (REQ-F-004, REQ-S-001, REQ-S-010).
4. **Pairing + code comparison.** Complete pairing and **compare the Session
   Approval Code on both endpoints**, confirming only when they match.
   - Expected: both endpoints display the same transcript-bound SAC; a mismatch
     cannot reach full control (REQ-F-005, REQ-S-006). Confirm the deliberate
     two-step separation — code comparison is distinct from consent.
5. **Consent.** Grant full-control consent on the customer machine, reading the
   disclosure.
   - Expected: the consent wording states the real authority granted and
     discloses that command output and selected files may flow to the
     technician's AI service (REQ-S-003, REQ-S-016, REQ-F-006); only after this
     does the session reach `Active`.
6. **MCP control + identity.** From the MCP client, list tools and run
   `remote_system_info`.
   - Expected: exactly the twenty-four `remote_*` tools; the result carries the
     **target's** identity and the routed `session_id`, never the technician
     host (REQ-F-016, REQ-N-001).
7. **Execution.** Run a portable `remote_exec` (for example `whoami /groups`).
   - Expected: honest exit code, output, duration, and elevated execution
     identity (REQ-F-007, REQ-S-011); the action is visible in the console
     activity view and audited with the routed `call_id` (REQ-F-017, REQ-S-017).
8. **Transfer.** `remote_upload` a file into staging, then `remote_download` it
   back, and compare digests.
   - Expected: length + SHA-256 verified in both directions before completion is
     reported; a mismatch would be surfaced, never silently accepted (REQ-N-007).
9. **Stop.** Stop from the technician side (and, on a second pass, revoke from the
   customer side).
   - Expected: new tool calls fail closed immediately; any running work is
     cancelled or honestly reported, and session secrets are invalidated
     (REQ-F-018, REQ-S-004, REQ-S-007, REQ-S-009).
10. **Cleanup + residual check.** Confirm the session's staging tree is removed
    and no auto-start entry, service, or standing access path remains (the MT-4
    discipline); if anything could not be removed, confirm it is reported, not
    hidden (REQ-F-001, REQ-F-019, REQ-S-012, REQ-N-008).
11. **No-secret check.** Confirm no provider token, API key, or login state was
    written to any CretLink-managed path on the target (REQ-S-015, REQ-F-015).

A platform class passes MT-E2E only when every step meets its expected outcome on
official artifacts from a fresh environment. Record the pass (date, host, agent
build, artifact provenance) in [`release-signoff.md`](release-signoff.md).

## Run log

Record each hardware run here: date, host, agent build, scenarios run, and the
actual outcome. An empty row means the scenario has not yet been run on
hardware.

| Date | Host | Agent build | Scenarios | Outcome |
| --- | --- | --- | --- | --- |
| 2026-08-12 | Target: Windows 11 Home test laptop (Win 11 Home 22H2, 22621.900, elevated, non-simulated); console + Claude Code 2.1.228 on Win 11 Pro (10.0.26200) | development build (agent self-reported `0.0.0`) | MT-3a — graceful agent exit (Ctrl-C, reaping property) | PASS. Console reported the running job as `cancelled` with `exit_code` null (never `Completed`); stdout ended at the process's `Control-C` line. Survivor check (`tasklist /fi "imagename eq ping.exe"`) showed **zero** PING.EXE. `Spawned::Drop` → `kill_tree` → `TerminateJobObject` reaped the whole tree (REQ-S-009/REQ-S-014, REQ-N-008). |
| 2026-08-12 | Target: Windows 11 Home test laptop (as above); console + Claude Code 2.1.228 | development build (agent self-reported `0.0.0`) | MT-3b — hard kill (`taskkill /f /im cretlink-support-agent.exe`) | One survivor observed — expected before ST-M006-007; superseded, re-run pending (row below). |
| _pending_ | | | MT-3b re-run (hard kill, expect **zero** survivors under ST-M006-007) | not yet recorded on hardware |
| _pending_ | | | MT-1 (UAC decline/accept), MT-2 (revoke + survivor), MT-4 (residual after session), MT-5 (locked-file cleanup + retry), MT-6 (network disconnect mid-job) | not yet recorded on hardware |
| _pending_ | | | MT-7 (`cretlink-shell` host-terminal profile smoke: interactive/resize/Ctrl-C-to-remote/Ctrl-] detach, no-consent + revoke fail-closed, no-secret check) | not yet recorded on hardware |
| _pending_ | | | MT-8 (reboot-resume: arm under UAC, survive a reboot, one attended reconnect via the `K_rr` proof, single-use consumption, expired/cloned refusal, residual removal via the recovery command — ST-M006-001/002/003) | not yet recorded on hardware |

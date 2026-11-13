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

The customer side is the application `cretlink.exe` in its **Get help** role on
the customer computer; the technician side is `cretlink` in its **Give help**
role plus the local MCP bridge, `cretlink mcp`, which the MCP client starts.
Where a scenario needs something only the headless development tools offer
(`cretlink-support-agent` for reboot-resume arming), it says so; those tools are built from the source tree and are not in the release
archives. Treat every path, process, and file named here as belonging to the
**remote target**, never the technician machine.

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
host) on the same LAN; the technician runs `cretlink` (Give help) and an MCP
client that starts `cretlink mcp`; the customer runs `cretlink.exe` and chooses
**Get help**. Complete pairing and grant full-control consent before each
scenario unless the scenario says otherwise.

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
   `taskkill /f /im cretlink.exe` on the customer machine, or End Task in Task
   Manager
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
   - no `cretlink.exe` process running (`tasklist`);
   - no CretLink service and no auto-start entry (`sc query`, `Get-Service`,
     the `Run`/`RunOnce` registry keys, and the Startup folder);
   - the session's staging tree under
     `C:\Program Files\CretLink Data\staging\<session-id>` is gone (its
     `on_session_end` artifact removed). Look from an administrator
     PowerShell: the folder is closed to every other account. A session that
     ran without administrator rights keeps its staging tree under
     `%LOCALAPPDATA%\CretLink\staging` instead.
   - Expected: no active access path remains; any residual is reported by the
     agent, not left silently (REQ-F-001, REQ-F-019, REQ-S-012).

**MT-4b — CretLink's own folders are closed to other accounts (ST-M006-020).**
It needs two PowerShell windows on the customer computer: one opened as
administrator, and one that is not elevated (of an account without
administrator rights, or of the same account). *The recovery command* below
is the one the program shows, without `--remove` unless a step says so:
`& '<folder>\cretlink-cli.exe' recover`, or
`& '<folder>\CretLink-Support.exe' recover | Out-Host`.
Start from a clean computer: run the recovery command with `--remove` in the
administrator PowerShell first. It removes what earlier sessions kept, also a
folder `C:\ProgramData\CretLink` that an earlier release left, whose owner
would otherwise decide what step 2 shows.

1. In the administrator PowerShell, create a folder
   `C:\Program Files\cretlink-test` and a text file `important.txt` in it: a
   file that an account without administrator rights cannot remove. In the
   PowerShell that is not elevated, plant what a release up to 0.24.0 could
   have been made to read: create the folder
   `C:\ProgramData\CretLink\staging\11111111-2222-4333-8444-555555555555` and
   in it a file `manifest.json` with this content:
   `{"version":1,"staged":[{"canonical_path":"C:\\Program Files\\cretlink-test\\important.txt","source":"x","len":0,"algo":"sha256","digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","cleanup":"on_session_end","created_ms":0}]}`
2. Start **Get help** and accept the UAC prompt.
   - Expected: `important.txt` is still there. The folder
     `C:\ProgramData\CretLink` is still there too, because an account owns
     it, and *Who is helping you?* shows the notice about an earlier session
     with a command to run as administrator.
3. Close CretLink and run the recovery command with `--remove` in the
   administrator PowerShell.
   - Expected: it names the folder of an earlier release and removes it, and
     `important.txt` is still there.
4. In the administrator PowerShell, create the same folder and manifest
   again, so that the administrators own `C:\ProgramData\CretLink`
   (`(Get-Acl C:\ProgramData\CretLink).Owner`). Start **Get help** and accept
   the UAC prompt.
   - Expected: `important.txt` is still there, the folder
     `C:\ProgramData\CretLink` is gone, and *Who is helping you?* shows no
     notice about an earlier session. Remove `C:\Program Files\cretlink-test`
     afterwards.
5. In the administrator PowerShell run
   `icacls "C:\Program Files\CretLink Data"`,
   `icacls "C:\Program Files\CretLink Data\staging"` and
   `(Get-Acl "C:\Program Files\CretLink Data").Owner`.
   - Expected: each folder lists `NT AUTHORITY\SYSTEM:(OI)(CI)(F)` and
     `BUILTIN\Administrators:(OI)(CI)(F)` and nobody else, and the owner is
     `BUILTIN\Administrators`.
6. In the PowerShell that is not elevated run
   `New-Item "C:\Program Files\CretLink Data\staging\x.txt"` and
   `Get-ChildItem "C:\Program Files\CretLink Data"`.
   - Expected: both are refused.
7. Let the technician upload a file into staging that is kept
   (`remote_upload` with `keep`), end the session and close CretLink. Run the
   recovery command in the PowerShell that is not elevated.
   - Expected: it does not call the computer clean. It says that what
     CretLink kept with administrator rights can only be read and removed by
     this command run as an administrator.
8. Run the recovery command in the administrator PowerShell, then again with
   `--remove`.
   - Expected: the first lists the kept file; the second removes it and ends
     with *this machine is clean*, and both say that the folders of the
     accounts are not covered. `C:\Program Files\CretLink Data` no longer
     exists, and the command of step 7 now calls the computer clean.
9. Copy `C:\Windows\System32\cmd.exe` into the folder of the downloaded
   CretLink program under the name `powershell.exe`, and put a file
   `ipconfig.bat` with the single line `@echo planted` beside it. In the
   PowerShell that is not elevated, go to that folder, run
   `$env:CRETLINK_TEST = 'planted'` and start **Get help** from there
   (`& .\CretLink-Support.exe`); accept the UAC prompt. Let the technician
   run `$PSVersionTable.PSVersion.Major` in PowerShell, open a PowerShell
   terminal, and run `cd`, `ipconfig` and `echo %CRETLINK_TEST%` in Command
   Prompt without naming a working directory. Remove both files afterwards.
   - Expected: the PowerShell command prints `5`, and the terminal shows a
     PowerShell prompt, not the banner of Command Prompt: the planted program
     was not started. `cd` prints the system folder
     (`C:\Windows\System32`), `ipconfig` prints the network configuration
     and not `planted`, and the last command prints `%CRETLINK_TEST%`
     unchanged: the job runs in neither the folder nor the environment of
     the program that asked for the elevation.
10. Start **Get help**, decline the UAC prompt and go on without administrator
   rights; let the technician upload a file into staging.
   - Expected: the upload works, and its staging tree lies under
     `%LOCALAPPDATA%\CretLink\staging`.

### MT-5 — Locked-file cleanup failure and retry

1. In an active session, `remote_upload` an artifact with `on_session_end`.
2. On the customer machine, hold the staged file open **without share-delete**
   so its removal is refused — for example open it in a program that keeps an
   exclusive handle, or run, in an administrator PowerShell (the staging
   folder is closed to every other account),
   `powershell -Command "$f=[IO.File]::Open('C:\\Program Files\\CretLink Data\\staging\\<session-id>\\<name>','Open','Read','None'); Read-Host 'holding'"`.
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

### MT-7 — `cretlink-cli shell` host-terminal profile smoke (ST-M008-002)

This exercises the command-line terminal client `cretlink-cli shell` as a real
host-terminal profile against a consented session — the interactive ergonomics
and raw-mode host that the automated engine tests cannot cover (the security
duplex itself is proven headlessly by `apps/cli/tests/shell_engine.rs` and the
real-IPC fail-closed by `apps/cli/tests/shell_ipc_closed.rs`).

Prerequisites as above, plus **Windows Terminal** and/or **WezTerm** installed
on the technician machine. The technician runs `cretlink` (Give help); a
full-control session is consented on the customer machine.

1. Add a profile that launches the client against the running console's
   same-user IPC endpoint, for example a Windows Terminal profile with
   `commandline` set to `cretlink-cli.exe shell` (optionally
   `cretlink-cli.exe shell --shell cmd` or `--endpoint <name>`), and an equivalent
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

Prerequisites as above, except that the customer side is the development tool
`cretlink-support-agent`: the application does not offer reboot-resume arming
yet. The recovery command is the application's (`cretlink recover`, also
`cretlink-cli recover` or a `CretLink-Support*` copy's `recover`). A full-control session is consented on the
customer machine before arming.

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
   after an armed-but-expired ticket, run the **recovery command**
   (`cretlink recover`, then `cretlink recover --remove`, from an elevated
   terminal) and confirm it
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

### MT-10 — End reports and the customer's record (ST-M009-012)

1. Run a Give help session against a Get help customer on Windows, with at
   least one command, one file each way, and (as in MT-5) one staged
   artifact held open so it cannot be removed. Stop from the technician side;
   on a second pass, from the customer side; on a third, unplug the network.
   - Expected on the technician side: the end screen and the Sessions page
     say who ended it, and the cleanup result is **1 residual** naming the
     held file on the first two passes and **Cleanup unknown** on the third,
     never "Cleaned up" (REQ-F-027, REQ-F-019). **Open audit log** opens that
     session's `.jsonl`, which ends with `session_ended`, `cleanup_report` and
     the seal.
2. On the customer computer, before using **Save a copy**, search the
   customer's profile, `%ProgramData%`, `C:\Program Files\CretLink Data` (as
   an administrator), `%TEMP%` and the folder CretLink ran from for any file
   written during the session (for example with
   `Get-ChildItem -Recurse | Where-Object LastWriteTime -gt <start>`).
   - Expected: no session record or report exists, and `Documents\CretLink`
     does not exist (REQ-F-027, REQ-F-001).
3. Choose **Save a copy**, then cancel the dialog.
   - Expected: the dialog opened on `Documents\CretLink\` with
     `YYYY-MM-DD <hostname>.html`; after cancelling, no file and no
     `Documents\CretLink` folder remain.
4. Choose **Save a copy** again and save; then **Open**.
   - Expected: one HTML file opens in the browser with no network access
     needed; it holds the activity the window showed, the cleanup notes with
     the residual first, and no approval code, connection code or key.

### MT-11 — Install, update and uninstall the technician role (ST-M009-006)

The per-user install of Give help on a real Windows and macOS computer (Linux
is covered by the automated tests over a temporary home directory). Use a
release archive, or a local build with both `cretlink` and `cretlink-cli` in
one folder, for example `Downloads`. Take screenshots of the start screen,
the install screen and the uninstall prompt.

1. **Start screen.** Start `cretlink` from `Downloads`. Windows shows Get help
   and Give help, macOS only Give help; Give help offers **Install for me**
   and **Run once without installing**, and the screen shows the version and
   the `Downloads` folder.
2. **Run once.** Choose **Run once without installing**. The technician window
   says the copy runs once; on AI assistants, Connect warns that the
   registration points at this copy. Close it. Expect nothing new outside the
   audit folder (`%LOCALAPPDATA%\CretLink\audit`, `~/.local/share/CretLink/audit`
   or the macOS equivalent) other than the webview profile (see the README's
   known gaps).
3. **Install.** Start it again and choose **Install for me**. The screen shows
   the location (`%LOCALAPPDATA%\Programs\CretLink` or
   `~/Applications/CretLink.app`), the Start-menu option (Windows), and each AI
   client with what was found and the exact command. Select Claude Code (and
   Codex CLI if present) and press **Install**. Expect: no UAC prompt; the
   installed copy opens in the technician role and the first window closes;
   `cretlink mcp status` from a terminal shows the registrations pointing at
   the installed program; on Windows, a *CretLink* Start-menu entry that opens
   Give help, and *CretLink* in *Settings → Apps → Installed apps* with the
   version and publisher; on macOS, `CretLink` in `~/Applications` that opens
   from Finder (Gatekeeper may ask, the build is unsigned). No service, no
   scheduled task, no Run key and no login item was added (Windows:
   `Get-ScheduledTask`, `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`;
   macOS: *Login Items*).
4. **Running copy.** With the installed copy open, start the `Downloads` copy
   and press **Install** again. Expect the install screen to say the installed
   copy is running, and **Install** to be unavailable.
5. **Update.** Close the installed copy, replace the `Downloads` copy with a
   newer build and install again. Expect the version in Settings to change and
   the AI clients to keep working without reconnecting.
6. **Uninstall.** In the installed copy, open Settings → **Uninstall…**. Expect
   the list of what it removes (files, shortcut, uninstall entry, the
   registrations) and that the audit trail stays. Confirm, close the window,
   and expect the folder (Windows: after the window closed), the shortcut, the
   *Installed apps* entry and the registrations to be gone, and the audit
   folder to be kept.
7. **Uninstall entry and CLI.** Install again, then uninstall from *Installed
   apps* (Windows): a message box lists what will be removed and asks; **No**
   changes nothing, **Yes** removes it. On macOS run
   `~/Applications/CretLink.app/Contents/MacOS/cretlink-cli uninstall` and
   answer `y`.
8. **Changed by hand.** Install with Claude Code, point its `cretlink`
   registration at another path by hand, then uninstall. Expect that
   registration to be left alone and the uninstall to say so.
9. **Customer copy.** Rename a copy to `CretLink-Support.exe`; `CretLink-Support
   uninstall` and `CretLink-Support install` are refused, and its window never
   offers to install. `CretLink-Support recover` lists what CretLink left on
   the computer and names `CretLink-Support recover --remove` (ST-M009-010).

### MT-12 — The customer interface on Windows (ST-M009-009)

A walk-through of Get help on a real Windows computer against Give help on a
second computer, compared with the approved mock, with screenshots of every
screen (`#c-admin`, `#c-find`, `#c-sac`, `#c-consent`, `#c-live`, `#c-mini`,
`#c-end`). Run it once with Narrator on and record that pass too.

1. **Before the prompt.** Start `cretlink` and choose **Get help**. Expect the
   new window to show *Windows will ask for permission* with four steps and
   *Nothing happens to this computer before step 4, and nothing is
   installed*, before any UAC prompt. **Back** returns to the start screen.
   Rename a copy to `CretLink-Support.exe` and start it: the same screen, and
   **Close** closes it.
2. **Declined.** Choose **Get help**, **Continue**, and answer **No** in the
   UAC prompt. Expect *Windows did not get permission* and *Administrator
   tasks will not work*, with **Try again** and **Continue without admin
   rights**. Choose **Continue without admin rights**: the technician list
   shows *Admin rights: No*, and a session's `remote_system_info` reports
   `elevated = false` (REQ-S-011).
3. **Accepted.** Start again, **Continue**, answer **Yes**. Expect the first
   window to close and an elevated window to open on the technician list with
   *Admin rights: Yes, approved in Windows* and no second explanation.
4. **Choose your technician.** Start a session on the technician computer.
   Expect it in the list with its name and host name, its address and *this
   network*, nothing selected and nothing connecting until **Connect**; the
   code field is there either way. The footer reads *Nothing is installed ·
   closing CretLink ends everything*. With no session, `Escape` does nothing
   and there is no Stop.
5. **Compare and decide.** Expect the same six digits on both sides,
   *It doesn't match — stop* (`Escape`), then *Allow <name> full control?*
   with the summary above the full disclosure; Tab reaches **Decline** and
   **Allow full control** without either being focused first.
6. **Under full control.** Let an AI client run a command, send a file to
   the customer, copy a file from it and fail one command. Expect *Happening
   now* while each runs, the Files and Commands tiles counting, the copied
   file marked *left this computer*, and the filters showing only their
   kind.
7. **Compact bar.** Press **Make small**: the main window goes and a small bar
   stays on top of other windows, can be dragged, and shows the state, the
   current action and the counts. `Alt+F4` on the bar does nothing. **Open**
   brings the main window back. Minimise the main window: the bar appears.
8. **Closing.** Press `Alt+F4` on the main window during the session. Expect
   *Stop the session and close?*; **Keep the session** keeps it. Again, and
   **Stop and close**: the technician's end screen says the customer ended it,
   with the cleanup result, and the customer's CretLink exits afterwards. In
   Task Manager no `cretlink.exe` of the customer remains.
9. **Session ended.** In another session press **Stop now** (or `Escape`).
   Expect *You stopped the session*, the counts and each cleanup step; from
   the technician side *<name> ended the session*; unplugging the network
   *The connection was lost* with the cleanup not confirmed. **Save a copy**
   then shows the saved path with **Open** (as in MT-10). **Close CretLink**
   exits at once.
10. **Left from an earlier session (ST-M009-010).** End a session during a
    file upload by killing the customer's `cretlink.exe` in Task Manager, and
    hold one file of `C:\Program Files\CretLink Data\staging` open in another
    program that runs as administrator (the folder is closed to every other
    account).
    Start Get help again. Expect *Who is helping you?* to say that a file
    from an earlier CretLink session could not be removed, with the command
    to run in an administrator Windows PowerShell and the full path of the
    running program: `& '<folder>\cretlink-cli.exe' recover --remove` when
    `cretlink-cli.exe` sits next to it, else
    `& '<folder>\CretLink-Support.exe' recover --remove | Out-Host` (the
    GUI-subsystem program, which PowerShell waits for only when its output is
    piped). Close the other program, open Windows PowerShell as administrator
    and paste the command exactly as shown: PowerShell waits for it, it lists
    what it removed, says the computer is clean, and `$LASTEXITCODE` is 0;
    `CretLink-Support uninstall` is still refused.

### MT-13 — A technician's computer on several networks (ST-M009-016)

Run this on a technician computer that has, next to its local network, a VPN
connection or a virtual network adapter (the virtual switch of a hypervisor),
with the customer computer on the local network.

1. **First choice.** With the VPN disconnected, start a session (Give help).
   - Expected: the waiting screen names the adapter of the local network and
     its address, not the virtual adapter's.
2. **The list.** Choose **Wait on another network**.
   - Expected: every other private address of the computer is listed with the
     name of its adapter; the address in use is not offered again, and no
     public address appears (REQ-S-010).
3. **Change and change back.** Choose another network, then choose the local
   network again.
   - Expected: the screen stays the waiting screen, says *Changing the
     network…* for a moment, and then shows the chosen adapter and address
     and a new connection code.
4. **Discovery follows.** On the customer computer open Get help, first while
   the technician waits on a network the customer is not on, then while it
   waits on the local network.
   - Expected: in the first case the technician is not in the customer's list
     and the connection code does not connect. In the second case the
     technician appears once, with the address of the local network, and the
     session connects.
5. **A VPN that connects later.** Cancel, connect the VPN and start a session
   again in the same run of the program.
   - Expected: the session waits on the adapter chosen in step 3. After the
     program is closed and started again the first choice applies; when that
     is the VPN's address, choosing the local network once more works.
6. **Escape.** Open the list and press Escape twice.
   - Expected: the first Escape closes the list and the session keeps
     waiting; the second cancels it (REQ-F-018).

### MT-14 — The file operations of the explorer on Windows (ST-M010-001, ST-M010-004)

The list of drives, the removal of a folder with its content, a file sent
into a folder on any drive, and a change to one entry — a delete, the removal
of an empty folder, a new folder, a move — against a real elevated agent on
Windows. The automated tests cover the same points on the Windows CI runner;
this run adds real drives, the customer's own folders and a real stop.

The technician starts these operations from the file explorer of the live
session (ST-M010-002). A build without that screen has no control for them:
this scenario waits for the first build that has it. Steps 15 to 17 are the
exception: the AI tools reach those operations in every build. The customer
computer
needs a second drive that may be written to and emptied, such as a spare USB
stick; the steps call it `E:`. Never point a step at a folder whose content
matters.

Prepare the customer computer in an administrator PowerShell, which the
technician terminal can also do:

```powershell
New-Item -ItemType Directory C:\CretLinkTest\tree\sub\deeper, C:\CretLinkTest\outside\inner | Out-Null
1..50 | ForEach-Object { Set-Content "C:\CretLinkTest\tree\sub\file-$_.txt" "x" }
Set-Content C:\CretLinkTest\outside\keep.txt "keep"
Set-Content C:\CretLinkTest\outside\inner\keep-too.txt "keep"
Set-Content C:\CretLinkTest\tree\read-only.txt "x"; Set-ItemProperty C:\CretLinkTest\tree\read-only.txt IsReadOnly $true
cmd /c mklink /J C:\CretLinkTest\tree\junction C:\CretLinkTest\outside
cmd /c mklink /D C:\CretLinkTest\tree\folder-link C:\CretLinkTest\outside
cmd /c mklink C:\CretLinkTest\tree\file-link C:\CretLinkTest\outside\keep.txt
cmd /c mklink /J C:\CretLinkTest\via C:\CretLinkTest\outside
```

1. **Drives.** List the drives.
   - Expected: every drive letter of the customer computer with its kind, its
     label and its total and free space, as *This PC* shows them in Windows. A
     card reader without a card, or an optical drive without a disc, is listed
     with its kind and without a size, and no dialog appears on the
     customer's screen. A network drive is listed as a network drive, without
     a label or a size. A network drive that only the customer's own,
     non-elevated sign-in has mapped may be missing: Windows does not show it
     to an elevated program, and that is recorded, not a failure.
   - Expected: the customer's activity shows one file action of the
     technician, *list the drives*, and the technician's Audit tab lists
     `console_fs_drives` (REQ-F-017, REQ-S-017).
2. **A folder with links in it.** Delete `C:\CretLinkTest\tree`.
   - Expected: the folder is gone, the read-only file included. The junction
     and the two symbolic links were removed as links:
     `C:\CretLinkTest\outside\keep.txt` is still there with its content.
     The result says how many files and folders were removed.
   - Expected: the customer's activity shows one item, *delete the folder
     C:\CretLinkTest\tree and everything in it*, with the path, done; the
     Audit tab lists `console_remove_tree` with the path and the counts
     (REQ-F-017, REQ-S-017).
3. **A folder reached through a junction.** Ask to delete
   `C:\CretLinkTest\via\inner`: the path leads through the junction `via` to
   `C:\CretLinkTest\outside\inner`.
   - Expected: refused as an invalid path before anything is removed, with the
     reason that the path leads through a link.
     `C:\CretLinkTest\outside\inner\keep-too.txt` is still there, and the
     customer's item is failed.
   - Expected: deleting `C:\CretLinkTest\via` itself removes the junction and
     nothing else: `C:\CretLinkTest\outside` keeps its content. Deleting
     `C:\CretLinkTest\outside\inner` by its own path removes it.
4. **A file held open.** Run the preparation again. On the customer computer
   hold one file open without sharing it, then delete the folder:

   ```powershell
   $held = [IO.File]::Open("C:\CretLinkTest\tree\sub\file-1.txt", "Open", "Read", "None")
   ```

   - Expected: the removal is reported as **failed**, never as done. It names
     `file-1.txt` with the reason that the file is in use, and says how many
     entries were left. Everything else in the folder is gone; the held file
     and the folders above it are still there. The customer's item is failed.
     After `$held.Close()` a second removal succeeds (REQ-N-008).
5. **A large folder, cancelled.** Create a folder with 20 000 small files
   (`1..20000 | ForEach-Object { Set-Content "C:\CretLinkTest\many\f$_.txt" "x" }`
   after creating the folder), start its removal and cancel it while it runs.
   - Expected: the removal stops within about a second. The result says
     *cancelled* and how many files were removed; the others are still there.
     The customer's item was in progress and ends as cancelled.
6. **A large folder, stopped by the customer.** Start the removal of the rest
   and press **Stop now** on the customer computer while it runs.
   - Expected: the session ends at once on both sides; the technician is not
     told that the folder was removed; files remain in the folder; nothing is
     removed after the stop (REQ-S-004, REQ-F-018).
7. **A folder swapped during a removal.** In a new session fill
   `C:\CretLinkTest\many` up to 20 000 files again and start its removal.
   While it runs, on the customer computer, move the folder away as soon as
   Windows allows it and put another one in its place:

   ```powershell
   while (-not (Test-Path C:\CretLinkTest\many-moved)) { Rename-Item C:\CretLinkTest\many many-moved -ErrorAction SilentlyContinue }
   New-Item -ItemType Directory C:\CretLinkTest\many | Out-Null; Set-Content C:\CretLinkTest\many\new.txt "new"
   ```

   - Expected: the rename fails for as long as a step of the removal holds the
     folder and succeeds between two steps. The removal then ends as
     **failed**: the folder is no longer there, or it is not the one the
     removal began with. `C:\CretLinkTest\many\new.txt` is still there, and
     so are the files that were left in `C:\CretLinkTest\many-moved`.
8. **A drive is not a folder.** Ask to delete `E:\` itself.
   - Expected: refused as an invalid path before anything is removed; the
     content of the stick is untouched.
9. **Send to a folder.** Send a file of some tens of megabytes to the
   customer's `Documents` folder, and watch that folder in Windows.
   - Expected: while the transfer runs the folder holds a file whose name
     starts with `.cretlink-part.` and never one with the final name. When the
     transfer completes the file appears under its name and the part is gone.
     `Get-FileHash` on the customer computer gives the SHA-256 of the file on
     the technician computer (REQ-N-007).
   - Expected: the customer, signed in without administrator rights, can open
     the file. The customer's activity shows a received file from the
     technician with its path and size. The Audit tab lists `console_send`
     with the path, the length and the digest (REQ-F-017, REQ-S-017).
10. **Another drive.** Send the same file to `E:\` and to a folder on `E:`.
    - Expected: as in step 9, on the other drive.
11. **A destination folder that is a junction.** Make a junction to a folder
    and send a small file to it, and to a folder behind it:

    ```powershell
    cmd /c mklink /J C:\CretLinkTest\link-folder C:\CretLinkTest\outside
    New-Item -ItemType Directory C:\CretLinkTest\outside\behind | Out-Null
    ```

    Send the file to `C:\CretLinkTest\link-folder`, then to
    `C:\CretLinkTest\link-folder\behind`.
    - Expected: both transfers fail with an invalid path. Nothing appears in
      `C:\CretLinkTest\outside` or in `behind`: neither the file nor a
      `.cretlink-part.` file. The same file sent to
      `C:\CretLinkTest\outside\behind`, the real path, arrives.
12. **A name that is taken.** Send a different file under the name of step 9
    without asking to replace it, then again asking to replace it. Then hold
    the file open on the customer computer
    (`$held = [IO.File]::Open("<the file>", "Open", "Read", "None")`) and
    send a third file under that name, asking to replace it.
    - Expected: the first transfer fails because the file already exists, and
      the file of step 9 keeps its hash. The second replaces it, and the hash
      is the new file's. The third fails with the reason that the file is in
      use, and the file keeps its hash; after `$held.Close()` the same
      transfer replaces it.
13. **Cancel and stop during a transfer.** Start a large transfer into
    `Documents` and cancel it. Start it again and press **Stop now** on the
    customer computer while it runs.
    - Expected: each time no file appears under the final name, and afterwards
      no `.cretlink-part.` file is left in the folder. The end report of the
      stopped session names nothing that could not be removed (REQ-F-019).
14. **A full drive.** Send a file that is larger than the free space of `E:`.
    - Expected: the transfer fails and says that the storage is exhausted;
      nothing appears under the final name, and no part is left after the
      session has ended.
15. **A change to one entry through a junction (ST-M010-004).** Deleting a
    file, removing an empty folder, creating a folder and moving an entry are
    asked for with the AI tools `remote_delete`, `remote_remove_dir`,
    `remote_make_dir` and `remote_move` from an AI client on the technician
    computer; in a build with the explorer, also with **Delete**,
    **New folder** and **Rename** there. Prepare:

    ```powershell
    New-Item -ItemType Directory C:\CretLinkTest\real\empty, C:\CretLinkTest\real\inside | Out-Null
    Set-Content C:\CretLinkTest\real\victim.txt "victim"
    Set-Content C:\CretLinkTest\real\inside\deep.txt "deep"
    Set-Content C:\CretLinkTest\beside.txt "beside"
    cmd /c mklink /J C:\CretLinkTest\link C:\CretLinkTest\real
    ```

    Ask, one after the other: to delete `C:\CretLinkTest\link\victim.txt`; to
    remove the empty folder `C:\CretLinkTest\link\empty`; to create the
    folder `C:\CretLinkTest\link\new`; to move
    `C:\CretLinkTest\link\victim.txt` to `C:\CretLinkTest\taken.txt`; to
    move `C:\CretLinkTest\beside.txt` to `C:\CretLinkTest\link\planted.txt`;
    and to delete `C:\CretLinkTest\link\inside\deep.txt`, where the junction
    is a folder further up.
    - Expected: every request fails with `invalid_path`, and the customer's
      activity shows each as failed. `C:\CretLinkTest\real` holds exactly what
      it held: `victim.txt` and `inside\deep.txt` with their content, the
      folder `empty`, and neither `new` nor `planted.txt`. `beside.txt` is
      where it was, and there is no `taken.txt`.
    - Expected: the same six requests with `real` in the place of `link` all
      succeed.
16. **A folder swapped between two requests.** Prepare a folder with two
    files, and another place with a file of the same name:

    ```powershell
    New-Item -ItemType Directory C:\CretLinkTest\work, C:\CretLinkTest\elsewhere | Out-Null
    Set-Content C:\CretLinkTest\work\one.txt "ours"; Set-Content C:\CretLinkTest\work\two.txt "ours"
    Set-Content C:\CretLinkTest\elsewhere\two.txt "theirs"
    ```

    Ask to delete `C:\CretLinkTest\work\one.txt`. Then, on the customer
    computer, move the folder away and put a junction in its place, and ask to
    delete `C:\CretLinkTest\work\two.txt`:

    ```powershell
    Rename-Item C:\CretLinkTest\work work-moved
    cmd /c mklink /J C:\CretLinkTest\work C:\CretLinkTest\elsewhere
    ```

    - Expected: the first delete succeeds. The second fails with
      `invalid_path`: `C:\CretLinkTest\elsewhere\two.txt` still reads
      `theirs`, and `C:\CretLinkTest\work-moved\two.txt` is still there.
17. **A link as the entry itself, and a substituted drive.** Ask to remove
    `C:\CretLinkTest\link`, the junction of step 15, as an empty folder; then
    to delete it. Then, in the administrator PowerShell, run
    `subst X: C:\CretLinkTest\elsewhere` and ask to delete `X:\two.txt`.
    Afterwards run `subst X: /D`.
    - Expected: the removal as a folder is refused, because a link is not a
      folder, and the junction stays. The delete removes the junction and
      nothing else: `C:\CretLinkTest\real` keeps its content.
    - Expected: the delete on the substituted drive fails with
      `invalid_path`, and `C:\CretLinkTest\elsewhere\two.txt` is still there.
      (Windows keeps a substituted drive per sign-in. If the program of the
      session was elevated by another account than the PowerShell, it does
      not see `X:` and answers that the path does not exist; the file stays
      then too.) By its real path, `C:\CretLinkTest\elsewhere\two.txt`, the
      file is deleted.
18. **Afterwards.** End the session and look again.
    - Expected: the files sent in steps 9, 10 and 12 are still in their
      folders: they are the customer's to keep, and the list of staged files
      never named them. `C:\CretLinkTest` can be deleted by hand.

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

1. **Launch.** On the customer machine start `cretlink.exe` and choose **Get
   help**; on the technician machine start `cretlink`, choose **Give help**, and
   let the MCP client start `cretlink mcp`, both as the same OS user
   (REQ-F-013).
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
   - Expected: the AI's stated `purpose` appears on the customer's command card
     and in the console activity view, labelled as the AI's reason, next to the
     real command; the console audit records hold it; a technician console
     command shows its source and no purpose (REQ-F-026, ST-M009-007).
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
| 2026-11-10 | Technician: a Windows 11 laptop with the installed copy and Claude Desktop (Code tab). Customer: a Windows 11 Pro 25H2 (build 26200) computer, elevated, non-simulated, started from a `CretLink-Support.exe` copy | pull request preview `0.0.0-pr.6.gc6b7c0b` | MT-1, MT-2, MT-4, MT-6; MT-10 steps 2 to 4 and the stops of step 1; MT-11 steps 1, 3 and 6 on Windows; MT-12 steps 1 to 9 | PASS for every point that was run. The UAC prompt was declined, then accepted; `remote_system_info` reported an elevated target and `whoami /groups` high integrity. An upload and a download were verified by SHA-256, and a failing command reported exit code 3 as failed. A customer **Stop now** during a 180-second command ended it with no surviving process; `Alt+F4` asked first; a lost network ended honestly, with the cleanup not confirmed and no automatic reconnect. No file existed on the customer computer before **Save a copy**; afterwards there was no process, service, scheduled task or autostart entry and the staging folder was empty. The webview profile folder remained (known gap). The install needed no UAC prompt and added the Start-menu and Installed apps entries; the uninstall removed them and kept the audit folder. Two defects were found and are fixed with this change: a download into a missing local folder was reported as a lost session (ST-M003-005), and a Command Prompt command with double quotes was mangled (ST-M003-001). Not confirmed: whether Windows Firewall asked for permission, and the behaviour on a network that Windows marks as public. |
| _pending_ | | | MT-3b re-run (hard kill, expect **zero** survivors under ST-M006-007) | not yet recorded on hardware |
| _pending_ | | | MT-5 (locked-file cleanup + retry) | not yet recorded on hardware |
| _pending_ | | | MT-7 (`cretlink-cli shell` host-terminal profile smoke: interactive/resize/Ctrl-C-to-remote/Ctrl-] detach, no-consent + revoke fail-closed, no-secret check) | not yet recorded on hardware |
| _pending_ | | | MT-10 step 1 with a staged file held open (the residual case — ST-M009-012) | not yet recorded on hardware |
| _pending_ | | | MT-8 (reboot-resume: arm under UAC, survive a reboot, one attended reconnect via the `K_rr` proof, single-use consumption, expired/cloned refusal, residual removal via the recovery command — ST-M006-001/002/003) | not yet recorded on hardware |
| _pending_ | | | MT-11 steps 2, 4, 5 and 7 to 9 on Windows, and all of it on macOS (ST-M009-006) | not yet recorded on hardware |
| _pending_ | | | MT-12 step 10 (files left from an earlier session and `cretlink recover`) and the Narrator pass (ST-M009-009, ST-M009-010) | not yet recorded on hardware |
| _pending_ | | | MT-13 (a technician's computer on several networks, ST-M009-016) | not yet recorded on hardware |
| _pending_ | | | MT-14 (the file operations of the explorer: the drives, a folder with links / through a junction / a held file / cancel and stop / swapped while it is removed, a file sent to a folder on two drives and refused for a junction, ST-M010-001; a delete, a folder removal, a new folder and a move refused through a junction, after a swap and on a substituted drive, ST-M010-004) | not yet recorded on hardware |

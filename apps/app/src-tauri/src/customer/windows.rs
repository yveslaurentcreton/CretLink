//! The customer's two windows and closing them (ST-M009-009, REQ-S-002,
//! REQ-F-018).
//!
//! - The main window (`main`) holds every screen.
//! - The compact bar (`mini`, `#c-mini`) is a second, small window that is
//!   always on top and can be moved. It loads the same frontend, which renders
//!   the compact view in a window with this label, and uses the customer
//!   role's commands only. **Make small** ([`make_small`]) shows it and hides
//!   the main window; **Open** ([`open_main`]) does the reverse.
//!
//! While a session exists, the main window or the compact bar is always
//! visible: minimising the main window shows the bar, the bar refuses to
//! close, and when the session ends the main window comes back with the end
//! report ([`sync_windows`]).
//!
//! Closing is decided by [`close_action`], a pure function of the window, of
//! whether a session exists and of whether a session task is still tearing
//! down. Outside a session the main window closes at once and the process
//! exits. During a session the close is held — the close button, `Alt+F4`, and
//! an exit request alike — and the main window asks "Stop the session and
//! close?". Confirming ([`stop_and_close`]) revokes the session, waits for its
//! cleanup like the teardown of ST-M009-012, and then exits. A close right
//! after **Stop**, while the session task still tears down, takes the same
//! bounded wait before exiting. No path exits without revoking.

use std::time::Duration;

use cretlink_protocol::RevokeReason;
use tauri::{
    AppHandle, Emitter, ExitRequestApi, LogicalPosition, Manager, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Window, WindowEvent,
};

use super::report::TEARDOWN_WAIT;
use super::session_driver::Intent;
use super::{AGENT_VIEW_EVENT, AppState, CommandError, session_exists};

/// The main window's label.
pub(crate) const MAIN: &str = "main";
/// The compact bar's label.
pub(crate) const MINI: &str = "mini";
/// Sent to the main window when a close or an exit was held because a session
/// exists: it asks "Stop the session and close?".
pub(crate) const CLOSE_REQUESTED_EVENT: &str = "customer-close-requested";

/// The compact bar's size, in logical pixels (the mock's 470 × ~110).
const MINI_SIZE: (f64, f64) = (470.0, 112.0);

/// How long closing waits for the session's cleanup, beyond the teardown wait
/// itself, before it exits anyway. The cleanup's outcome is then what the
/// session task reported, or unknown — never a success.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Which window a close request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowKind {
    /// The main window.
    Main,
    /// The compact bar.
    Mini,
}

impl WindowKind {
    fn of(label: &str) -> Self {
        if label == MINI {
            Self::Mini
        } else {
            Self::Main
        }
    }
}

/// What a close request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// No session: end the process at once (a hidden compact bar must not keep
    /// it alive).
    Exit,
    /// No session any more, but its task is still tearing down (the customer
    /// just pressed **Stop**): wait for it, bounded, then end the process.
    ExitWhenSettled,
    /// A session exists: hold the close and ask "Stop the session and close?".
    AskToStop,
    /// The compact bar during a session: it cannot be closed.
    Refuse,
    /// The compact bar outside a session: let it close.
    Close,
}

/// The close decision, a pure function of the window, of whether a session
/// exists, and of whether a session task is still `settling` — tearing down
/// after it ended (ST-M009-009). An exit request is decided as a close of the
/// main window.
#[must_use]
pub(crate) fn close_action(window: WindowKind, session: bool, settling: bool) -> CloseAction {
    match (window, session) {
        (WindowKind::Main, true) => CloseAction::AskToStop,
        (WindowKind::Main, false) if settling => CloseAction::ExitWhenSettled,
        (WindowKind::Main, false) => CloseAction::Exit,
        (WindowKind::Mini, true) => CloseAction::Refuse,
        (WindowKind::Mini, false) => CloseAction::Close,
    }
}

/// Whether the compact bar must be shown so that something stays visible
/// (REQ-S-002): a session exists and the main window is minimised or hidden.
#[must_use]
pub(crate) fn bar_needed(session: bool, main_shown: bool) -> bool {
    session && !main_shown
}

fn has_session(app: &AppHandle) -> bool {
    app.try_state::<AppState>()
        .is_some_and(|state| session_exists(&state.guard()))
}

/// Whether a live session's task has not finished yet (its teardown and
/// cleanup report included).
fn settling(session: &super::ConsentSession) -> bool {
    session.settled.as_ref().is_some_and(|done| !*done.borrow())
}

/// Whether a session exists, and whether a session task is still settling.
fn session_state(app: &AppHandle) -> (bool, bool) {
    app.try_state::<AppState>().map_or((false, false), |state| {
        let session = state.guard();
        (session_exists(&session), settling(&session))
    })
}

/// Waits, bounded, until the session task has settled, then ends the process.
async fn exit_when_settled(app: AppHandle) {
    let settled = app
        .try_state::<AppState>()
        .and_then(|state| state.guard().settled.clone());
    if let Some(mut settled) = settled {
        let _ =
            tokio::time::timeout(TEARDOWN_WAIT + CLOSE_GRACE, settled.wait_for(|done| *done)).await;
    }
    app.exit(0);
}

/// Carries out a close or exit decision that ends the process.
fn act(app: &AppHandle, action: CloseAction) {
    match action {
        CloseAction::AskToStop => ask_to_stop(app),
        CloseAction::Exit => app.exit(0),
        CloseAction::ExitWhenSettled => {
            tauri::async_runtime::spawn(exit_when_settled(app.clone()));
        }
        CloseAction::Refuse | CloseAction::Close => {}
    }
}

fn main_shown(window: &Window) -> bool {
    window.is_visible().unwrap_or(true) && !window.is_minimized().unwrap_or(false)
}

/// The window events of both customer windows.
pub(crate) fn on_window_event(window: &Window, event: &WindowEvent) {
    let app = window.app_handle().clone();
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            let (session, settling) = session_state(&app);
            let action = close_action(WindowKind::of(window.label()), session, settling);
            if action != CloseAction::Close {
                api.prevent_close();
            }
            act(&app, action);
        }
        // Minimising the main window during a session shows the bar.
        WindowEvent::Resized(_)
            if window.label() == MAIN && bar_needed(has_session(&app), main_shown(window)) =>
        {
            // A window is created off the event loop's thread (WebView2
            // deadlocks when it is created inside an event handler).
            tauri::async_runtime::spawn(async move {
                let _ = show_mini(&app);
            });
        }
        _ => {}
    }
}

/// Holds an exit request while a session exists (`Alt+F4` on the last window,
/// the system closing the application) and asks the customer instead; while a
/// stopped session still tears down, the exit waits for it, bounded.
///
/// A programmatic exit (`code` is `Some`) is this module's own, made once the
/// session was revoked and the bounded wait is over: it is let through.
pub(crate) fn on_exit_requested(app: &AppHandle, api: &ExitRequestApi, code: Option<i32>) {
    if code.is_some() {
        return;
    }
    let (session, settling) = session_state(app);
    let action = close_action(WindowKind::Main, session, settling);
    if action != CloseAction::Exit {
        api.prevent_exit();
        act(app, action);
    }
}

/// Brings the main window to the front and asks "Stop the session and close?".
fn ask_to_stop(app: &AppHandle) {
    show_main(app);
    let _ = app.emit_to(MAIN, CLOSE_REQUESTED_EVENT, ());
}

/// Shows, restores and focuses the main window.
fn show_main(app: &AppHandle) {
    if let Some(main) = app.get_webview_window(MAIN) {
        let _ = main.show();
        let _ = main.unminimize();
        let _ = main.set_focus();
    }
}

/// Shows the compact bar, creating it the first time: always on top, movable,
/// without the system frame (its own Stop and Open are the controls), and in
/// the taskbar like any window (REQ-S-002).
fn show_mini(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(mini) = app.get_webview_window(MINI) {
        mini.show()?;
        return Ok(mini);
    }
    let mut builder = WebviewWindowBuilder::new(app, MINI, WebviewUrl::App("index.html".into()))
        .title("CretLink — compact bar")
        .inner_size(MINI_SIZE.0, MINI_SIZE.1)
        .min_inner_size(MINI_SIZE.0, MINI_SIZE.1)
        .max_inner_size(MINI_SIZE.0, MINI_SIZE.1)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(false)
        .visible(true);
    if let Some(position) = mini_position(app) {
        builder = builder.position(position.x, position.y);
    }
    builder.build()
}

/// The top centre of the main window's monitor, a little below the edge.
fn mini_position(app: &AppHandle) -> Option<LogicalPosition<f64>> {
    let monitor = app.get_webview_window(MAIN)?.current_monitor().ok()??;
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let origin = monitor.position().to_logical::<f64>(scale);
    Some(LogicalPosition::new(
        origin.x + (size.width - MINI_SIZE.0) / 2.0,
        origin.y + 12.0,
    ))
}

/// Keeps the windows honest when the session state changes: once no session
/// exists, the compact bar goes and the main window shows the end (called on
/// every rendered view).
pub(crate) fn sync_windows(app: &AppHandle, session: bool) {
    if session {
        return;
    }
    let mini_visible = app
        .get_webview_window(MINI)
        .is_some_and(|mini| mini.is_visible().unwrap_or(false));
    let main_hidden = app
        .get_webview_window(MAIN)
        .is_some_and(|main| !main.is_visible().unwrap_or(true));
    if mini_visible || main_hidden {
        show_main(app);
        if let Some(mini) = app.get_webview_window(MINI) {
            let _ = mini.hide();
        }
    }
}

/// **Make small**: shows the compact bar and hides the main window. Only during
/// a session; the main window stays when the bar cannot be shown.
#[tauri::command]
pub(super) async fn make_small(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), CommandError> {
    if !session_exists(&state.guard()) {
        return Err(CommandError::new(
            "window",
            "the compact bar is for a running session",
        ));
    }
    show_mini(&app).map_err(|err| {
        CommandError::new("window", format!("the compact bar could not open: {err}"))
    })?;
    if let Some(main) = app.get_webview_window(MAIN) {
        main.hide()
            .map_err(|err| CommandError::new("window", err))?;
    }
    Ok(())
}

/// **Open**: shows the main window again and hides the compact bar.
#[tauri::command]
pub(super) fn open_main(app: AppHandle) {
    show_main(&app);
    if let Some(mini) = app.get_webview_window(MINI) {
        let _ = mini.hide();
    }
}

/// Moves the compact bar with the pointer (it has no system frame to drag).
#[tauri::command]
pub(super) fn drag_window(window: WebviewWindow) -> Result<(), CommandError> {
    if window.label() != MINI {
        return Err(CommandError::new(
            "window",
            "only the compact bar is moved this way",
        ));
    }
    window
        .start_dragging()
        .map_err(|err| CommandError::new("window", err))
}

/// **Stop and close**: revokes the session, waits for its cleanup (bounded, as
/// the end report does), and exits. Outside a session it exits at once.
#[tauri::command]
pub(super) async fn stop_and_close(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), CommandError> {
    let settled = {
        let mut session = state.guard();
        if session_exists(&session) {
            if let Some(tx) = session.live.as_ref() {
                let _ = tx.send(Intent::Stop(RevokeReason::CustomerRequested));
            }
            // The local model goes safe at once, as for Stop.
            session.agent.stop();
            let _ = app.emit(AGENT_VIEW_EVENT, session.view());
        }
        session.settled.clone()
    };
    if settled.is_some() {
        exit_when_settled(app).await;
    } else {
        app.exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_a_session_the_main_window_closes_at_once() {
        assert_eq!(
            close_action(WindowKind::Main, false, false),
            CloseAction::Exit
        );
    }

    #[test]
    fn during_a_session_closing_asks_to_stop_first() {
        // The close button, Alt+F4 and an exit request (decided as a close of
        // the main window) arrive as the same close request.
        assert_eq!(
            close_action(WindowKind::Main, true, true),
            CloseAction::AskToStop
        );
        assert_eq!(
            close_action(WindowKind::Main, true, false),
            CloseAction::AskToStop
        );
    }

    #[test]
    fn closing_right_after_stop_waits_for_the_teardown() {
        // Stop ended the session, but its task still revokes and cleans up:
        // the process must not exit under it.
        assert_eq!(
            close_action(WindowKind::Main, false, true),
            CloseAction::ExitWhenSettled
        );
    }

    #[test]
    fn a_settled_session_is_not_settling() {
        use super::super::ConsentSession;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let session = ConsentSession::live_dialing(tx, Some(done_rx));
        assert!(settling(&session));
        assert!(done_tx.send(true).is_ok());
        assert!(!settling(&session));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(!settling(&ConsentSession::live_dialing(tx, None)));
    }

    #[test]
    fn the_compact_bar_cannot_be_closed_during_a_session() {
        assert_eq!(
            close_action(WindowKind::Mini, true, false),
            CloseAction::Refuse
        );
        assert_eq!(
            close_action(WindowKind::Mini, false, false),
            CloseAction::Close
        );
        assert_eq!(
            close_action(WindowKind::Mini, false, true),
            CloseAction::Close
        );
    }

    #[test]
    fn a_window_is_always_visible_during_a_session() {
        // Minimised or hidden main window: the bar comes.
        assert!(bar_needed(true, false));
        assert!(!bar_needed(true, true));
        // No session: nothing has to stay on screen.
        assert!(!bar_needed(false, false));
    }

    #[test]
    fn labels_map_to_windows() {
        assert_eq!(WindowKind::of(MAIN), WindowKind::Main);
        assert_eq!(WindowKind::of(MINI), WindowKind::Mini);
        assert_eq!(WindowKind::of("anything"), WindowKind::Main);
    }
}

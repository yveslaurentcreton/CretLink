//! This process's webview profile, and the end of the process that removes it
//! (ST-M009-014).
//!
//! A webview keeps a profile — cache and local storage — in a folder. Where
//! that folder is, which folders other copies left, and how a folder is
//! removed, is decided in [`cretlink_cli::webview_profile`]; this module
//! applies it to the windows of one process:
//!
//! - [`Profile::open`] runs before any window opens. A copy that is not
//!   installed, and every customer process, gets a new temporary folder of its
//!   own and holds its lock for the life of the process; the installed
//!   technician copy uses the folder in the application's data folder; on
//!   macOS the webview takes no folder. The same start removes, off the
//!   window's thread, the temporary folders of copies that no longer run.
//! - A process with administrator rights makes its folder in its private
//!   folder in `CretLink Data` and looks for leftovers only there
//!   ([`Base::Private`]): nothing of the temporary folder or the local
//!   application data that its environment names is looked at or removed
//!   (REQ-S-024). It drops the variables that WebView2 obeys from its
//!   environment before any window opens
//!   ([`webview_profile::forget_webview_overrides`], called by the caller).
//! - Every window is built with that folder: the main window through
//!   [`open_main_window`] — `tauri.conf.json` describes it but does not create
//!   it, because the folder is only known at run time — and the customer's
//!   compact bar through [`directory`].
//! - [`Profile::close`] removes the folder at the very end of the process:
//!   after the last window is gone and the async runtime, with every session
//!   task on it, was shut down. Nothing but the removal outlives the window.
//!
//! A webview holds its profile for as long as it exists, and an exit the
//! program asks for itself (`AppHandle::exit`) ends the event loop with the
//! windows still open. [`Closing`] therefore turns such an exit into closing
//! every window first; the event loop then ends by itself when the last one
//! is gone. Should the windows not close, the process still ends after a
//! bounded wait, and what the webview still holds is left for the next start.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cretlink_cli::install::{Layout, Platform};
use cretlink_cli::webview_profile::{self, Base, Home, Patience, TempProfile};
use tauri::{AppHandle, ExitRequestApi, Manager, WebviewWindow, WebviewWindowBuilder};

/// The main window's label, as in `tauri.conf.json`.
pub(crate) const MAIN_WINDOW: &str = "main";

/// How long the windows get to close before the process ends without them.
const WINDOW_CLOSE_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct Inner {
    /// The folder every window gives its webview; `None` where the webview
    /// takes none (macOS).
    dir: Option<PathBuf>,
    /// This process's own temporary folder, with its lock, held until the
    /// process ends.
    temp: Mutex<Option<TempProfile>>,
    /// Whether `dir` is the installed copy's profile.
    installed: bool,
    /// Set when the installed copy was uninstalled from this window.
    uninstalled: AtomicBool,
}

/// The profile of this process's webview, shared by its windows as managed
/// state.
#[derive(Debug, Clone)]
pub(crate) struct Profile(Arc<Inner>);

impl Profile {
    fn new(dir: Option<PathBuf>, temp: Option<TempProfile>, installed: bool) -> Self {
        Self(Arc::new(Inner {
            dir,
            temp: Mutex::new(temp),
            installed,
            uninstalled: AtomicBool::new(false),
        }))
    }

    /// Decides and prepares the profile of this process, before its window
    /// opens; `customer` is whether it shows the customer role, and `base`
    /// is where its temporary folder is made ([`Base::current`]). Then
    /// removes, on a thread of its own, the temporary folders that other
    /// copies left in `base`.
    ///
    /// # Errors
    ///
    /// Returns an error when the temporary folder of this process could not
    /// be created. No window opens then: the webview would fall back to the
    /// folder that stays.
    pub(crate) fn open(customer: bool, base: &Base) -> io::Result<Self> {
        // The profile of the installed copy, only for a process that runs
        // from the install location, and never for a process with
        // administrator rights: the account's folders, which its environment
        // names, are not its own.
        let installed = if base.is_private() {
            None
        } else {
            let exe = std::env::current_exe().ok();
            Layout::current()
                .filter(|layout| {
                    exe.as_deref()
                        .is_some_and(|exe| layout.is_installed_exe(exe))
                })
                .and_then(|layout| layout.webview)
        };
        let profile = match Home::choose(Platform::current(), installed.as_deref(), customer) {
            Home::Unmanaged => return Ok(Self::new(None, None, false)),
            // An installed copy whose data folder cannot be written runs
            // with a temporary profile, like a copy that is not installed.
            Home::Installed(dir) if webview_profile::create_installed(&dir).is_ok() => {
                Self::new(Some(dir), None, true)
            }
            Home::Installed(_) | Home::Temporary => {
                // The private folder is made with its owner and its
                // permissions in one step; the profile made in it takes
                // them when it is created, and is then opened to the
                // account whose desktop shows the window: WebView2 runs its
                // browser as that account, without administrator rights.
                base.prepare()?;
                let temp = TempProfile::create_for(base)?;
                Self::new(Some(temp.path().to_path_buf()), Some(temp), false)
            }
        };
        let swept = base.dir().to_path_buf();
        // A folder that cannot be removed, or whose lock cannot be told, is
        // left alone, and is not an error for this session: a later start
        // tries again, and `recover` names it.
        let own = profile
            .temp()
            .as_ref()
            .map(|temp| temp.path().to_path_buf());
        std::thread::spawn(move || {
            let _ = webview_profile::sweep(&swept, own.as_deref(), &mut Patience::none());
        });
        Ok(profile)
    }

    fn temp(&self) -> MutexGuard<'_, Option<TempProfile>> {
        self.0.temp.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The folder every window of this process gives its webview. `None`
    /// where the webview takes none (macOS).
    pub(crate) fn directory(&self) -> Option<PathBuf> {
        self.0.dir.clone()
    }

    /// Whether this process's webview holds the installed copy's profile, so
    /// that an uninstall from this window cannot remove it at once.
    pub(crate) fn is_installed(&self) -> bool {
        self.0.installed
    }

    /// The installed copy was uninstalled from this window: its profile goes
    /// when the window has closed.
    pub(crate) fn remove_when_closed(&self) {
        self.0.uninstalled.store(true, Ordering::SeqCst);
    }

    /// Whether this process has a folder to remove when it ends: its
    /// temporary one, or the installed one after an uninstall.
    pub(crate) fn leaves_a_folder(&self) -> bool {
        self.temp().is_some() || (self.0.installed && self.0.uninstalled.load(Ordering::SeqCst))
    }

    /// Removes what this process must not leave, trying again for a few
    /// seconds while the closing webview still holds files. Call it at the
    /// end of the process, when no window is left.
    ///
    /// # Errors
    ///
    /// Returns an error when the folder is still there. The lock of a
    /// temporary folder is free then, so the next start of a CretLink window
    /// removes it, and `recover` lists it; the installed copy's profile is
    /// found by a second uninstall.
    pub(crate) fn close(&self) -> io::Result<()> {
        self.close_with(&mut Patience::a_few_seconds())
    }

    /// [`Self::close`] with the patience given.
    fn close_with(&self, patience: &mut Patience) -> io::Result<()> {
        let temp = self.temp().take();
        if let Some(temp) = temp {
            return temp.remove(patience);
        }
        match &self.0.dir {
            Some(dir) if self.leaves_a_folder() => webview_profile::remove_installed(dir, patience),
            _ => Ok(()),
        }
    }
}

/// The profile folder of the process `app` belongs to, for a window built
/// after the main one.
pub(crate) fn directory(app: &AppHandle) -> Option<PathBuf> {
    app.try_state::<Profile>()
        .and_then(|profile| profile.directory())
}

/// Builds the main window as `tauri.conf.json` describes it, with this
/// process's profile folder.
///
/// # Errors
///
/// Returns an error when the configuration has no main window or the window
/// cannot be created.
pub(crate) fn open_main_window(
    app: &AppHandle,
    profile: &Profile,
) -> Result<WebviewWindow, Box<dyn std::error::Error>> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == MAIN_WINDOW)
        .ok_or("the configuration has no main window")?;
    let mut builder = WebviewWindowBuilder::from_config(app, config)?;
    if let Some(dir) = profile.directory() {
        builder = builder.data_directory(dir);
    }
    Ok(builder.build()?)
}

/// What an exit request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitStep {
    /// Let the event loop end.
    End,
    /// Hold the exit and close every window; the event loop ends when the
    /// last one is gone.
    CloseWindows,
    /// Hold the exit: the windows are closing.
    Wait,
}

/// Decides an exit request. `code` is `Some` when the program asked for the
/// exit itself and `None` when the last window is gone; `folder` is whether
/// this process has a profile folder to remove, `windows` whether a window
/// still exists, `closing` whether the windows are being closed already, and
/// `forced` whether they were given their time.
#[must_use]
pub(crate) fn exit_step(
    code: Option<i32>,
    folder: bool,
    windows: bool,
    closing: bool,
    forced: bool,
) -> ExitStep {
    match code {
        // The last window is gone: nothing holds the profile any more.
        None => ExitStep::End,
        Some(_) if !windows || forced => ExitStep::End,
        Some(_) if closing => ExitStep::Wait,
        // A webview holds its profile for as long as it exists.
        Some(_) if folder => ExitStep::CloseWindows,
        Some(_) => ExitStep::End,
    }
}

#[derive(Debug, Default)]
struct ClosingState {
    /// The windows are being closed for an exit.
    begun: bool,
    /// The windows did not close in time; the exit goes ahead without them.
    forced: bool,
    /// The event loop has ended.
    ended: bool,
    /// The exit code the program asked for.
    code: Option<i32>,
}

/// The end of a process that has a profile folder to remove: an exit the
/// program asks for closes every window first.
#[derive(Debug, Clone, Default)]
pub(crate) struct Closing(Arc<Mutex<ClosingState>>);

impl Closing {
    fn state(&self) -> MutexGuard<'_, ClosingState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the windows are being closed for an exit.
    pub(crate) fn begun(&self) -> bool {
        self.state().begun
    }

    /// The exit code the program asked for, if it asked.
    pub(crate) fn code(&self) -> Option<i32> {
        self.state().code
    }

    /// Handles an exit request ([`exit_step`]). `folder` is whether this
    /// process has a profile folder to remove when it ends.
    pub(crate) fn on_exit_requested(
        &self,
        app: &AppHandle,
        api: &ExitRequestApi,
        code: Option<i32>,
        folder: bool,
    ) {
        let windows = app.webview_windows();
        let mut state = self.state();
        if state.code.is_none() {
            state.code = code;
        }
        match exit_step(code, folder, !windows.is_empty(), state.begun, state.forced) {
            ExitStep::End => {}
            ExitStep::Wait => api.prevent_exit(),
            ExitStep::CloseWindows => {
                state.begun = true;
                drop(state);
                api.prevent_exit();
                for window in windows.values() {
                    let _ = window.destroy();
                }
                self.end_after(app.clone(), WINDOW_CLOSE_WAIT, code.unwrap_or(0));
            }
        }
    }

    /// Ends the process after `wait` even when the windows did not close, so
    /// that no path leaves a process without a window running.
    fn end_after(&self, app: AppHandle, wait: Duration, code: i32) {
        let closing = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            let mut state = closing.state();
            // Checked and asked under one lock: once the event loop has
            // ended, nothing is sent to it any more.
            if !state.ended {
                state.forced = true;
                app.exit(code);
            }
        });
    }

    /// The event loop has ended.
    pub(crate) fn ended(&self) {
        self.state().ended = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for one test, removed afterwards.
    struct Folder(PathBuf);

    impl Folder {
        fn new(test: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cretlink-app-profile-{test}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_exit_the_program_asks_for_closes_the_windows_first() {
        // A profile folder to remove, and a window that still holds it.
        assert_eq!(
            exit_step(Some(0), true, true, false, false),
            ExitStep::CloseWindows
        );
        // Further requests while the windows close are held too.
        assert_eq!(exit_step(Some(0), true, true, true, false), ExitStep::Wait);
        // The last window is gone: the event loop ends.
        assert_eq!(exit_step(None, true, false, true, false), ExitStep::End);
        assert_eq!(exit_step(Some(0), true, false, true, false), ExitStep::End);
    }

    #[test]
    fn a_process_without_a_folder_to_remove_exits_as_before() {
        // The installed copy, and every process on macOS.
        assert_eq!(exit_step(Some(0), false, true, false, false), ExitStep::End);
        assert_eq!(exit_step(None, false, false, false, false), ExitStep::End);
    }

    #[test]
    fn closing_the_last_window_ends_the_process() {
        // The user closed it: no exit code, nothing left to close.
        assert_eq!(exit_step(None, true, false, false, false), ExitStep::End);
        // The system ends the application while a window exists (macOS).
        assert_eq!(exit_step(None, true, true, false, false), ExitStep::End);
    }

    #[test]
    fn windows_that_do_not_close_never_keep_the_process_alive() {
        assert_eq!(exit_step(Some(0), true, true, true, true), ExitStep::End);
        assert_eq!(exit_step(Some(1), true, true, false, true), ExitStep::End);
    }

    #[test]
    fn the_first_exit_code_is_kept_and_the_end_is_recorded() {
        let closing = Closing::default();
        assert!(!closing.begun());
        assert_eq!(closing.code(), None);
        {
            let mut state = closing.state();
            state.begun = true;
            state.code = Some(3);
        }
        assert!(closing.clone().begun());
        assert_eq!(closing.code(), Some(3));
        closing.ended();
        assert!(closing.state().ended);
    }

    #[test]
    fn a_profile_without_a_folder_has_nothing_to_remove() {
        // macOS: the webview takes no folder.
        let unmanaged = Profile::new(None, None, false);
        assert_eq!(unmanaged.directory(), None);
        assert!(!unmanaged.leaves_a_folder());
        assert!(unmanaged.close_with(&mut Patience::none()).is_ok());
    }

    #[test]
    fn a_temporary_profile_is_removed_when_the_process_ends() {
        let base = Folder::new("temp");
        let temp = TempProfile::create_in(&base.0).unwrap();
        let dir = temp.path().to_path_buf();
        let profile = Profile::new(Some(dir.clone()), Some(temp), false);
        assert_eq!(profile.directory(), Some(dir.clone()));
        assert!(!profile.is_installed());
        assert!(profile.leaves_a_folder());
        std::fs::write(dir.join("cookies"), b"x").unwrap();
        profile.close_with(&mut Patience::none()).unwrap();
        assert!(!dir.exists());
        // Nothing is left to remove a second time.
        assert!(!profile.leaves_a_folder());
        assert!(profile.close_with(&mut Patience::none()).is_ok());
    }

    #[test]
    fn the_installed_profile_stays_unless_the_copy_was_uninstalled_from_this_window() {
        let base = Folder::new("installed");
        let dir = base.0.join("CretLink").join("webview");
        webview_profile::create_installed(&dir).unwrap();
        std::fs::write(dir.join("cookies"), b"x").unwrap();
        let audit = base.0.join("CretLink").join("audit");
        std::fs::create_dir_all(&audit).unwrap();

        let profile = Profile::new(Some(dir.clone()), None, true);
        assert!(profile.is_installed());
        assert!(!profile.leaves_a_folder());
        profile.close_with(&mut Patience::none()).unwrap();
        assert!(dir.join("cookies").exists(), "an installed copy keeps it");

        profile.remove_when_closed();
        assert!(profile.leaves_a_folder());
        profile.close_with(&mut Patience::none()).unwrap();
        assert!(!dir.exists());
        assert!(audit.exists(), "the audit trail stays");
    }
}

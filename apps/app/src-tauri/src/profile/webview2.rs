//! A real WebView2 with the profile folder this process chooses
//! (ST-M009-014): the test that would have caught an elevated window whose
//! browser could not use its folder.
//!
//! It opens the profile of a customer process exactly as the application
//! does ([`Profile::open`] with [`Base::current`]), creates a WebView2
//! environment and a controller in a hidden window with that folder, and
//! checks that both come up and that the browser made its data folder in it.
//! Then it closes everything and checks that the folder is gone.
//!
//! What it cannot show: the Windows computers of continuous verification run
//! the tests as an administrator with user account control switched off. The
//! process is elevated there, so the folder is the private one in
//! `CretLink Data`, but WebView2 does not start its browser through the shell
//! with an unelevated token, which is what kept the browser out of a folder
//! that admits only the administrators. That part is a manual check on real
//! hardware (`docs/manual-tests.md`, MT-10).

#![allow(
    unsafe_code,
    reason = "WebView2 and the window that holds it are COM and Win32 calls; \
              this test makes them the way the webview library does"
)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use cretlink_cli::webview_profile::Base;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2EnvironmentOptions,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::Win32::Foundation::{E_POINTER, E_UNEXPECTED, HWND};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::Profile;

/// How long the webview gets to come up: a first start of the runtime on a
/// busy machine takes a few seconds, never minutes.
const LIMIT: Duration = Duration::from_secs(120);

#[test]
fn the_customer_window_s_webview_comes_up_with_its_profile_folder() {
    let in_ci = std::env::var_os("CI").is_some();
    if tauri::webview_version().is_err() {
        assert!(!in_ci, "the WebView2 runtime is missing");
        eprintln!("skipped: no WebView2 runtime");
        return;
    }
    let base = Base::current();
    if !base.is_private() {
        // Continuous verification runs as an administrator: the elevated
        // path must be the one tested there.
        assert!(!in_ci, "the tests do not run with administrator rights");
        eprintln!("skipped: this process does not run with administrator rights");
        return;
    }
    let profile = Profile::open(true, &base).expect("the profile of a customer process");
    let dir = profile.directory().expect("a profile folder on Windows");
    assert!(dir.starts_with(base.dir()), "{}", dir.display());

    let (tx, rx) = mpsc::channel();
    let tried = dir.clone();
    // Its own thread, so that a webview that never answers fails the test
    // after the limit instead of holding it.
    std::thread::spawn(move || {
        let _ = tx.send(webview_comes_up(&tried));
    });
    let outcome = rx
        .recv_timeout(LIMIT)
        .unwrap_or_else(|_| Err(format!("no answer within {LIMIT:?}")));
    let browser_folder = dir.join("EBWebView").is_dir();

    // Removed whatever happened; WebView2 lets go of it within a few seconds.
    let closed = profile.close();
    outcome.unwrap_or_else(|err| panic!("the webview did not come up in {}: {err}", dir.display()));
    assert!(
        browser_folder,
        "the browser made no data folder in the profile"
    );
    closed.expect("the profile folder is removed");
    assert!(!dir.exists(), "{}", dir.display());
}

/// Creates a WebView2 environment with `dir` as its folder and a controller
/// in a hidden window, on this thread, and closes them again.
fn webview_comes_up(dir: &Path) -> Result<(), String> {
    // SAFETY: called once on this new thread, undone below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|err| format!("COM: {err}"))?;
    // SAFETY: a window of a system class, without a parent, never shown.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("CretLink test"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            640,
            480,
            None,
            None,
            None,
            None,
        )
    };
    let result = match window {
        Ok(window) => {
            let result = environment(dir).and_then(|environment| {
                let controller = controller(&environment, window)?;
                // SAFETY: the controller came up; it is closed once.
                unsafe { controller.CoreWebView2() }.map_err(|err| format!("webview: {err}"))?;
                // SAFETY: as above.
                unsafe { controller.Close() }.map_err(|err| format!("close: {err}"))
            });
            // SAFETY: the window was created on this thread above.
            let _ = unsafe { DestroyWindow(window) };
            result
        }
        Err(err) => Err(format!("window: {err}")),
    };
    // SAFETY: paired with the initialisation above, on the same thread.
    unsafe { CoUninitialize() };
    result
}

/// The WebView2 environment with `dir` as its folder.
fn environment(dir: &Path) -> Result<ICoreWebView2Environment, String> {
    let (tx, rx) = mpsc::channel();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |code, environment| {
            let result = code.and_then(|()| environment.ok_or_else(|| E_POINTER.into()));
            tx.send(result).map_err(|_| E_UNEXPECTED.into())
        },
    ));
    let folder = HSTRING::from(dir.as_os_str());
    // SAFETY: the strings and the handler outlive the call; the answer comes
    // to the handler on this thread while the messages are pumped below.
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &folder,
            None::<&ICoreWebView2EnvironmentOptions>,
            &handler,
        )
    }
    .map_err(|err| format!("environment: {err}"))?;
    webview2_com::wait_with_pump(rx)
        .map_err(|err| format!("environment: {err:?}"))?
        .map_err(|err| format!("environment: {err}"))
}

/// A controller of `environment` in `window`.
fn controller(
    environment: &ICoreWebView2Environment,
    window: HWND,
) -> Result<ICoreWebView2Controller, String> {
    let (tx, rx) = mpsc::channel();
    let handler =
        CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |code, controller| {
            let result = code.and_then(|()| controller.ok_or_else(|| E_POINTER.into()));
            tx.send(result).map_err(|_| E_UNEXPECTED.into())
        }));
    // SAFETY: the window and the handler outlive the call; the answer comes
    // to the handler on this thread while the messages are pumped below.
    unsafe { environment.CreateCoreWebView2Controller(window, &handler) }
        .map_err(|err| format!("controller: {err}"))?;
    webview2_com::wait_with_pump(rx)
        .map_err(|err| format!("controller: {err:?}"))?
        .map_err(|err| format!("controller: {err}"))
}

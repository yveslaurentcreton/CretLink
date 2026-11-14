//! Windows for the support program that a customer keeps (ST-M009-017).
//!
//! Every part of [`System`] is a call that exists already: the folder that
//! only administrators can change and the held program file come from
//! `cretlink_exec`, and the shortcut, the entry in Installed apps, the
//! process list and the removal after this process has exited are those of
//! the technician's install (`crate::install::win`), used for the whole
//! computer. This module adds no call to Windows of its own.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::{Layout, OsSystem, System};
use crate::install::win::{self, Hive};
use crate::install::{Shortcut, UninstallEntry};

/// The layout of this computer: the folder in `Program Files` and the
/// Start menu and the desktop of all users, each as the system names it.
pub(super) fn layout() -> Option<Layout> {
    Some(Layout::new(
        cretlink_exec::kept_program_folder()?,
        &win::common_programs()?,
        &win::public_desktop()?,
    ))
}

impl System for OsSystem {
    fn require_administrator(&self) -> io::Result<()> {
        cretlink_exec::require_administrator()
    }

    fn prepare_folder(&self, folder: &Path) -> io::Result<()> {
        cretlink_exec::prepare_program_folder(folder)
    }

    fn hold_running_program(&self) -> io::Result<File> {
        cretlink_exec::hold_running_program()
    }

    fn is_running(&self, exe: &Path) -> io::Result<bool> {
        win::running_elsewhere(exe)
    }

    fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()> {
        win::create_shortcut(shortcut)
    }

    fn write_uninstall_entry(&self, key: &str, entry: &UninstallEntry) -> io::Result<()> {
        win::write_uninstall_entry_in(Hive::LocalMachine, key, entry)
    }

    fn has_uninstall_entry(&self, key: &str) -> bool {
        win::has_uninstall_entry_in(Hive::LocalMachine, key)
    }

    fn uninstall_entry_location(&self, key: &str) -> Option<PathBuf> {
        win::uninstall_entry_location_in(Hive::LocalMachine, key)
    }

    fn shortcut_target(&self, path: &Path) -> io::Result<PathBuf> {
        win::shortcut_target(path)
    }

    fn remove_uninstall_entry(&self, key: &str) -> io::Result<bool> {
        win::remove_uninstall_entry_in(Hive::LocalMachine, key)
    }

    fn remove_after_exit(&self, files: &[PathBuf], dir: &Path) -> io::Result<()> {
        // The helper is started from the system folder that the system names,
        // with an environment built from it: nothing of the environment of
        // this elevated program is passed on.
        win::remove_after_exit(files, dir)
    }
}

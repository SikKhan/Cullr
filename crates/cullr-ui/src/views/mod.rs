//! egui views: Home, Grid and shared widget helpers.
//!
//! Views draw state handed to them and report user intent as [`Action`];
//! all state transitions live in the App state machine (`app.rs`).

pub mod grid;
pub mod home;
pub mod loupe;
pub mod modals;
pub mod widgets;

use std::path::PathBuf;

/// A user intent emitted by a view; executed by the state machine in
/// [`crate::app`].
#[derive(Debug)]
pub enum Action {
    /// Show the native folder picker.
    PickFolder,
    /// Scan the folder at this path and browse it in the grid.
    OpenFolder(PathBuf),
    /// Leave the grid and return Home.
    BackToHome,
    /// Open the About dialog (SPEC §10 T14).
    ShowAbout,
}

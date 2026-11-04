//! Where the daemon keeps its data, state and socket.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

/// Resolved directories; `XDG_*` variables are honoured.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// Holds the persisted copy of the current content.
    pub data: PathBuf,
    /// Holds `state.toml`.
    pub state: PathBuf,
    /// The IPC socket.
    pub socket: PathBuf,
}

impl Dirs {
    /// Resolves the directories for this user.
    ///
    /// # Errors
    /// Fails when no home directory or no socket location can be determined.
    pub fn new() -> Result<Self> {
        let project = ProjectDirs::from("", "", "tt500-lcd")
            .ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        let state = project
            .state_dir()
            .ok_or_else(|| anyhow!("cannot determine the state directory"))?
            .to_owned();
        let socket = tt500_lcd_ipc::socket_path().context("cannot locate the socket")?;
        Ok(Self {
            data: project.data_dir().to_owned(),
            state,
            socket,
        })
    }
}

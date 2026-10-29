//! The installed host (F4, milestone 1): a Windows service, online with the
//! computer's own ID from boot, and an agent in the user's session (see
//! [`crate::agent`]). macOS comes later (milestone 9).

use anyhow::Result;

use crate::ServiceAction;

#[cfg(windows)]
mod windows;

/// Above this size a log starts over (the previous one kept as `.old`).
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_LOG: u64 = 4 << 20;

/// The log file of the service or an agent (`name`), in the computer's
/// folder; `None` where there is none.
pub fn log_file(name: &str) -> Option<std::fs::File> {
    let dir = hext_crypto::machine_dir().ok()?;
    let path = dir.join(format!("{name}.log"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

pub fn command(action: ServiceAction) -> Result<()> {
    #[cfg(windows)]
    {
        match action {
            ServiceAction::Install => windows::install(),
            ServiceAction::Uninstall { purge } => windows::uninstall(purge),
            ServiceAction::Status => windows::status(),
            ServiceAction::Run => windows::run(),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = action;
        anyhow::bail!("o serviço por enquanto só existe no Windows")
    }
}

pub fn agent(pipe: &str) -> Result<()> {
    #[cfg(windows)]
    {
        windows::agent(pipe)
    }
    #[cfg(not(windows))]
    {
        let _ = pipe;
        anyhow::bail!("o agent só existe no Windows")
    }
}

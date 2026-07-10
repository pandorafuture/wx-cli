use std::path::{Path, PathBuf};

use crate::PathsError;

pub(crate) struct PlatformBaseDirs {
    pub config_root: PathBuf,
    pub cache_root: PathBuf,
    pub state_root: PathBuf,
    pub logs_root: PathBuf,
}

impl PlatformBaseDirs {
    pub(crate) fn resolve(home: &Path) -> Result<Self, PathsError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                config_root: home.join("Library/Application Support/wx-cli/config"),
                cache_root: home.join("Library/Caches/wx-cli"),
                state_root: home.join("Library/Application Support/wx-cli/state"),
                logs_root: home.join("Library/Logs/wx-cli"),
            })
        }

        #[cfg(target_os = "linux")]
        {
            let config_root = dirs::config_dir()
                .ok_or(PathsError::NoConfig)?
                .join("wx-cli");
            let cache_root = dirs::cache_dir().ok_or(PathsError::NoCache)?.join("wx-cli");
            let state_root = dirs::state_dir()
                .or_else(dirs::data_local_dir)
                .or_else(dirs::data_dir)
                .ok_or(PathsError::NoState)?
                .join("wx-cli");
            let logs_root = state_root.join("logs");
            Ok(Self {
                config_root,
                cache_root,
                state_root,
                logs_root,
            })
        }

        #[cfg(target_os = "windows")]
        {
            let config_root = dirs::config_dir()
                .ok_or(PathsError::NoConfig)?
                .join("wx-cli");
            let cache_root = dirs::cache_dir().ok_or(PathsError::NoCache)?.join("wx-cli");
            let local_data = dirs::data_local_dir()
                .or_else(dirs::data_dir)
                .ok_or(PathsError::NoState)?
                .join("wx-cli");
            let state_root = local_data.join("state");
            let logs_root = local_data.join("logs");
            Ok(Self {
                config_root,
                cache_root,
                state_root,
                logs_root,
            })
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            // Fallback: use XDG-like conventions
            let config_root = dirs::config_dir()
                .ok_or(PathsError::NoConfig)?
                .join("wx-cli");
            let cache_root = dirs::cache_dir().ok_or(PathsError::NoCache)?.join("wx-cli");
            let state_root = dirs::state_dir()
                .or_else(dirs::data_local_dir)
                .or_else(dirs::data_dir)
                .ok_or(PathsError::NoState)?
                .join("wx-cli");
            let logs_root = state_root.join("logs");
            Ok(Self {
                config_root,
                cache_root,
                state_root,
                logs_root,
            })
        }
    }
}

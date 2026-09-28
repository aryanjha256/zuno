//! Where Zuno keeps its files, on each platform.
//!
//! | | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`config_dir`] — settings, sessions | `~/.config/zuno` | `~/Library/Application Support/Zuno` | `%APPDATA%\Zuno` |
//! | [`data_dir`] — default collections and workspaces | `~/.local/share/zuno` | the same as config | the same as config |
//!
//! **Linux is exactly what these paths always were** — `dirs` reads `XDG_CONFIG_HOME` and
//! `XDG_DATA_HOME`, ignores either when it is not absolute, and otherwise falls back to
//! `~/.config` and `~/.local/share`, which is the rule the four hand-written copies this replaced
//! each implemented. So nothing moves for anyone who already has data. macOS and Windows had no
//! users when this was written, so there is nothing to migrate there either.
//!
//! **The folder is `zuno` on Linux and `Zuno` elsewhere**, following each platform: XDG
//! directories are lowercase by convention, while Application Support and `%APPDATA%` hold
//! folders named after the app.

use std::path::PathBuf;

/// The name of Zuno's own folder inside a platform directory.
pub fn app_dir_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "zuno"
    } else {
        "Zuno"
    }
}

/// Settings and sessions.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(app_dir_name()))
}

/// Where collections and workspaces go when nobody names a location. On macOS and Windows this is
/// the same folder as `config_dir`, as each platform does; the names inside it do not collide.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(app_dir_name()))
}

/// The person's home directory — `$HOME`, or on Windows the user profile, where `$HOME` is usually
/// not set at all.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Each platform's own location**, asserted per platform because the whole point is that
    /// they differ. Reads the real environment, so on shape rather than an exact value.
    #[test]
    fn each_platform_keeps_zuno_where_that_platform_expects() {
        let (Some(config), Some(data)) = (config_dir(), data_dir()) else {
            return;
        };
        assert!(config.is_absolute() && data.is_absolute(), "{config:?} {data:?}");
        assert!(config.ends_with(app_dir_name()), "{config:?}");
        assert!(data.ends_with(app_dir_name()), "{data:?}");

        #[cfg(target_os = "linux")]
        {
            // Collections are documents, not config: the two are separate on Linux, and mixing
            // them into `~/.config` is the mistake this has always asserted against.
            assert_ne!(config, data);
            assert!(!data.to_string_lossy().contains("/.config/"), "{data:?}");
        }
        #[cfg(target_os = "macos")]
        {
            assert!(
                config.ends_with("Library/Application Support/Zuno"),
                "{config:?}"
            );
            assert_eq!(config, data, "one folder on macOS");
        }
    }
}

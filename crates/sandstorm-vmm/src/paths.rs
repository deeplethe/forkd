//! Canonical on-disk paths. Used by both the CLI (direct mode) and the
//! controller (daemon mode), so they read/write the same snapshot store.
use std::path::PathBuf;

/// `$XDG_DATA_HOME/sandstorm/` or `~/.local/share/sandstorm/`.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(d).join("sandstorm");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/sandstorm")
}

pub fn snapshot_dir(tag: &str) -> PathBuf {
    data_dir().join("snapshots").join(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_dir_uses_data_dir() {
        let d = snapshot_dir("foo");
        assert!(d.ends_with("sandstorm/snapshots/foo"));
    }
}

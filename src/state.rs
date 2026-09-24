//! Sidecar file that remembers marks and position between runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = ".cull-state.json";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Stems of shots marked for deletion.
    pub marked: BTreeSet<String>,
    /// Stem of the shot being viewed.
    pub cursor: Option<String>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

pub fn load(dir: &Path) -> State {
    std::fs::read(path(dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, state: &State) -> Result<()> {
    let target = path(dir);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&State { version: 1, ..state.clone() })?)?;
    std::fs::rename(tmp, target)?;
    Ok(())
}

pub fn remove(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("cull-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), State::default());

        let state = State {
            version: 1,
            marked: ["a".to_string(), "b".to_string()].into(),
            cursor: Some("b".into()),
        };
        save(&dir, &state).unwrap();
        assert_eq!(load(&dir), state);

        // Files written while the edit mark existed still load.
        std::fs::write(path(&dir), r#"{"version":1,"marked":["a"],"edit":["c"],"cursor":null}"#).unwrap();
        assert_eq!(load(&dir).marked.len(), 1);

        remove(&dir);
        assert_eq!(load(&dir), State::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

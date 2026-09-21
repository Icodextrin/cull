//! Acting on marked shots: moving culls to the OS trash and edit picks into a subfolder.

use std::path::{Path, PathBuf};

use crate::catalog::Shot;

/// Folder, inside the dump directory, that shots marked for editing are moved into.
pub const NEEDS_EDIT_DIR: &str = "needs-edit";

#[derive(Default)]
pub struct Report {
    pub shots: usize,
    pub files: usize,
    pub failures: Vec<(PathBuf, String)>,
}

impl Report {
    /// Run `op` on every file of every shot; a shot counts as done only if all its files were.
    fn run<'a>(shots: impl IntoIterator<Item = &'a Shot>, mut op: impl FnMut(&Path) -> Result<(), String>) -> Report {
        let mut report = Report::default();
        for shot in shots {
            let mut ok = true;
            for file in shot.files() {
                match op(file) {
                    Ok(()) => report.files += 1,
                    Err(e) => {
                        ok = false;
                        report.failures.push((file.clone(), e));
                    }
                }
            }
            report.shots += ok as usize;
        }
        report
    }
}

fn trash_context() -> trash::TrashContext {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        // Avoids scripting Finder, which needs an extra permission prompt.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx
}

pub fn trash_shots<'a>(shots: impl IntoIterator<Item = &'a Shot>) -> Report {
    let ctx = trash_context();
    Report::run(shots, |file| ctx.delete(file).map_err(|e| e.to_string()))
}

/// Move every file of each shot into `dest`, creating it if needed. Never overwrites.
pub fn move_shots<'a>(shots: impl IntoIterator<Item = &'a Shot>, dest: &Path) -> Report {
    if let Err(e) = std::fs::create_dir_all(dest) {
        let msg = format!("could not create {}: {e}", dest.display());
        return Report::run(shots, |_| Err(msg.clone()));
    }
    Report::run(shots, |file| {
        let target = dest.join(file.file_name().ok_or("no file name")?);
        if target.exists() {
            return Err(format!("{} already exists", target.display()));
        }
        std::fs::rename(file, &target).map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_shots_moves_pairs_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("cull-move-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.JPG", "a.ARW", "b.JPG"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let dest = dir.join(NEEDS_EDIT_DIR);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("b.JPG"), "existing").unwrap();

        let shot = |stem: &str, raws: Vec<PathBuf>| Shot {
            stem: stem.into(),
            jpeg: dir.join(format!("{stem}.JPG")),
            raws,
            sidecars: vec![],
        };
        let shots = [shot("a", vec![dir.join("a.ARW")]), shot("b", vec![])];
        let report = move_shots(&shots, &dest);

        assert_eq!((report.shots, report.files, report.failures.len()), (1, 2, 1));
        assert!(dest.join("a.JPG").exists() && dest.join("a.ARW").exists());
        assert!(!dir.join("a.JPG").exists());
        // The clash is reported and neither file is touched.
        assert!(dir.join("b.JPG").exists());
        assert_eq!(std::fs::read_to_string(dest.join("b.JPG")).unwrap(), "existing");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

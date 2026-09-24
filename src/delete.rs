//! Moving culled shots to the OS trash.

use std::path::{Path, PathBuf};

use crate::catalog::Shot;

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

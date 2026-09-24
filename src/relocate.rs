//! Moving whatever survived the cull into a new, dated folder.

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{unbounded, Receiver};

/// Every visible regular file in `dir`. Hidden files (the state file, macOS `._` files) stay behind.
pub fn remaining_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// `2026:09:14 17:02:05` → `2026-09-14`.
fn exif_day(v: &str) -> Option<String> {
    let d = v.get(..10)?.replace(':', "-");
    let b = d.as_bytes();
    let ok = b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    (ok && !d.starts_with("0000")).then_some(d)
}

fn taken_day(jpeg: &Path) -> Option<String> {
    let exif = exif::Reader::new().read_from_container(&mut BufReader::new(File::open(jpeg).ok()?)).ok()?;
    match &exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(parts) => exif_day(&String::from_utf8_lossy(parts.first()?)),
        _ => None,
    }
}

fn modified_day(path: &Path) -> Option<String> {
    let t = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d").to_string())
}

/// Day the last picture was taken, as `YYYY-MM-DD`: the latest EXIF capture date of `jpegs`, falling
/// back to file modification times, then to today.
pub fn last_taken(jpegs: &[&Path], files: &[PathBuf]) -> String {
    jpegs
        .iter()
        .filter_map(|j| taken_day(j).or_else(|| modified_day(j)))
        .max()
        .or_else(|| files.iter().filter_map(|f| modified_day(f)).max())
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string())
}

/// `path` if nothing is there yet, otherwise the first free `path_1`, `path_2`, …
pub fn unique(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (1..)
        .map(|i| path.with_file_name(format!("{name}_{i}")))
        .find(|p| !p.exists())
        .unwrap()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Expands a leading `~`.
pub fn expand(s: &str) -> PathBuf {
    match (s.strip_prefix('~'), home()) {
        (Some(""), Some(h)) => h,
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(&rest[1..]),
        _ => PathBuf::from(s),
    }
}

/// Shortens the home directory back to `~` for display.
pub fn abbreviate(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Rename, or copy and delete when `dst` is on another filesystem (an SD card, a network share).
/// The copy goes to a hidden temporary name first, so an interrupted move never leaves a
/// truncated file under the real name, and the original is only removed once the copy is complete.
fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    match fs::rename(src, dst) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
        other => return other,
    }
    copy_then_remove(src, dst)
}

fn copy_then_remove(src: &Path, dst: &Path) -> io::Result<()> {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dst.with_file_name(format!(".{name}.cull-part"));
    let copy = || -> io::Result<()> {
        let meta = fs::metadata(src)?;
        let copied = fs::copy(src, &tmp)?;
        if copied != meta.len() {
            return Err(io::Error::other(format!("copied {copied} of {} bytes", meta.len())));
        }
        let out = File::options().write(true).open(&tmp)?;
        if let Ok(t) = meta.modified() {
            let _ = out.set_modified(t);
        }
        out.sync_all()?;
        fs::rename(&tmp, dst)
    };
    if let Err(e) = copy() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::remove_file(src)
}

#[derive(Default)]
pub struct Report {
    pub files: usize,
    pub failures: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

pub enum Event {
    /// Files dealt with so far, moved or failed.
    Progress(usize),
    Done(Report),
}

/// A move running on a background thread.
pub struct Job {
    pub events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    /// Stop after the file currently being moved.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Create `dest` and move `files` into it, reporting each file as it goes.
pub fn start(files: Vec<PathBuf>, dest: PathBuf, wake: impl Fn() + Send + 'static) -> Job {
    let (tx, events) = unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let mut report = Report::default();
        if let Err(e) = fs::create_dir_all(&dest) {
            report.failures.push((dest, e.to_string()));
        } else {
            for (i, file) in files.iter().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    report.cancelled = true;
                    break;
                }
                match move_file(file, &dest.join(file.file_name().unwrap_or_default())) {
                    Ok(()) => report.files += 1,
                    Err(e) => report.failures.push((file.clone(), e.to_string())),
                }
                let _ = tx.send(Event::Progress(i + 1));
                wake();
            }
        }
        let _ = tx.send(Event::Done(report));
        wake();
    });
    Job { events, cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cull-relocate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn days() {
        assert_eq!(exif_day("2026:09:14 17:02:05").as_deref(), Some("2026-09-14"));
        assert_eq!(exif_day("0000:00:00 00:00:00"), None);
        assert_eq!(exif_day("    :  :     :  :  "), None);
        assert_eq!(exif_day("2026"), None);
    }

    #[test]
    fn sample_images() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-images");
        let jpeg = dir.join("IMG_1959.JPG");
        if jpeg.exists() {
            assert_eq!(taken_day(&jpeg).as_deref(), Some("2026-09-19"));
        }
    }

    #[test]
    fn unique_appends_counter() {
        let dir = scratch("unique");
        let p = dir.join("2026-09-14");
        assert_eq!(unique(&p), p);
        fs::create_dir(&p).unwrap();
        assert_eq!(unique(&p), dir.join("2026-09-14_1"));
        fs::create_dir(dir.join("2026-09-14_1")).unwrap();
        assert_eq!(unique(&p), dir.join("2026-09-14_2"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tilde() {
        let h = home().unwrap();
        assert_eq!(expand("~"), h);
        assert_eq!(expand("~/Pictures/x"), h.join("Pictures/x"));
        assert_eq!(expand("/a/~b"), PathBuf::from("/a/~b"));
        assert_eq!(abbreviate(&h.join("Pictures")), "~/Pictures");
    }

    #[test]
    fn moves_visible_files() {
        let dir = scratch("move");
        for name in ["a.JPG", "a.ARW", "clip.MOV", ".cull-state.json", "._a.JPG"] {
            fs::write(dir.join(name), name).unwrap();
        }
        let files = remaining_files(&dir).unwrap();
        assert_eq!(files.len(), 3);
        let dest = dir.join("out/2026-09-14");
        let job = start(files, dest.clone(), || {});
        let report = loop {
            if let Event::Done(r) = job.events.recv().unwrap() {
                break r;
            }
        };
        assert_eq!((report.files, report.failures.len()), (3, 0));
        assert_eq!(fs::read_to_string(dest.join("a.ARW")).unwrap(), "a.ARW");
        assert!(!dir.join("a.JPG").exists() && dir.join(".cull-state.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_keeps_contents_and_time() {
        let dir = scratch("copy");
        let (src, dst) = (dir.join("a.CR2"), dir.join("b.CR2"));
        fs::write(&src, b"raw bytes").unwrap();
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options().write(true).open(&src).unwrap().set_modified(t).unwrap();
        copy_then_remove(&src, &dst).unwrap();
        assert!(!src.exists() && !dir.join(".b.CR2.cull-part").exists());
        assert_eq!(fs::read(&dst).unwrap(), b"raw bytes");
        assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), t);
        assert!(move_file(&dst, &dst).is_err(), "never overwrites");
        fs::remove_dir_all(&dir).unwrap();
    }
}

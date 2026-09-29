//! Moving whatever survived the cull into dated folders, one per day the pictures were taken.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{unbounded, Receiver};

use crate::catalog::Shot;

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

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Sorts `files` by the day each was taken, as `YYYY-MM-DD`, oldest day first. A shot's files (RAWs,
/// sidecars) go with its JPEG, dated by its EXIF capture date or else its modification time. Anything
/// else (videos, RAWs with no JPEG) is dated by its own modification time, then today.
pub fn by_day(files: Vec<PathBuf>, shots: &[Shot]) -> Vec<(String, Vec<PathBuf>)> {
    let mut shot_day: HashMap<&Path, String> = HashMap::new();
    for shot in shots.iter().filter(|s| s.jpeg.exists()) {
        if let Some(day) = taken_day(&shot.jpeg).or_else(|| modified_day(&shot.jpeg)) {
            for f in shot.files() {
                shot_day.insert(f, day.clone());
            }
        }
    }
    let mut days: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let day = shot_day.get(file.as_path()).cloned().or_else(|| modified_day(&file)).unwrap_or_else(today);
        days.entry(day).or_default().push(file);
    }
    days.into_iter().collect()
}

/// `<root>/YYYY/YYYY-MM-DD`. It may already exist, in which case the files are added to it.
pub fn day_folder(root: &Path, day: &str) -> PathBuf {
    root.join(&day[..4]).join(day)
}

/// `DSC0001.ARW.xmp` → (`DSC0001`, `.ARW.xmp`), so a shot's files all share a stem.
fn split_stem(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Where each of `files` should go in `dest`. `files` share a stem, and if any of their names is
/// taken they are all renamed `stem_1.ext`, `stem_2.ext`, …, so a JPEG and its RAW stay paired.
fn free_names(dest: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    (0..)
        .map(|n| {
            files
                .iter()
                .map(|f| {
                    let name = f.file_name().unwrap_or_default().to_string_lossy();
                    let (stem, rest) = split_stem(&name);
                    dest.join(if n == 0 { name.to_string() } else { format!("{stem}_{n}{rest}") })
                })
                .collect::<Vec<_>>()
        })
        .find(|names| names.iter().all(|p| !p.exists()))
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
        flush(&out)?;
        fs::rename(&tmp, dst)
    };
    if let Err(e) = copy() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::remove_file(src)
}

/// `sync_all`, which on macOS is `F_FULLFSYNC`. Network shares (smbfs, nfs) reject that with
/// ENOTSUP, so fall back to a plain `fsync`, which still pushes the data to the server.
fn flush(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => {
            use std::os::fd::AsRawFd;
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        }
        other => other,
    }
}

#[derive(Default)]
pub struct Report {
    pub files: usize,
    /// Files given a new name because theirs was taken: (from, to).
    pub renamed: Vec<(PathBuf, PathBuf)>,
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

/// Move each batch of files into its folder, creating it if needed, reporting each file as it goes.
/// Files whose names are already taken there are renamed (see `free_names`).
pub fn start(batches: Vec<(PathBuf, Vec<PathBuf>)>, wake: impl Fn() + Send + 'static) -> Job {
    let (tx, events) = unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let mut report = Report::default();
        let mut done = 0;
        'batches: for (dest, files) in batches {
            if let Err(e) = fs::create_dir_all(&dest) {
                report.failures.push((dest, e.to_string()));
                done += files.len();
                let _ = tx.send(Event::Progress(done));
                continue;
            }
            let mut shots: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
            for file in files {
                let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
                shots.entry(split_stem(&name).0.to_owned()).or_default().push(file);
            }
            for files in shots.into_values() {
                for (file, to) in files.iter().zip(free_names(&dest, &files)) {
                    if stop.load(Ordering::Relaxed) {
                        report.cancelled = true;
                        break 'batches;
                    }
                    match move_file(file, &to) {
                        Ok(()) => {
                            report.files += 1;
                            if to.file_name() != file.file_name() {
                                report.renamed.push((file.clone(), to));
                            }
                        }
                        Err(e) => report.failures.push((file.clone(), e.to_string())),
                    }
                    done += 1;
                    let _ = tx.send(Event::Progress(done));
                    wake();
                }
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
    fn stems() {
        assert_eq!(split_stem("DSC0001.ARW.xmp"), ("DSC0001", ".ARW.xmp"));
        assert_eq!(split_stem("DSC0001.JPG"), ("DSC0001", ".JPG"));
        assert_eq!(split_stem("README"), ("README", ""));
    }

    #[test]
    fn merges_and_renames_clashes() {
        let dir = scratch("merge");
        let dest = dir.join("out/2026/2026-09-14");
        fs::create_dir_all(&dest).unwrap();
        for name in ["a.JPG", "b.JPG", "b_1.ARW"] {
            fs::write(dest.join(name), "old").unwrap();
        }
        let src = dir.join("src");
        fs::create_dir(&src).unwrap();
        for name in ["a.ARW", "b.JPG", "b.ARW", "b.ARW.xmp", "c.JPG"] {
            fs::write(src.join(name), name).unwrap();
        }
        let files = remaining_files(&src).unwrap();
        let job = start(vec![(dest.clone(), files)], || {});
        let report = loop {
            if let Event::Done(r) = job.events.recv().unwrap() {
                break r;
            }
        };
        assert_eq!((report.files, report.failures.len(), report.renamed.len()), (5, 0, 3));
        // a.ARW doesn't clash, even though a.JPG is there.
        assert_eq!(fs::read_to_string(dest.join("a.ARW")).unwrap(), "a.ARW");
        assert_eq!(fs::read_to_string(dest.join("a.JPG")).unwrap(), "old");
        // b's files all move to the first suffix free for every one of them: b_1.ARW is taken.
        assert_eq!(fs::read_to_string(dest.join("b_2.JPG")).unwrap(), "b.JPG");
        assert_eq!(fs::read_to_string(dest.join("b_2.ARW")).unwrap(), "b.ARW");
        assert_eq!(fs::read_to_string(dest.join("b_2.ARW.xmp")).unwrap(), "b.ARW.xmp");
        assert_eq!(fs::read_to_string(dest.join("b.JPG")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("c.JPG")).unwrap(), "c.JPG");
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
        let job = start(vec![(dest.clone(), files)], || {});
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
    fn sorts_by_day() {
        let dir = scratch("days");
        let at = |name: &str, secs: u64| {
            let p = dir.join(name);
            fs::write(&p, name).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
            p
        };
        // Noon UTC, so the local day is the same in any time zone within ±11h.
        let (d1, d2) = (1_757_851_200, 1_757_851_200 + 86_400); // 2025-09-14, 2025-09-15
        let (jpeg, raw) = (at("a.JPG", d1), at("a.ARW", d2));
        let clip = at("clip.MOV", d2);
        let shot = Shot { stem: "a".into(), jpeg: jpeg.clone(), raws: vec![raw.clone()], sidecars: vec![] };
        let days = by_day(vec![raw.clone(), clip.clone(), jpeg.clone()], &[shot]);
        assert_eq!(
            days,
            vec![("2025-09-14".to_owned(), vec![raw, jpeg]), ("2025-09-15".to_owned(), vec![clip])],
            "the RAW follows its JPEG"
        );
        assert_eq!(day_folder(&dir, "2025-09-14"), dir.join("2025/2025-09-14"));
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

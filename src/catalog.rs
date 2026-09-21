//! Scans a directory and groups files sharing a basename into shots.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];
pub const RAW_EXTS: &[&str] = &[
    "arw", "cr2", "cr3", "crw", "nef", "nrw", "raf", "orf", "rw2", "dng", "pef", "srw", "3fr",
    "iiq", "rwl", "x3f", "erf", "kdc", "mef", "mos", "mrw", "sr2", "srf",
];
pub const SIDECAR_EXTS: &[&str] = &["xmp"];

/// One shot: a displayable JPEG plus every other file that shares its basename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    pub stem: String,
    pub jpeg: PathBuf,
    pub raws: Vec<PathBuf>,
    pub sidecars: Vec<PathBuf>,
}

impl Shot {
    /// Every file that should be removed when this shot is culled.
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.jpeg).chain(&self.raws).chain(&self.sidecars)
    }

    pub fn has_raw(&self) -> bool {
        !self.raws.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub shots: Vec<Shot>,
    /// RAW files with no matching JPEG. They can't be displayed, so they are never touched.
    pub orphan_raws: Vec<PathBuf>,
}

#[derive(Default)]
struct Group {
    jpegs: Vec<PathBuf>,
    raws: Vec<PathBuf>,
    sidecars: Vec<PathBuf>,
}

enum Kind {
    Jpeg,
    Raw,
    Sidecar,
}

fn classify(ext: &str) -> Option<Kind> {
    let ext = ext.to_ascii_lowercase();
    if JPEG_EXTS.contains(&ext.as_str()) {
        Some(Kind::Jpeg)
    } else if RAW_EXTS.contains(&ext.as_str()) {
        Some(Kind::Raw)
    } else if SIDECAR_EXTS.contains(&ext.as_str()) {
        Some(Kind::Sidecar)
    } else {
        None
    }
}

/// Splits a file name into (stem, kind). Sidecars may be named `X.xmp` or `X.ARW.xmp`.
fn split_name(name: &str) -> Option<(&str, Kind)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let kind = classify(ext)?;
    if matches!(kind, Kind::Sidecar)
        && let Some((inner, inner_ext)) = stem.rsplit_once('.')
        && matches!(classify(inner_ext), Some(Kind::Jpeg | Kind::Raw))
    {
        return Some((inner, kind));
    }
    if stem.is_empty() {
        return None;
    }
    Some((stem, kind))
}

impl Catalog {
    pub fn scan(dir: &Path) -> Result<Catalog> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        Ok(Catalog::from_names(dir, names))
    }

    pub fn from_names(dir: &Path, names: impl IntoIterator<Item = String>) -> Catalog {
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for name in names {
            // Skip hidden files, including macOS AppleDouble `._DSC0001.JPG` files on SD cards.
            if name.starts_with('.') {
                continue;
            }
            let Some((stem, kind)) = split_name(&name) else {
                continue;
            };
            let group = groups.entry(stem.to_owned()).or_default();
            let path = dir.join(&name);
            match kind {
                Kind::Jpeg => group.jpegs.push(path),
                Kind::Raw => group.raws.push(path),
                Kind::Sidecar => group.sidecars.push(path),
            }
        }

        let mut catalog = Catalog::default();
        for (stem, mut g) in groups {
            g.jpegs.sort();
            g.raws.sort();
            g.sidecars.sort();
            if g.jpegs.is_empty() {
                catalog.orphan_raws.extend(g.raws);
                continue;
            }
            let jpeg = g.jpegs.remove(0);
            // Any duplicate JPEGs (e.g. both .JPG and .jpeg) go with the shot too.
            let mut sidecars = g.jpegs;
            sidecars.extend(g.sidecars);
            catalog.shots.push(Shot { stem, jpeg, raws: g.raws, sidecars });
        }
        catalog
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(names: &[&str]) -> Catalog {
        Catalog::from_names(Path::new("/d"), names.iter().map(|s| s.to_string()))
    }

    #[test]
    fn pairs_by_basename_case_insensitive_ext() {
        let c = cat(&["DSC0001.ARW", "DSC0001.JPG", "IMG_1.cr3", "IMG_1.jpg", "b.NEF", "b.jpeg"]);
        let stems: Vec<_> = c.shots.iter().map(|s| s.stem.as_str()).collect();
        assert_eq!(stems, ["DSC0001", "IMG_1", "b"]);
        assert!(c.shots.iter().all(|s| s.raws.len() == 1));
        assert_eq!(c.shots[0].raws[0], Path::new("/d/DSC0001.ARW"));
    }

    #[test]
    fn jpeg_only_and_raw_only() {
        let c = cat(&["a.JPG", "b.ARW"]);
        assert_eq!(c.shots.len(), 1);
        assert!(!c.shots[0].has_raw());
        assert_eq!(c.orphan_raws, [PathBuf::from("/d/b.ARW")]);
    }

    #[test]
    fn sidecars_and_ignored_files() {
        let c = cat(&["a.JPG", "a.ARW", "a.ARW.xmp", "a.xmp", "._a.JPG", "notes.txt", "a.MOV"]);
        assert_eq!(c.shots.len(), 1);
        let files: Vec<_> = c.shots[0].files().cloned().collect();
        assert_eq!(files.len(), 4);
        assert!(files.contains(&PathBuf::from("/d/a.ARW.xmp")));
        assert!(files.contains(&PathBuf::from("/d/a.xmp")));
    }

    #[test]
    fn sorted_by_stem() {
        let c = cat(&["DSC0003.JPG", "DSC0001.JPG", "DSC0002.JPG"]);
        let stems: Vec<_> = c.shots.iter().map(|s| s.stem.as_str()).collect();
        assert_eq!(stems, ["DSC0001", "DSC0002", "DSC0003"]);
    }
}

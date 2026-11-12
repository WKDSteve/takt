// SPDX-License-Identifier: GPL-3.0-or-later
//! The playlist: what was dropped on the window, in order, with the entry
//! that plays.
//!
//! Dropped paths are foreign input (a file name may hold anything): they are
//! made absolute and normalised in one place ([`normalise`]) and only ever
//! shown as text. A folder stands for the media files directly in it, sorted
//! by name with numbers compared as numbers (`2` before `10`), as a file
//! manager shows them.

use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};

/// File extensions taken from a dropped folder (lower case). A dropped file
/// is taken whatever its extension: the backend decides what it can open.
pub const MEDIA_EXTENSIONS: [&str; 22] = [
    "mkv", "mp4", "m4v", "mov", "webm", "avi", "wmv", "ts", "m2ts", "mts", "mpg", "mpeg", "flv",
    "ogv", "mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma",
];

/// Whether `path` names a media file by its extension.
#[must_use]
pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// `path` absolute (against `base` when relative) with `.` and `..` resolved
/// lexically, without asking the file system (a dropped path need not exist
/// any more, and resolving links is not ours to do).
#[must_use]
pub fn normalise(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never above the root: `C:\..` stays `C:\`.
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Compares two names with runs of digits as numbers, the rest without
/// regard to case: `ep2` before `ep10`, `B` after `a`.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// The media files directly in `dir`, in natural order of their names.
///
/// # Errors
///
/// When the folder cannot be read.
pub fn media_in(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.is_file() && is_media(p))
        .collect();
    files.sort_by(|a, b| {
        natural_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    Ok(files)
}

/// The dropped `paths` as playlist entries: files as they are, folders as
/// their media files. What cannot be read is reported, not dropped silently.
#[must_use]
pub fn expand(paths: &[PathBuf], base: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        let path = normalise(path, base);
        if path.is_dir() {
            match media_in(&path) {
                Ok(files) if files.is_empty() => {
                    problems.push(format!("no media files in {}", path.display()));
                }
                Ok(files) => entries.extend(files),
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        } else {
            entries.push(path);
        }
    }
    (entries, problems)
}

/// Entries in order and the one that plays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Playlist {
    entries: Vec<PathBuf>,
    current: Option<usize>,
}

impl Playlist {
    /// The entries in playing order.
    #[must_use]
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// The index of the entry that plays.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// The entry that plays.
    #[must_use]
    pub fn current_entry(&self) -> Option<&Path> {
        self.current
            .and_then(|i| self.entries.get(i))
            .map(PathBuf::as_path)
    }

    /// Replaces the list (a drop) and plays its first entry.
    pub fn replace(&mut self, entries: Vec<PathBuf>) -> Option<&Path> {
        self.entries = entries;
        self.current = (!self.entries.is_empty()).then_some(0);
        self.current_entry()
    }

    /// Appends (a drop with Shift); plays the first new one when nothing played.
    pub fn append(&mut self, entries: Vec<PathBuf>) -> Option<&Path> {
        let start = self.entries.len();
        self.entries.extend(entries);
        if self.current.is_none() && self.entries.len() > start {
            self.current = Some(start);
            return self.current_entry();
        }
        None
    }

    /// Plays entry `index`.
    pub fn select(&mut self, index: usize) -> Option<&Path> {
        if index < self.entries.len() {
            self.current = Some(index);
        }
        self.current_entry()
    }

    /// The next entry, `None` at the end (the list stops there).
    pub fn next(&mut self) -> Option<&Path> {
        let next = self.current.map_or(0, |i| i + 1);
        if next >= self.entries.len() {
            return None;
        }
        self.current = Some(next);
        self.current_entry()
    }

    /// The entry before, `None` at the start.
    pub fn previous(&mut self) -> Option<&Path> {
        let previous = self.current?.checked_sub(1)?;
        self.current = Some(previous);
        self.current_entry()
    }

    /// Removes entry `index`. True when it was the one that played: the
    /// caller then plays [`current_entry`](Self::current_entry), the entry
    /// that moved into its place (or nothing at the end).
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        self.entries.remove(index);
        match self.current {
            Some(c) if c == index => {
                self.current = (index < self.entries.len()).then_some(index);
                true
            }
            Some(c) if c > index => {
                self.current = Some(c - 1);
                false
            }
            _ => false,
        }
    }

    /// Shuffles the entries after the one that plays (it stays and goes on
    /// playing), with a xorshift from `seed`.
    pub fn shuffle(&mut self, seed: u64) {
        let start = self.current.map_or(0, |c| c + 1);
        let mut state = seed | 1;
        let start = start.min(self.entries.len());
        let rest = &mut self.entries[start..];
        for i in (1..rest.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = usize::try_from(state % u64::try_from(i + 1).unwrap_or(1)).unwrap_or(0);
            rest.swap(i, j);
        }
    }

    /// The text of an entry for the list: its file name, never interpreted.
    #[must_use]
    pub fn label(path: &Path) -> String {
        path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn numbers_sort_as_numbers_and_case_does_not_count() {
        let mut names = vec![
            "ep10.mkv",
            "Ep2.mkv",
            "ep1.mkv",
            "a.mkv",
            "B.mkv",
            "ep02b.mkv",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "a.mkv",
                "B.mkv",
                "ep1.mkv",
                "Ep2.mkv",
                "ep02b.mkv",
                "ep10.mkv"
            ]
        );
    }

    #[test]
    fn paths_are_made_absolute_and_dots_resolved_without_the_disk() {
        let base = std::env::temp_dir();
        let got = normalise(Path::new("x/./y/../z.mkv"), &base);
        assert_eq!(got, base.join("x").join("z.mkv"));
        let root = base.ancestors().last().unwrap().to_path_buf();
        let above = normalise(&root.join("..").join("..").join("a.mkv"), &base);
        assert_eq!(above, root.join("a.mkv"), "never above the root");
    }

    #[test]
    fn a_folder_stands_for_its_media_files_in_order() {
        let dir = std::env::temp_dir().join(format!("takt-playlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for name in ["10.mkv", "2.MP4", "1.mkv", "notes.txt", "cover.jpg"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::write(dir.join("sub").join("0.mkv"), b"x").unwrap();
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let single = dir.join("notes.txt");
        let (entries, problems) = expand(&[dir.clone(), single.clone(), empty], &dir);
        let names: Vec<_> = entries.iter().map(|e| Playlist::label(e)).collect();
        assert_eq!(
            names,
            ["1.mkv", "2.MP4", "10.mkv", "notes.txt"],
            "a dropped file is taken as it is"
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("no media files in"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_and_previous_stop_at_the_ends() {
        let mut list = Playlist::default();
        assert_eq!(list.next(), None);
        assert_eq!(list.replace(p(&["a", "b", "c"])), Some(Path::new("a")));
        assert_eq!(list.previous(), None);
        assert_eq!(list.next(), Some(Path::new("b")));
        assert_eq!(list.next(), Some(Path::new("c")));
        assert_eq!(list.next(), None);
        assert_eq!(list.current(), Some(2), "the end keeps the last entry");
        assert_eq!(list.select(0), Some(Path::new("a")));
        assert_eq!(
            list.select(9),
            Some(Path::new("a")),
            "out of range changes nothing"
        );
    }

    #[test]
    fn append_plays_only_when_nothing_played() {
        let mut list = Playlist::default();
        assert_eq!(list.append(p(&["a"])), Some(Path::new("a")));
        assert_eq!(list.append(p(&["b"])), None);
        assert_eq!(list.entries().len(), 2);
    }

    #[test]
    fn removing_keeps_the_entry_that_plays() {
        let mut list = Playlist::default();
        list.replace(p(&["a", "b", "c", "d"]));
        list.select(2);
        assert!(!list.remove(0));
        assert_eq!(list.current_entry(), Some(Path::new("c")));
        assert!(list.remove(1), "the playing one");
        assert_eq!(
            list.current_entry(),
            Some(Path::new("d")),
            "its successor moves in"
        );
        assert!(list.remove(1));
        assert_eq!(list.current_entry(), None, "nothing after the end");
        assert!(!list.remove(7));
    }

    #[test]
    fn shuffle_keeps_the_playing_entry_and_all_others() {
        let mut list = Playlist::default();
        list.replace((0..20).map(|i| PathBuf::from(i.to_string())).collect());
        list.select(3);
        list.shuffle(42);
        assert_eq!(list.current_entry(), Some(Path::new("3")));
        assert_eq!(&list.entries()[..4], &p(&["0", "1", "2", "3"])[..]);
        let mut sorted: Vec<_> = list.entries().to_vec();
        sorted.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        assert_eq!(
            sorted,
            (0..20)
                .map(|i| PathBuf::from(i.to_string()))
                .collect::<Vec<_>>()
        );
        assert_ne!(
            list.entries()[4..],
            (4..20)
                .map(|i| PathBuf::from(i.to_string()))
                .collect::<Vec<_>>()[..],
            "it moved"
        );
    }
}

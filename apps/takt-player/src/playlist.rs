// SPDX-License-Identifier: GPL-3.0-or-later
//! The playlist: what was dropped on the window, in order, with the entry
//! that plays.
//!
//! Dropped paths are foreign input (a file name may hold anything): they are
//! made absolute and normalised in one place ([`normalise`]) and only ever
//! shown as text. A folder stands for the media files directly in it, sorted
//! by name with numbers compared as numbers (`2` before `10`), as a file
//! manager shows them.
//!
//! A playlist file (`.m3u`, `.m3u8`) stands for its entries ([`read_list`]).
//! It is read here, as text, and never handed to a backend: a backend that
//! reads lists (mpv does) would play their entries on its own, past the
//! window, addresses included. Only what lies in the folder of the list gets
//! in; an address, an absolute path and a path that leaves the folder are
//! named and skipped.

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

/// File extensions of a subtitle file (lower case): text formats that
/// are one file. A dropped one is not an entry of the playlist; it is shown
/// with the file that plays.
pub const SUBTITLE_EXTENSIONS: [&str; 4] = ["srt", "ass", "ssa", "vtt"];

/// Whether `path` names a subtitle file by its extension.
#[must_use]
pub fn is_subtitles(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SUBTITLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// File extensions of a playlist file (lower case).
pub const LIST_EXTENSIONS: [&str; 2] = ["m3u", "m3u8"];

/// The largest playlist file that is read: a megabyte of text.
const LIST_BYTES: u64 = 1 << 20;

/// The most entries taken from one playlist file.
const LIST_ENTRIES: usize = 10_000;

/// Whether `path` names a playlist file by its extension.
#[must_use]
pub fn is_list(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| LIST_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Why a line of a playlist file is no entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// It names a protocol (`http://`, `edl://`, `data:`): not a file.
    Address,
    /// It is an absolute path (a drive, a root, a network share).
    Absolute,
    /// It leaves the folder of the list through `..`.
    Outside,
    /// It is a playlist file itself: lists are not followed.
    List,
}

impl Skipped {
    const ALL: [Self; 4] = [Self::Address, Self::Absolute, Self::Outside, Self::List];

    const fn what(self) -> &'static str {
        match self {
            Self::Address => "addresses, not files",
            Self::Absolute => "absolute paths",
            Self::Outside => "outside the folder of the list",
            Self::List => "lists themselves",
        }
    }
}

/// Whether `line` starts like an address: a scheme of two or more characters
/// and a colon (`http:`, `data:`). One letter and a colon is a drive.
fn names_a_protocol(line: &str) -> bool {
    let Some((scheme, _)) = line.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    scheme.len() >= 2
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// One line of a playlist file as an entry: a path relative to `dir`, the
/// folder of the list, that stays in it.
///
/// # Errors
///
/// [`Skipped`] for everything else. Nothing is opened to decide this.
pub fn list_entry(line: &str, dir: &Path) -> Result<PathBuf, Skipped> {
    if names_a_protocol(line) {
        return Err(Skipped::Address);
    }
    let entry = Path::new(line);
    // A root or a drive, with either separator and also without a root after
    // the drive (`C:film.mkv`): none of these is relative to the list.
    let rooted = line.starts_with(['/', '\\']) || line.as_bytes().get(1) == Some(&b':');
    if rooted || entry.is_absolute() || entry.has_root() {
        return Err(Skipped::Absolute);
    }
    // A backslash separates on Windows only; a list written there is read
    // the same way everywhere.
    let relative: PathBuf = line.split(['/', '\\']).collect();
    let dir = normalise(dir, dir);
    let path = normalise(&relative, &dir);
    if !path.starts_with(&dir) || path == dir {
        return Err(Skipped::Outside);
    }
    if is_list(&path) {
        return Err(Skipped::List);
    }
    Ok(path)
}

/// The entries of the playlist file `list`, and what was not taken.
///
/// The file is read as text and nothing else: lines that are empty or start
/// with `#` are skipped, every other line is judged by [`list_entry`]. A file
/// larger than a megabyte is not read, and entries beyond the first ten
/// thousand are left out; both are reported.
#[must_use]
pub fn read_list(list: &Path) -> (Vec<PathBuf>, Vec<String>) {
    use std::io::Read as _;
    let name = Playlist::label(list);
    let dir = list.parent().unwrap_or_else(|| Path::new("."));
    let mut bytes = Vec::new();
    let read = std::fs::File::open(list)
        .and_then(|file| file.take(LIST_BYTES + 1).read_to_end(&mut bytes));
    if let Err(e) = read {
        return (Vec::new(), vec![format!("{name}: {e}")]);
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIST_BYTES {
        return (
            Vec::new(),
            vec![format!("{name}: larger than a megabyte, not read")],
        );
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut entries = Vec::new();
    let mut skipped = [0_usize; Skipped::ALL.len()];
    let mut beyond = 0_usize;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match list_entry(line, dir) {
            Ok(_) if entries.len() >= LIST_ENTRIES => beyond += 1,
            Ok(path) => entries.push(path),
            Err(why) => {
                if let Some(at) = Skipped::ALL.iter().position(|&s| s == why) {
                    skipped[at] += 1;
                }
            }
        }
    }
    let mut problems: Vec<String> = Skipped::ALL
        .iter()
        .zip(skipped)
        .filter(|&(_, count)| count > 0)
        .map(|(why, count)| format!("{name}: {count} skipped ({})", why.what()))
        .collect();
    if beyond > 0 {
        problems.push(format!(
            "{name}: {beyond} beyond the first {LIST_ENTRIES} left out"
        ));
    }
    if entries.is_empty() && problems.is_empty() {
        problems.push(format!("{name}: no entries"));
    }
    (entries, problems)
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
/// their media files, a playlist file as its entries ([`read_list`]). What
/// cannot be read is reported, not dropped silently.
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
        } else if is_list(&path) {
            let (files, skipped) = read_list(&path);
            entries.extend(files);
            problems.extend(skipped);
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
    fn a_line_of_a_list_is_a_file_in_its_folder_or_nothing() {
        let dir = std::env::temp_dir().join("takt-lists");
        let ok = |line: &str| list_entry(line, &dir).unwrap();
        assert_eq!(ok("a.mkv"), dir.join("a.mkv"));
        assert_eq!(
            ok("season 1/ep 2.mkv"),
            dir.join("season 1").join("ep 2.mkv")
        );
        assert_eq!(
            ok("season 1\\ep 2.mkv"),
            dir.join("season 1").join("ep 2.mkv")
        );
        assert_eq!(
            ok("./x/../b.mkv"),
            dir.join("b.mkv"),
            "dots that stay inside"
        );

        let no = |line: &str| list_entry(line, &dir).unwrap_err();
        for address in [
            "http://127.0.0.1/a.mkv",
            "https://example.org/a.mkv",
            "edl://a.mkv",
            "memory://x",
            "av://lavfi:testsrc",
            "file:///C:/a.mkv",
            "data:text/plain,x",
            "smb://server/share/a.mkv",
        ] {
            assert_eq!(no(address), Skipped::Address, "{address}");
        }
        for absolute in [
            "/etc/passwd",
            "\\\\server\\share\\a.mkv",
            "//server/share/a.mkv",
            "\\a.mkv",
            "C:\\films\\a.mkv",
            "C:/films/a.mkv",
            "C:a.mkv",
        ] {
            assert_eq!(no(absolute), Skipped::Absolute, "{absolute}");
        }
        for outside in ["../a.mkv", "x/../../a.mkv", "..\\a.mkv", "..", ".", "x/.."] {
            assert_eq!(no(outside), Skipped::Outside, "{outside}");
        }
        assert_eq!(no("more.m3u"), Skipped::List);
        assert_eq!(no("sub/MORE.M3U8"), Skipped::List);
    }

    #[test]
    fn a_list_file_is_read_as_text_and_stands_for_its_entries() {
        let dir = std::env::temp_dir().join(format!("takt-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("Evening.M3U8");
        std::fs::write(
            &list,
            "\u{feff}#EXTM3U\r\n#EXTINF:10,One\r\n  one.mkv  \r\n\r\nsub/two.mp4\r\n\
             http://127.0.0.1:9/three.mkv\r\nedl://four.mkv\r\n../five.mkv\r\n\
             C:\\six.mkv\r\nother.m3u\r\n",
        )
        .unwrap();
        assert!(is_list(&list) && !is_list(&dir.join("one.mkv")));
        assert!(is_subtitles(Path::new("film.SRT")) && is_subtitles(Path::new("a/b.ass")));
        assert!(!is_subtitles(&list) && !is_subtitles(Path::new("film.mkv")));
        assert!(!is_subtitles(Path::new("srt")), "a name, not an extension");

        // Dropped next to a plain file: the list stands for its entries.
        let (entries, problems) = expand(&[list.clone(), dir.join("zero.mkv")], &dir);
        assert_eq!(
            entries,
            [
                dir.join("one.mkv"),
                dir.join("sub").join("two.mp4"),
                dir.join("zero.mkv")
            ],
            "the list itself is no entry"
        );
        assert_eq!(
            problems,
            [
                "Evening.M3U8: 2 skipped (addresses, not files)",
                "Evening.M3U8: 1 skipped (absolute paths)",
                "Evening.M3U8: 1 skipped (outside the folder of the list)",
                "Evening.M3U8: 1 skipped (lists themselves)",
            ]
        );

        // Not text, empty, missing, too large: reported, never an entry.
        let odd = dir.join("odd.m3u");
        std::fs::write(&odd, [0xff, 0xfe, b'\n', b'#', b'x']).unwrap();
        let (entries, problems) = read_list(&odd);
        // The first line is no valid text; it is still only a name in the
        // folder of the list, which nothing has to open.
        assert_eq!(entries.len(), 1);
        assert!(entries[0].starts_with(&dir) && problems.is_empty());
        std::fs::write(&odd, "# nothing\n\n").unwrap();
        assert_eq!(
            read_list(&odd),
            (vec![], vec!["odd.m3u: no entries".to_owned()])
        );
        let (entries, problems) = read_list(&dir.join("missing.m3u"));
        assert!(entries.is_empty() && problems.len() == 1, "{problems:?}");
        assert!(problems[0].starts_with("missing.m3u: "));

        let line = "a.mkv\n";
        let too_many = LIST_ENTRIES + 3;
        std::fs::write(&odd, line.repeat(too_many)).unwrap();
        let (entries, problems) = read_list(&odd);
        assert_eq!(entries.len(), LIST_ENTRIES);
        assert_eq!(
            problems,
            [format!(
                "odd.m3u: 3 beyond the first {LIST_ENTRIES} left out"
            )]
        );
        std::fs::write(&odd, "a".repeat(usize::try_from(LIST_BYTES).unwrap() + 1)).unwrap();
        assert_eq!(
            read_list(&odd),
            (
                vec![],
                vec!["odd.m3u: larger than a megabyte, not read".to_owned()]
            )
        );
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

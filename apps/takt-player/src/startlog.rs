// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a failed start and a panic leave their message.
//!
//! A release build for Windows is a window program: started from the Start menu it has no
//! console, and what `main` and a panic write to stderr goes nowhere. So both also go into a log
//! file, `takt-player.log` in the folder for temporary files (where F4 saves the diagnostics
//! too; not under `%LOCALAPPDATA%\takt`, which the uninstall removes only when it is empty), and
//! without a console a message window of the system shows them (`console::show_error`). Started
//! from a terminal, everything is written there as before.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

/// A log larger than this starts anew: it only has to hold the last failures.
const LIMIT: u64 = 64 * 1024;

/// The log file: `takt-player.log` in the folder for temporary files.
pub fn path() -> PathBuf {
    std::env::temp_dir().join("takt-player.log")
}

/// Appends `message` as one line to the log at `path`, with the time (seconds since 1970) and
/// the version. Failing to write is no second failure to report: it is ignored.
pub fn record_at(path: &Path, message: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let fresh = std::fs::metadata(path).is_ok_and(|m| m.len() > LIMIT);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(!fresh)
        .truncate(fresh)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(
            file,
            "{secs} takt-player {}: {message}",
            env!("CARGO_PKG_VERSION")
        );
    }
}

/// A failure of the start: to the log, and into a message window when there is no console to
/// read stderr in. The caller has written it to stderr already.
pub fn report(message: &str) {
    record_at(&path(), message);
    if !crate::console::has_console() {
        crate::console::show_error(&format!(
            "{message}\n\nThe message is also in {}.",
            path().display()
        ));
    }
}

/// What a panic says: where, and its message when it has one.
fn panic_text(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let what = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    match info.location() {
        Some(at) => format!("panic at {}:{}: {what}", at.file(), at.line()),
        None => format!("panic: {what}"),
    }
}

/// Lets every panic go to the log as well, after the default hook has written it to stderr. A
/// panic of the main thread without a console is shown in a message window: the process ends
/// with it, and nothing else would tell.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let text = panic_text(info);
        record_at(&path(), &text);
        if std::thread::current().name() == Some("main") && !crate::console::has_console() {
            crate::console::show_error(&format!(
                "takt-player stopped: {text}\n\nThe message is also in {}.",
                path().display()
            ));
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("takt-startlog-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_failure_is_appended_as_a_line_with_the_version() {
        let path = temp_log("append");
        record_at(&path, "no window: no adapter");
        record_at(&path, "second");
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let version = concat!(" takt-player ", env!("CARGO_PKG_VERSION"), ": ");
        assert!(
            lines[0].ends_with(&format!("{version}no window: no adapter")),
            "{text}"
        );
        assert!(lines[1].ends_with(&format!("{version}second")), "{text}");
    }

    #[test]
    fn a_log_beyond_its_limit_starts_anew() {
        let path = temp_log("limit");
        std::fs::write(&path, vec![b'x'; usize::try_from(LIMIT).unwrap() + 1]).unwrap();
        record_at(&path, "fresh");
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 1, "{}", text.len());
        assert!(text.ends_with(": fresh\n"), "{text}");
    }

    #[test]
    fn a_panic_names_where_and_what() {
        // The hook is global: it keeps only the panic of this thread, so a test that panics
        // meanwhile on another thread changes nothing here.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let into = std::sync::Arc::clone(&seen);
        let me = std::thread::current().id();
        let before = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() == me {
                *into.lock().unwrap() = panic_text(info);
            }
        }));
        let _ = std::panic::catch_unwind(|| panic!("no adapter {}", 7));
        std::panic::set_hook(before);
        let text = seen.lock().unwrap().clone();
        assert!(text.starts_with("panic at "), "{text}");
        assert!(text.contains("startlog.rs:"), "{text}");
        assert!(text.ends_with(": no adapter 7"), "{text}");
    }
}

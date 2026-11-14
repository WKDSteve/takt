// SPDX-License-Identifier: GPL-3.0-or-later
//! `takt-player --uninstall`: the program removes the install it is part of, in its own process.
//!
//! This is what the entry in Settings > Apps runs. It takes away what `scripts/install.ps1` made,
//! with the checks of `scripts/uninstall.ps1` (which stays, for the terminal): the Start menu entry and the desktop shortcut when they lead into this install, the
//! files of a release, and last the entry in Settings > Apps. It asks first, in a window; no
//! shell and no other process is started.
//!
//! The install is always the folder this program lies in. The other places are the ones of the
//! user, unless an option or the environment names them (as `install.ps1` takes them; a test
//! passes a throw-away registry root and temp folders).
//!
//! Windows does not let a running program delete its own file, and marking it for deletion at
//! the next restart needs administrator rights, which an install for one user never has. So the
//! program moves its own file out of the install folder into the folder for temporary files and
//! leaves it there, free to delete; the cleanup of Windows takes it. Everything else is gone when
//! this ends.
//!
//! [`Product`] holds all that is specific to takt; the rest knows nothing about it. The calls of
//! Windows are in `sys`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod sys;

/// What an app tells the uninstall about itself.
pub struct Product {
    /// The name in the question, of the key of the Settings > Apps entry and of the folder the
    /// installer makes (`<name>\bin`).
    pub name: &'static str,
    /// The file name of this program.
    pub program: &'static str,
    /// The file name of the shortcut in the Start menu and on the desktop.
    pub link: &'static str,
    /// The programs of an install: none of them may run from it.
    pub programs: &'static [&'static str],
    /// The files of an install, relative to its folder; `VERSION` marks a folder as one.
    pub files: &'static [&'static str],
    /// The value of the start with Windows, when the app has one.
    pub run_value: Option<&'static str>,
    /// The first part of the environment variables that name the places (`<PREFIX>_START_MENU_DIR`).
    pub env: &'static str,
    /// How the user closes the app, for the refusal while it runs.
    pub close: &'static str,
    /// What stays, as the subject of a sentence ("your profiles").
    pub kept: &'static str,
}

/// takt: the player; it has no start with Windows.
pub const PRODUCT: Product = Product {
    name: "takt",
    program: "takt-player.exe",
    link: "Takt.lnk",
    programs: &["takt-player.exe"],
    files: &[
        "takt-player.exe",
        "uninstall.ps1",
        "LICENSE",
        "README.md",
        "VERSION",
    ],
    run_value: None,
    env: "TAKT",
    close: "close its window",
    kept: "your media files",
};

/// The file that marks a folder as an install of a release.
const MARK: &str = "VERSION";

/// What the command line asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    /// Do not ask and show no window: the answer is the output and the exit code.
    yes: bool,
    allow_elevated: bool,
    start_menu: Option<String>,
    desktop: Option<String>,
    registry_root: Option<String>,
}

/// Reads the arguments after the program name; `--uninstall` is among them.
fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| format!("{name} needs a value after it"))
        };
        match arg.as_str() {
            "--uninstall" => {}
            "--yes" => options.yes = true,
            "--allow-elevated" => options.allow_elevated = true,
            "--start-menu-dir" => options.start_menu = Some(value("--start-menu-dir")?),
            "--desktop-dir" => options.desktop = Some(value("--desktop-dir")?),
            "--registry-root" => options.registry_root = Some(value("--registry-root")?),
            other => return Err(format!("--uninstall does not go with {other}")),
        }
    }
    Ok(options)
}

/// Where the parts of an install are.
#[derive(Debug)]
struct Places {
    install: PathBuf,
    /// The folders of the shortcuts; `None` when the account has no such folder (a service
    /// account): then there is no shortcut to look for.
    start_menu: Option<PathBuf>,
    desktop: Option<PathBuf>,
    /// The key below `HKEY_CURRENT_USER` that stands for `Software`.
    software: String,
}

/// `path` as Windows compares paths: whole, and without regard to letter case.
fn folded(path: &Path) -> String {
    let whole = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    whole
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Whether `path` lies inside `dir`.
fn under(path: &Path, dir: &Path) -> bool {
    folded(path).starts_with(&(folded(dir) + "\\"))
}

fn same(a: &Path, b: &Path) -> bool {
    folded(a) == folded(b)
}

/// An absolute folder that is not a drive root.
fn place(dir: &str) -> Result<PathBuf, String> {
    let path = Path::new(dir);
    if dir.is_empty() || dir.contains('"') || !path.is_absolute() {
        return Err(format!("not an absolute path: '{dir}'"));
    }
    let whole = std::path::absolute(path).map_err(|e| format!("not a path: '{dir}': {e}"))?;
    if whole.parent().is_none() {
        return Err(format!("a drive root is no place for this: '{dir}'"));
    }
    Ok(whole)
}

/// The key below `HKEY_CURRENT_USER` that `root` names (`HKCU:\Software`, as the scripts write
/// it): a key of the current user, never another hive.
fn registry_root(root: &str) -> Result<String, String> {
    let root = root.trim_end_matches('\\');
    let below = [r"HKCU:\", r"Registry::HKEY_CURRENT_USER\"]
        .iter()
        .find_map(|hive| {
            let head = root.get(..hive.len())?;
            head.eq_ignore_ascii_case(hive).then(|| &root[hive.len()..])
        });
    match below {
        Some(key) if !key.is_empty() && !key.contains('"') => Ok(key.to_owned()),
        _ => Err(format!("not a key below HKCU: '{root}'")),
    }
}

/// What an option says, else the environment, else nothing.
fn setting(given: Option<&str>, product: &Product, name: &str) -> Option<String> {
    given
        .map(str::to_owned)
        .or_else(|| std::env::var(format!("{}_{name}", product.env)).ok())
        .filter(|value| !value.is_empty())
}

/// The places of the install this program lies in, after the checks that come before anything
/// is asked or removed.
fn prepare(product: &Product, options: &Options, program: &Path) -> Result<Places, String> {
    if std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty()) {
        return Err(
            "remove in a normal session, not in a Claude session (the app redirects AppData and the registry)"
                .to_owned(),
        );
    }
    if !options.allow_elevated && sys::elevated() {
        return Err(
            "do not run this elevated: it removes the installation of one user (--allow-elevated for a build machine)"
                .to_owned(),
        );
    }
    let install = program
        .parent()
        .filter(|dir| dir.parent().is_some())
        .ok_or_else(|| format!("{} lies in no folder of its own", program.display()))?
        .to_path_buf();
    if !install.join(MARK).is_file() {
        return Err(format!(
            "{} is not an install of {} (no {MARK} in it): nothing was removed",
            install.display(),
            product.name
        ));
    }
    let folder = |given: Option<&str>, name: &str, which: sys::Folder| {
        setting(given, product, name)
            .map_or_else(|| Ok(sys::folder(which)), |dir| place(&dir).map(Some))
    };
    let places = Places {
        start_menu: folder(
            options.start_menu.as_deref(),
            "START_MENU_DIR",
            sys::Folder::Programs,
        )?,
        desktop: folder(
            options.desktop.as_deref(),
            "DESKTOP_DIR",
            sys::Folder::Desktop,
        )?,
        software: match setting(options.registry_root.as_deref(), product, "REGISTRY_ROOT") {
            Some(root) => registry_root(&root)?,
            None => "Software".to_owned(),
        },
        install,
    };
    let running = sys::running(product.programs, |path| under(path, &places.install));
    if !running.is_empty() {
        let names: Vec<String> = running
            .iter()
            .map(|(path, pid)| {
                let name = path.file_name().unwrap_or(path.as_os_str());
                format!("{} (PID {pid})", name.to_string_lossy())
            })
            .collect();
        return Err(format!(
            "running from {}: {}; close {} ({}), then remove it again",
            places.install.display(),
            names.join(", "),
            product.name,
            product.close
        ));
    }
    Ok(places)
}

/// What a removal did.
#[derive(Debug, Default)]
struct Report {
    removed: Vec<String>,
    /// What stayed, and why.
    notes: Vec<String>,
}

fn remove_file(path: &Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))
}

/// The program a Run value starts, when it has the form the app writes: the path in quotes,
/// then nothing or one option.
fn run_program(command: &str) -> Option<&str> {
    let rest = command.strip_prefix('"')?;
    let (path, tail) = rest.split_once('"')?;
    (tail.is_empty() || (tail.starts_with(" --") && !tail[3..].contains([' ', '"'])))
        .then_some(path)
}

/// Removes the shortcut `link` when it leads into `install`.
fn remove_link(link: &Path, install: &Path, what: &str, report: &mut Report) -> Result<(), String> {
    if !link.is_file() {
        return Ok(());
    }
    match sys::shortcut_target(link) {
        Some(target) if under(&target, install) => {
            remove_file(link)?;
            report.removed.push(what.to_owned());
        }
        target => report.notes.push(format!(
            "kept {} (it starts {}, not this install)",
            link.display(),
            target.map_or_else(
                || "nothing readable".to_owned(),
                |t| t.display().to_string()
            )
        )),
    }
    Ok(())
}

/// Removes the folder `dir` when it is empty; one that is in use stays, with a word.
fn remove_empty(dir: &Path, report: &mut Report) {
    let empty = fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none());
    if empty && let Err(e) = fs::remove_dir(dir) {
        report.notes.push(format!(
            "kept the empty folder {} (in use: {e})",
            dir.display()
        ));
    }
}

/// The name the file of this program gets in the folder for temporary files.
fn moved_name(product: &Product) -> String {
    let stem = product.program.trim_end_matches(".exe");
    format!("{stem}-removed-{}.exe", std::process::id())
}

/// Takes away the programs earlier removals left in `temp`: by now nothing runs from them.
fn sweep(product: &Product, temp: &Path) {
    let head = format!("{}-removed-", product.program.trim_end_matches(".exe"));
    for entry in fs::read_dir(temp).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&head) && name.ends_with(".exe") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Removes the install at `places`. `program` is the file of the running program, which cannot
/// be deleted and is moved to `temp` instead. An error leaves the entry in Settings > Apps, so
/// the removal can run again.
fn remove(
    product: &Product,
    places: &Places,
    program: &Path,
    temp: &Path,
) -> Result<Report, String> {
    let mut report = Report::default();
    let install = &places.install;
    let windows = format!(r"{}\Microsoft\Windows\CurrentVersion", places.software);
    sweep(product, temp);

    // 1. The start with Windows, when it starts this install; one of another copy stays.
    if let Some(name) = product.run_value {
        let key = format!(r"{windows}\Run");
        if let Some(command) = sys::registry_text(&key, name) {
            if run_program(&command).is_some_and(|exe| under(Path::new(exe), install)) {
                sys::remove_registry_value(&key, name)
                    .map_err(|e| format!("cannot remove the start with Windows: {e}"))?;
                report.removed.push("the start with Windows".to_owned());
            } else {
                report.notes.push(format!(
                    "kept the Run value '{name}' (it is not this install): {command}"
                ));
            }
        }
    }

    // 2. The shortcuts, when they lead into the install folder.
    for (dir, what) in [
        (&places.start_menu, "the Start menu entry"),
        (&places.desktop, "the desktop shortcut"),
    ] {
        if let Some(dir) = dir {
            remove_link(&dir.join(product.link), install, what, &mut report)?;
        }
    }

    // 3. The files the installer puts there, with the .new and .old of an install that was cut
    // short; a folder that holds anything else stays. The mark goes last: until then this can
    // run again. The running program itself cannot be deleted and is moved out.
    let mut own = None;
    let marked_last = product
        .files
        .iter()
        .filter(|name| **name != MARK)
        .chain([&MARK]);
    for name in marked_last {
        for suffix in ["", ".new", ".old"] {
            let file = install.join(format!("{name}{suffix}"));
            if !file.is_file() {
                continue;
            }
            if same(&file, program) {
                own = Some(file);
                continue;
            }
            remove_file(&file)?;
            if suffix.is_empty() {
                report.removed.push(file.display().to_string());
            }
        }
    }
    if let Some(own) = own {
        let moved = temp.join(moved_name(product));
        match fs::rename(&own, &moved) {
            Ok(()) => {
                report.removed.push(own.display().to_string());
                report.notes.push(format!(
                    "the running program was moved to {}: Windows does not let it delete itself; that file is free to delete, and the cleanup of temporary files takes it",
                    moved.display()
                ));
            }
            Err(e) => report.notes.push(format!(
                "{} stays, delete it and its folder by hand: Windows does not let a running program delete itself, and it could not be moved to {} ({e})",
                own.display(),
                temp.display()
            )),
        }
    }
    // Not from inside the folder that is to go.
    let _ = std::env::set_current_dir(temp);
    remove_empty(install, &mut report);
    // The parent only when it is the folder the installer makes (never a folder the user chose).
    if let Some(parent) = install.parent()
        && install.file_name().is_some_and(|leaf| leaf == "bin")
        && parent.file_name().is_some_and(|leaf| leaf == product.name)
    {
        remove_empty(parent, &mut report);
    }

    // 4. The Settings > Apps entry last: while anything above fails, it stays to run again.
    let key = format!(r"{windows}\Uninstall\{}", product.name);
    if let Some(location) = sys::registry_text(&key, "InstallLocation") {
        if same(Path::new(&location), install) {
            sys::remove_registry_key(&key)
                .map_err(|e| format!("cannot remove the Settings > Apps entry: {e}"))?;
            report.removed.push("the Settings > Apps entry".to_owned());
        } else {
            report.notes.push(format!(
                "kept the Settings > Apps entry (it is for {location}, not this install)"
            ));
        }
    }
    Ok(report)
}

/// `text` with a capital first letter, to begin a sentence.
fn capital(text: &str) -> String {
    let mut letters = text.chars();
    letters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(letters).collect()
    })
}

/// Runs `--uninstall` for `product` with the arguments after the program name: asks, removes and
/// says what happened, on the terminal when there is one and in a window unless `--yes` was given.
pub fn run<I: IntoIterator<Item = String>>(product: &Product, args: I) -> ExitCode {
    let name = product.name;
    let args: Vec<String> = args.into_iter().collect();
    let quiet = args.iter().any(|arg| arg == "--yes");
    let options = match parse(args) {
        Ok(options) => options,
        Err(problem) => {
            eprintln!("{}: {problem}", product.program);
            // Settings > Apps has no terminal: a command that is wrong must not end in silence.
            if !quiet {
                sys::tell(name, &format!("{name} was not removed.\n\n{problem}"), true);
            }
            return ExitCode::from(2);
        }
    };
    let outcome = std::env::current_exe()
        .map_err(|e| format!("where is this program? {e}"))
        .and_then(|program| {
            let places = prepare(product, &options, &program)?;
            let question = format!(
                "Remove {name} from this computer?\n\n{}\n\n{} stay.",
                places.install.display(),
                capital(product.kept)
            );
            if !options.yes && !sys::ask(name, &question) {
                return Ok(None);
            }
            remove(product, &places, &program, &std::env::temp_dir()).map(Some)
        });
    match outcome {
        Ok(Some(report)) => {
            for line in &report.removed {
                println!("removed {line}");
            }
            for note in &report.notes {
                println!("{note}");
            }
            println!("{} were not touched", product.kept);
            if !options.yes {
                let mut parts = vec![format!("{name} was removed.")];
                parts.extend(report.notes.iter().cloned());
                parts.push(format!("{} were not touched.", capital(product.kept)));
                sys::tell(name, &parts.join("\n\n"), false);
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("nothing was removed");
            ExitCode::FAILURE
        }
        Err(problem) => {
            eprintln!("{}: uninstall failed: {problem}", product.program);
            if !options.yes {
                sys::tell(name, &format!("{name} was not removed.\n\n{problem}"), true);
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sys::TestPlace;
    use super::*;

    /// An app of the tests: with every part an app can have.
    const DEMO: Product = Product {
        name: "demo",
        program: "demo-app.exe",
        link: "demo.lnk",
        programs: &["demo.exe", "demo-app.exe"],
        files: &[
            "demo.exe",
            "demo-app.exe",
            "README.md",
            "LICENSE",
            "VERSION",
        ],
        run_value: Some("demo"),
        env: "DEMO_UNINSTALL_TEST",
        close: "close its window",
        kept: "your files",
    };

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|&word| word.to_owned()).collect()
    }

    #[test]
    fn the_options_are_read() {
        assert_eq!(parse(words(&["--uninstall"])), Ok(Options::default()));
        let options = parse(words(&[
            "--uninstall",
            "--yes",
            "--allow-elevated",
            "--start-menu-dir",
            r"C:\a b",
            "--desktop-dir",
            r"C:\d",
            "--registry-root",
            r"HKCU:\Software\x",
        ]))
        .unwrap();
        assert!(options.yes && options.allow_elevated);
        assert_eq!(options.start_menu.as_deref(), Some(r"C:\a b"));
        assert_eq!(options.desktop.as_deref(), Some(r"C:\d"));
        assert_eq!(options.registry_root.as_deref(), Some(r"HKCU:\Software\x"));
        assert!(parse(words(&["--uninstall", "--demo"])).is_err());
        assert!(parse(words(&["--uninstall", "profile.toml"])).is_err());
        assert!(parse(words(&["--uninstall", "--desktop-dir"])).is_err());
    }

    #[test]
    fn a_registry_root_is_a_key_of_the_user() {
        assert_eq!(registry_root(r"HKCU:\Software").as_deref(), Ok("Software"));
        assert_eq!(
            registry_root(r"hkcu:\Software\test-1\").as_deref(),
            Ok(r"Software\test-1")
        );
        assert_eq!(
            registry_root(r"Registry::HKEY_CURRENT_USER\Software").as_deref(),
            Ok("Software")
        );
        assert!(registry_root(r"HKLM:\Software\x").is_err());
        assert!(registry_root(r"HKCU:\").is_err());
        assert!(registry_root("HKCU:").is_err());
        assert!(registry_root(r"C:\Software").is_err());
        assert!(registry_root("HKCU:\\Soft\"ware").is_err());
    }

    #[test]
    fn a_place_is_an_absolute_folder_that_is_no_drive_root() {
        assert_eq!(place(r"C:\a\b\"), Ok(PathBuf::from(r"C:\a\b")));
        assert!(place("").is_err());
        assert!(place(r"a\b").is_err());
        assert!(place(r"C:\").is_err());
        assert!(place("C:\\a\"b").is_err());
    }

    #[test]
    fn inside_a_folder_is_not_beside_it() {
        let dir = Path::new(r"C:\Apps\demo\bin");
        assert!(under(Path::new(r"c:\apps\DEMO\bin\demo-app.exe"), dir));
        assert!(!under(Path::new(r"C:\Apps\demo\bin2\demo-app.exe"), dir));
        assert!(!under(dir, dir));
        assert!(same(Path::new(r"c:\apps\demo\BIN\"), dir));
    }

    #[test]
    fn a_run_value_names_its_program_only_in_the_form_the_app_writes() {
        assert_eq!(
            run_program(r#""C:\a b\demo-app.exe""#),
            Some(r"C:\a b\demo-app.exe")
        );
        assert_eq!(
            run_program(r#""C:\a\demo-app.exe" --tray"#),
            Some(r"C:\a\demo-app.exe")
        );
        assert_eq!(run_program(r"C:\a\demo-app.exe --tray"), None);
        assert_eq!(run_program(r#""C:\a\demo-app.exe" --tray more"#), None);
        assert_eq!(run_program(r#"cmd /c "C:\a\demo-app.exe""#), None);
    }

    /// An install in a temp folder with its registry below a key of this test; both go with it.
    struct Install {
        root: PathBuf,
        registry: TestPlace,
        places: Places,
        program: PathBuf,
    }

    impl Install {
        fn new(what: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("demo-app-uninstall-{what}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let install = root.join("demo").join("bin");
            for dir in [
                &install,
                &root.join("menu"),
                &root.join("desk"),
                &root.join("temp"),
            ] {
                fs::create_dir_all(dir).unwrap();
            }
            for name in DEMO.files {
                fs::write(install.join(name), name).unwrap();
            }
            let registry = TestPlace::new(&format!("uninstall-{what}"));
            let places = Places {
                install: install.clone(),
                start_menu: Some(root.join("menu")),
                desktop: Some(root.join("desk")),
                software: registry.key().to_owned(),
            };
            let program = install.join(DEMO.program);
            Self {
                root,
                registry,
                places,
                program,
            }
        }

        fn key(&self, below: &str) -> String {
            format!(
                r"{}\Microsoft\Windows\CurrentVersion\{below}",
                self.registry.key()
            )
        }

        fn enter(&self, location: &Path) {
            sys::write_registry_text(
                &self.key(r"Uninstall\demo"),
                "InstallLocation",
                &location.display().to_string(),
            );
        }

        fn remove(&self) -> Result<Report, String> {
            remove(&DEMO, &self.places, &self.program, &self.root.join("temp"))
        }
    }

    impl Drop for Install {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn an_install_goes_whole_and_its_own_program_is_moved_out() {
        let it = Install::new("whole");
        let install = it.places.install.clone();
        fs::write(install.join("demo.exe.new"), "cut short").unwrap();
        fs::write(install.join("VERSION.old"), "earlier").unwrap();
        fs::write(it.root.join("temp").join("demo-app-removed-1.exe"), "left").unwrap();
        it.enter(&install);
        let command = format!("\"{}\" --tray", it.program.display());
        sys::write_registry_text(&it.key("Run"), "demo", &command);

        let report = it.remove().unwrap();

        assert!(!it.root.join("demo").exists(), "the folder and its parent");
        assert_eq!(sys::registry_text(&it.key("Run"), "demo"), None);
        assert_eq!(
            sys::registry_text(&it.key(r"Uninstall\demo"), "InstallLocation"),
            None
        );
        let moved: Vec<String> = fs::read_dir(it.root.join("temp"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            moved,
            [moved_name(&DEMO)],
            "what an earlier removal left is swept"
        );
        for what in ["the start with Windows", "the Settings > Apps entry"] {
            assert!(report.removed.iter().any(|line| line == what), "{what}");
        }
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("was moved to"))
        );
    }

    #[test]
    fn what_is_not_of_this_install_stays() {
        let it = Install::new("foreign");
        let install = it.places.install.clone();
        fs::write(install.join("mine.txt"), "mine").unwrap();
        it.enter(Path::new(r"C:\another\demo\bin"));
        sys::write_registry_text(&it.key("Run"), "demo", "cmd /c something-of-yours");
        fs::write(it.root.join("menu").join("demo.lnk"), "no shortcut").unwrap();

        let report = it.remove().unwrap();

        assert_eq!(
            fs::read_dir(&install).unwrap().count(),
            1,
            "only the file of somebody else is left"
        );
        assert!(install.join("mine.txt").is_file());
        assert_eq!(
            sys::registry_text(&it.key("Run"), "demo").as_deref(),
            Some("cmd /c something-of-yours")
        );
        assert!(sys::registry_text(&it.key(r"Uninstall\demo"), "InstallLocation").is_some());
        assert!(it.root.join("menu").join("demo.lnk").is_file());
        assert_eq!(
            report
                .notes
                .iter()
                .filter(|n| n.starts_with("kept "))
                .count(),
            3
        );
    }

    #[test]
    fn a_file_that_cannot_go_is_an_error_and_leaves_the_entry_and_the_mark() {
        let it = Install::new("held");
        let install = it.places.install.clone();
        it.enter(&install);
        // A file somebody holds open without sharing it cannot be removed.
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _held = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(install.join("LICENSE"))
                .unwrap();
            let problem = it.remove().unwrap_err();
            assert!(problem.contains("cannot remove") && problem.contains("LICENSE"));
        }
        assert!(install.join(MARK).is_file(), "so it can run again");
        assert!(sys::registry_text(&it.key(r"Uninstall\demo"), "InstallLocation").is_some());
        it.remove().unwrap();
        assert!(!install.exists());
    }

    #[test]
    fn an_account_without_shortcut_folders_loses_its_install_all_the_same() {
        let mut it = Install::new("nofolders");
        let install = it.places.install.clone();
        it.places.start_menu = None;
        it.places.desktop = None;
        it.enter(&install);
        let report = it.remove().unwrap();
        assert!(!install.exists());
        assert!(
            report
                .removed
                .iter()
                .any(|line| line == "the Settings > Apps entry")
        );
    }

    #[test]
    fn a_folder_that_is_no_install_is_refused() {
        let it = Install::new("unmarked");
        fs::remove_file(it.places.install.join(MARK)).unwrap();
        let options = Options {
            allow_elevated: true,
            start_menu: Some(it.root.join("menu").display().to_string()),
            desktop: Some(it.root.join("desk").display().to_string()),
            registry_root: Some(format!(r"HKCU:\{}", it.registry.key())),
            ..Options::default()
        };
        // A Claude session is refused before anything else; the check after it is the one meant.
        if std::env::var_os("CLAUDECODE").is_none() {
            let problem = prepare(&DEMO, &options, &it.program).unwrap_err();
            assert!(problem.contains("not an install of demo"), "{problem}");
        }
        assert!(it.program.is_file());
    }
}

#[cfg(test)]
mod product {
    use super::*;

    #[test]
    fn the_product_names_its_own_program_and_the_mark() {
        let product = &PRODUCT;
        assert!(product.files.contains(&product.program));
        assert!(product.programs.contains(&product.program));
        assert!(product.files.contains(&MARK));
        let extension = |name: &str| {
            Path::new(name)
                .extension()
                .map(std::ffi::OsStr::to_ascii_lowercase)
        };
        assert_eq!(extension(product.program), Some("exe".into()));
        assert_eq!(extension(product.link), Some("lnk".into()));
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! The state of the player and what a key means, without a window: the UI
//! only reads this state and turns input into [`Command`]s, so all of it is
//! tested here.

/// What the person asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Play when paused, pause when playing.
    TogglePlay,
    /// Close the player.
    Quit,
    /// Jump by this many milliseconds (negative: back).
    SeekBy(i64),
    /// Jump to the start.
    SeekStart,
    /// Show or hide the diagnostic overlay.
    ToggleOverlay,
    /// Save the diagnostics as text (clipboard and a file).
    SaveDiagnostics,
    /// Jump to this position in milliseconds (a click on the timeline).
    SeekTo(i64),
    /// Louder by one step.
    VolumeUp,
    /// Quieter by one step.
    VolumeDown,
    /// Mute, or unmute to the volume before.
    ToggleMute,
    /// The next entry of the playlist.
    Next,
    /// The entry before in the playlist.
    Previous,
    /// Show or hide the playlist window.
    TogglePlaylist,
    /// Show or hide the list of keys.
    ToggleHelp,
}

/// How far the volume keys move the volume.
pub const VOLUME_STEP: f32 = 0.05;

/// How far the arrow keys jump.
pub const SEEK_STEP_MILLIS: i64 = 5_000;

/// A key the player knows, independent of the window library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The space bar.
    Space,
    /// Escape.
    Escape,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home (Pos1).
    Home,
    /// F3.
    F3,
    /// F4.
    F4,
    /// `9`, quieter (as in mpv).
    Nine,
    /// `0`, louder (as in mpv).
    Zero,
    /// `m`, mute (as in mpv).
    M,
    /// `>`, next entry (as in mpv).
    Greater,
    /// `<`, entry before (as in mpv).
    Less,
    /// F8, the playlist (as in mpv).
    F8,
    /// `?`, the list of keys.
    Question,
}

/// Every key with its label and what it does: the one table the help overlay
/// and `docs/KEYS.md` are made from (a test holds the file to it).
pub const BINDINGS: [(Key, &str, &str); 14] = [
    (Key::Space, "Space", "play / pause"),
    (Key::Left, "Left", "jump back 5 s"),
    (Key::Right, "Right", "jump forward 5 s"),
    (Key::Home, "Home", "jump to the start"),
    (Key::Nine, "9", "quieter (5 %)"),
    (Key::Zero, "0", "louder (5 %)"),
    (Key::M, "m", "mute / unmute"),
    (Key::Greater, ">", "next entry of the playlist"),
    (Key::Less, "<", "entry before in the playlist"),
    (Key::F8, "F8", "show / hide the playlist"),
    (Key::Question, "?", "show / hide this list of keys"),
    (Key::F3, "F3", "show / hide the diagnostics"),
    (Key::F4, "F4", "save the diagnostics (clipboard and a file)"),
    (Key::Escape, "Escape", "quit"),
];

/// What the mouse does, for the same list.
pub const MOUSE: [(&str, &str); 3] = [
    ("Click / drag on the timeline", "jump there"),
    (
        "Drop files or folders",
        "play them (a folder: its media files by name)",
    ),
    ("Drop with Shift held", "append them to the playlist"),
];

/// `docs/KEYS.md`, made from [`BINDINGS`] and [`MOUSE`].
#[must_use]
pub fn keys_markdown() -> String {
    let mut out = String::from(
        "# Tasten von takt-player\n\n\
         Erzeugt aus `BINDINGS` in `apps/takt-player/src/app.rs`; der Test \
         `docs_keys_md_is_the_table` hält die Datei daran fest (neu schreiben: \
         `TAKT_WRITE_KEYS=1 cargo test -p takt-player docs_keys_md`). Das Hilfe-Overlay \
         (`?`) zeigt dieselbe Tabelle.\n\n| Taste | Wirkung |\n|---|---|\n",
    );
    for (_, label, what) in BINDINGS {
        let label = label.replace('|', "\\|");
        out.push_str(&format!("| `{label}` | {what} |\n"));
    }
    out.push_str("\n| Maus | Wirkung |\n|---|---|\n");
    for (what, effect) in MOUSE {
        out.push_str(&format!("| {what} | {effect} |\n"));
    }
    out
}

/// The command of a key.
#[must_use]
pub fn command_for(key: Key) -> Command {
    match key {
        Key::Space => Command::TogglePlay,
        Key::Escape => Command::Quit,
        Key::Left => Command::SeekBy(-SEEK_STEP_MILLIS),
        Key::Right => Command::SeekBy(SEEK_STEP_MILLIS),
        Key::Home => Command::SeekStart,
        Key::F3 => Command::ToggleOverlay,
        Key::F4 => Command::SaveDiagnostics,
        Key::Nine => Command::VolumeDown,
        Key::Zero => Command::VolumeUp,
        Key::M => Command::ToggleMute,
        Key::Greater => Command::Next,
        Key::Less => Command::Previous,
        Key::F8 => Command::TogglePlaylist,
        Key::Question => Command::ToggleHelp,
    }
}

/// Where a jump of `by` from `now` lands, inside `0..=duration` (all in
/// milliseconds).
#[must_use]
pub fn seek_target(now: i64, by: i64, duration: i64) -> i64 {
    now.saturating_add(by).clamp(0, duration.max(0))
}

/// Playing or paused, the volume, and whether the overlay shows.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    playing: bool,
    volume: f32,
    muted: bool,
    overlay: bool,
    playlist: bool,
    help: bool,
}

impl Default for PlayerState {
    /// Playing, at full volume.
    fn default() -> Self {
        Self {
            playing: true,
            volume: 1.0,
            muted: false,
            overlay: false,
            playlist: false,
            help: false,
        }
    }
}

impl PlayerState {
    /// Whether it plays.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.playing
    }

    /// Whether the diagnostic overlay shows.
    #[must_use]
    pub fn overlay(&self) -> bool {
        self.overlay
    }

    /// Volume from 0 (silent) to 1 (unchanged).
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Whether the playlist window shows.
    #[must_use]
    pub fn playlist(&self) -> bool {
        self.playlist
    }

    /// Whether the list of keys shows.
    #[must_use]
    pub fn help(&self) -> bool {
        self.help
    }

    /// Whether it is muted (the volume is kept for unmuting).
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// What the sound path plays with: 0 when muted, else the volume.
    #[must_use]
    pub fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Sets the volume, kept to 0..=1 (a value that is no number is 0).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
    }

    /// Applies `command`; true when the player should close.
    pub fn apply(&mut self, command: Command) -> bool {
        match command {
            Command::TogglePlay => {
                self.playing = !self.playing;
                false
            }
            Command::Quit => true,
            Command::ToggleOverlay => {
                self.overlay = !self.overlay;
                false
            }
            Command::VolumeUp | Command::VolumeDown => {
                let step = if command == Command::VolumeUp {
                    VOLUME_STEP
                } else {
                    -VOLUME_STEP
                };
                self.set_volume(self.volume + step);
                // A volume key unmutes, as in mpv.
                self.muted = false;
                false
            }
            Command::ToggleMute => {
                self.muted = !self.muted;
                false
            }
            Command::TogglePlaylist => {
                self.playlist = !self.playlist;
                false
            }
            Command::ToggleHelp => {
                self.help = !self.help;
                false
            }
            // The clock and the source jump, the window saves; the state does
            // not change.
            Command::SeekBy(_)
            | Command::SeekStart
            | Command::SeekTo(_)
            | Command::Next
            | Command::Previous
            | Command::SaveDiagnostics => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_toggles_and_escape_quits() {
        let mut state = PlayerState::default();
        assert!(state.playing());
        assert!(!state.apply(command_for(Key::Space)));
        assert!(!state.playing());
        assert!(!state.apply(command_for(Key::Space)));
        assert!(state.playing());
        assert!(state.apply(command_for(Key::Escape)));
    }

    #[test]
    fn the_arrows_jump_and_home_goes_to_the_start() {
        assert_eq!(command_for(Key::Left), Command::SeekBy(-SEEK_STEP_MILLIS));
        assert_eq!(command_for(Key::Right), Command::SeekBy(SEEK_STEP_MILLIS));
        assert_eq!(command_for(Key::Home), Command::SeekStart);
        let mut state = PlayerState::default();
        assert!(!state.apply(Command::SeekBy(5_000)));
        assert!(state.playing(), "a jump does not pause");
    }

    #[test]
    fn f3_toggles_the_overlay_and_f4_saves() {
        let mut state = PlayerState::default();
        assert!(!state.overlay());
        assert!(!state.apply(command_for(Key::F3)));
        assert!(state.overlay());
        assert_eq!(command_for(Key::F4), Command::SaveDiagnostics);
        assert!(!state.apply(Command::SaveDiagnostics));
        assert!(state.overlay() && state.playing());
        state.apply(Command::ToggleOverlay);
        assert!(!state.overlay());
    }

    #[test]
    fn nine_and_zero_change_the_volume_and_m_mutes() {
        let mut state = PlayerState::default();
        state.apply(command_for(Key::Nine));
        assert!((state.volume() - 0.95).abs() < 1e-6);
        state.apply(command_for(Key::M));
        assert!(state.muted());
        assert!(state.effective_volume().abs() < f32::EPSILON);
        assert!((state.volume() - 0.95).abs() < 1e-6, "the volume is kept");
        state.apply(command_for(Key::Zero));
        assert!(!state.muted(), "a volume key unmutes");
        assert!((state.effective_volume() - 1.0).abs() < 1e-6);
        state.apply(Command::VolumeUp);
        assert!((state.volume() - 1.0).abs() < f32::EPSILON, "never above 1");
        for _ in 0..30 {
            state.apply(Command::VolumeDown);
        }
        assert!(state.volume().abs() < f32::EPSILON, "never below 0");
        assert!(!state.apply(Command::SeekTo(1_000)) && state.playing());
    }

    #[test]
    fn every_key_is_in_the_table_once() {
        // A new Key that is not in BINDINGS fails here: the match lists them all.
        let all = |key: Key| match key {
            Key::Space
            | Key::Escape
            | Key::Left
            | Key::Right
            | Key::Home
            | Key::F3
            | Key::F4
            | Key::Nine
            | Key::Zero
            | Key::M
            | Key::Greater
            | Key::Less
            | Key::F8
            | Key::Question => (),
        };
        for (key, _, _) in BINDINGS {
            all(key);
            assert_eq!(
                BINDINGS.iter().filter(|(k, _, _)| *k == key).count(),
                1,
                "{key:?}"
            );
        }
        assert_eq!(BINDINGS.len(), 14, "every variant of Key, once");
    }

    #[test]
    fn the_playlist_keys() {
        assert_eq!(command_for(Key::Greater), Command::Next);
        assert_eq!(command_for(Key::Less), Command::Previous);
        let mut state = PlayerState::default();
        state.apply(command_for(Key::F8));
        state.apply(command_for(Key::Question));
        assert!(state.playlist() && state.help());
        assert!(!state.apply(Command::Next) && state.playing());
    }

    #[test]
    fn docs_keys_md_is_the_table() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/KEYS.md");
        let wanted = keys_markdown();
        if std::env::var("TAKT_WRITE_KEYS").as_deref() == Ok("1") {
            std::fs::write(path, &wanted).unwrap();
        }
        let have = std::fs::read_to_string(path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            have, wanted,
            "docs/KEYS.md is not the table: TAKT_WRITE_KEYS=1 cargo test -p takt-player docs_keys_md"
        );
    }

    #[test]
    fn a_jump_stays_inside_the_source() {
        assert_eq!(seek_target(10_000, -5_000, 60_000), 5_000);
        assert_eq!(seek_target(2_000, -5_000, 60_000), 0);
        assert_eq!(seek_target(58_000, 5_000, 60_000), 60_000);
        assert_eq!(seek_target(0, i64::MAX, 60_000), 60_000);
    }

    #[test]
    fn the_volume_stays_in_range() {
        let mut state = PlayerState::default();
        state.set_volume(0.25);
        assert!((state.volume() - 0.25).abs() < f32::EPSILON);
        state.set_volume(3.0);
        assert!((state.volume() - 1.0).abs() < f32::EPSILON);
        state.set_volume(-1.0);
        assert!(state.volume().abs() < f32::EPSILON);
        state.set_volume(f32::NAN);
        assert!(state.volume().abs() < f32::EPSILON);
    }
}

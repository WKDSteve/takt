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
}

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
    overlay: bool,
}

impl Default for PlayerState {
    /// Playing, at full volume.
    fn default() -> Self {
        Self {
            playing: true,
            volume: 1.0,
            overlay: false,
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
            // The clock and the source jump, the window saves; the state does
            // not change.
            Command::SeekBy(_) | Command::SeekStart | Command::SaveDiagnostics => false,
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

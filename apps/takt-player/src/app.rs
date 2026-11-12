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
}

/// A key the player knows, independent of the window library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The space bar.
    Space,
    /// Escape.
    Escape,
}

/// The command of a key.
#[must_use]
pub fn command_for(key: Key) -> Command {
    match key {
        Key::Space => Command::TogglePlay,
        Key::Escape => Command::Quit,
    }
}

/// Playing or paused, and the volume.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    playing: bool,
    volume: f32,
}

impl Default for PlayerState {
    /// Playing, at full volume.
    fn default() -> Self {
        Self {
            playing: true,
            volume: 1.0,
        }
    }
}

impl PlayerState {
    /// Whether it plays.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.playing
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

-- SPDX-License-Identifier: GPL-3.0-or-later
-- The window with the test picture, by the names a screen reader hears (egui through accesskit):
--   testing sheets step test-picture-window
-- What it replaces of the hand runs: play, pause and mute in the real window, and the overlay (F3) in a
-- picture for the judge. Needs sheet-driver of appkit, else it is a skip; not part of the CI.
return {
    id = "takt.test-picture-window",
    feature = "takt.window-controls",
    task = "takt/handabnahme-overlay-drag-drop-installer",
    title = "takt: the window with the test picture, its controls and the overlay",
    engine = "desktop",
    app = "takt-test-picture",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 20000, note = "the window is there and plays" },
        { click = "Pause", note = "pause" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { click = "Play", note = "play again" },
        { expect = { name = "Pause" }, note = "the button says Pause" },
        { click = "Mute", note = "mute" },
        { expect = { name = "Unmute" }, note = "the button says Unmute" },
        { click = "Unmute", note = "sound again" },
        { expect = { name = "Mute" }, note = "the button says Mute" },
        -- The volume slider is not addressed: egui names its label "Volume" too, three elements answer to it.
        { shot = "overlay" },
    },
    judge = {
        -- The picture of the test picture itself may stay black in the shot (a wgpu surface the capture does not see).
        "in the picture overlay: the overlay with the error histogram, the anchor age and the line audio saying SIMULATED",
    },
}

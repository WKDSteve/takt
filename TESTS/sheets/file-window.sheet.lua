-- SPDX-License-Identifier: GPL-3.0-or-later
-- The window playing the test file of the repository through libmpv, by the names a screen reader hears:
--   testing sheets step file-window
-- What it replaces of the hand run of the MpvBackend: the file plays and pauses in the real window, and the menus of
-- the bar list the audio and subtitle tracks (the file has one audio and two subtitle tracks). Needs sheet-driver of
-- appkit and a build with libmpv (TAKT_LIBMPV_DIR, scripts/build-sheet-app.py); silent. Not part of the CI.
return {
    id = "takt.file-window",
    feature = "takt.window-file",
    task = "takt/mpv-backend-im-fenster-von-hand-abnehmen",
    title = "takt: a file plays in the window, with its tracks in the menus of the bar",
    engine = "desktop",
    app = "takt-file",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 30000, note = "the window is there and the file plays" },
        { wait = { name = "Subtitles" }, note = "the bar has the menu of the subtitle tracks (only for a file)" },
        { click = "Pause", note = "pause" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { click = "Subtitles", note = "open the list of the subtitle tracks" },
        { wait = { name = "1 (deu)" }, note = "the list is open: off and the two tracks (they come a frame later)" },
        { click = "1 (deu)", note = "choose the first subtitle track" },
        { click = "Subtitles", note = "open the list again" },
        { wait = { name = "1 (deu)" }, note = "the list is open" },
        { shot = "subtitles" },
        { click = "Play", note = "play on" },
        { expect = { name = "Pause" }, note = "the button says Pause" },
    },
    judge = {
        "in the picture subtitles: the pattern of the file, the list of the subtitle tracks open (off, 1 (deu), 2 (eng)) with 1 (deu) marked, and its text in the picture",
    },
}

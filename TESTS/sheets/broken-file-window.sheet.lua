-- SPDX-License-Identifier: GPL-3.0-or-later
-- A file that is no film, then a film: the bar says once why the first one does not play, and the second plays.
--   testing sheets step broken-file-window
-- What it replaces of the hand run of the MpvBackend: "drop a text file with the ending .mkv: the bar says cannot
-- play, nothing plays; an entry after it in the playlist plays". The two files are named on the command line, which
-- fills the playlist as a drop does. Needs sheet-driver of appkit, a build with libmpv and ffmpeg; silent. Not part
-- of the CI.
return {
    id = "takt.broken-file-window",
    feature = "takt.window-broken-file",
    task = "takt/mpv-backend-im-fenster-von-hand-abnehmen",
    title = "takt: a file that cannot play is named in the bar, and the next entry plays",
    engine = "desktop",
    app = "takt-broken-then-file",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 30000, note = "the window is there and plays" },
        {
            wait = { name = "Subtitles" },
            timeout = 10000,
            note = "a file plays: the second entry (only a file has the menu)",
        },
        { expect = { contains = "cannot play broken.mkv" }, note = "the bar names the first entry and why" },
        { click = "Pause", note = "pause, for a picture that stands" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { shot = "cannot-play" },
    },
    judge = {
        -- The line of the bar is checked by the step above: the picture of the driver ends above the bar.
        "in the picture cannot-play: the pattern of the film fills the window (the second entry plays)",
    },
}

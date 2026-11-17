-- SPDX-License-Identifier: GPL-3.0-or-later
-- The window playing the test clip through the own pipeline, and the switch to libmpv and back while it plays:
--   testing sheets step own-pipeline-window
-- What it replaces of a hand run: the picture of takt-demux and takt-decode (two helper processes, frames out of the
-- frame pool as NV12) reaches the real window through the layer of takt-gpu, the bar says which pipeline plays, and its
-- button swaps the two backends behind `VideoBackend` in the run (F6 does the same; a desktop sheet presses no keys).
-- With libmpv the bar has the menus of the tracks, with the own pipeline it has none: that is how the sheet tells them
-- apart by name. What stays for a person: whether the two pictures look the same (TESTS of compare.rs measure it),
-- and how 4K runs. Needs sheet-driver of appkit and a build with libmpv and FFmpeg (TAKT_LIBMPV_DIR, FFMPEG_DIR,
-- scripts/build-sheet-app.py own); silent. Not part of the CI.
return {
    id = "takt.own-pipeline-window",
    feature = "takt.window-own-pipeline",
    task = "takt/eigene-pipeline-im-fenster-von-hand-abnehmen",
    title = "takt: a file plays through the own pipeline, and the bar switches to libmpv and back",
    engine = "desktop",
    app = "takt-own-pipeline",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 30000, note = "the window is there and plays" },
        { wait = { name = "Pipeline: own" }, timeout = 20000, note = "the file plays through the own pipeline" },
        {
            wait = { name = "own pipeline: picture only, no sound and no subtitles yet" },
            timeout = 20000,
            note = "the file is open, and the bar says what the pipeline does not do yet",
        },
        { click = "Pause", note = "pause: the picture of the own pipeline stays" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { shot = "own" },
        { procs = "clean", note = "the two helpers are no shells and open no console window" },
        { click = "Pipeline: own", note = "switch to libmpv: the file is opened again by it" },
        { wait = { name = "Pipeline: libmpv" }, timeout = 20000, note = "the bar names libmpv" },
        {
            wait = { name = "Subtitles" },
            timeout = 20000,
            note = "libmpv has the file open: the menus of the tracks are there",
        },
        { expect = { name = "Play" }, note = "still paused: the pause went along" },
        { shot = "mpv" },
        { click = "Pipeline: libmpv", note = "and back to the own pipeline" },
        { wait = { name = "Pipeline: own" }, timeout = 20000, note = "the bar names the own pipeline" },
        {
            wait = { name = "own pipeline: picture only, no sound and no subtitles yet" },
            timeout = 20000,
            note = "the own pipeline has the file open again",
        },
        { click = "Play", note = "play on" },
        { expect = { name = "Pause" }, note = "the button says Pause" },
        { shot = "own-again" },
    },
    judge = {
        -- The picture itself may stay black in a shot (a wgpu surface the capture does not see); the overlay is text.
        "in the picture own: the test pattern, and the overlay names the source takt-sw (h264), 160x90 Nv12, pool empty 0x",
        "in the picture mpv: the same pattern in the same colours, and the overlay names the source mpv-sw, 160x90 rgb0",
        "in the picture own-again: the overlay names takt-sw (h264) again, and the picture is the pattern",
    },
}

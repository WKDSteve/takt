-- SPDX-License-Identifier: GPL-3.0-or-later
-- A film.srt beside film.mkv: the subtitles are a track of the film without anything being done.
--   testing sheets step sidecar-subtitles-window
-- What it replaces of the hand run of the MpvBackend: "a film.srt beside film.mkv: the subtitles are in the picture
-- without anything being done", and the menu "Audio" of the bar. Needs sheet-driver of appkit, a build with libmpv
-- and ffmpeg; silent. Not part of the CI.
return {
    id = "takt.sidecar-subtitles-window",
    feature = "takt.window-file",
    task = "takt/mpv-backend-im-fenster-von-hand-abnehmen",
    title = "takt: the subtitle file beside a film is its third subtitle track, and the sound track is in its menu",
    engine = "desktop",
    app = "takt-film-with-sidecar",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 30000, note = "the window is there and the film plays" },
        { wait = { name = "Subtitles" }, timeout = 10000, note = "the bar has the menus of a file" },
        { click = "Pause", note = "pause" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { click = "Audio", note = "open the list of the sound tracks" },
        { wait = { name = "1" }, note = "the one sound track of the clip (no language, no title)" },
        { shot = "audio" },
        { click = "1", note = "choose it: the list closes" },
        { click = "Subtitles", note = "open the list of the subtitle tracks" },
        { wait = { name = "3 (srt)" }, note = "the two tracks of the film and the file beside it" },
        { expect = { name = "1 (deu)" } },
        { expect = { name = "2 (eng)" } },
        { shot = "subtitles" },
    },
    judge = {
        "in the picture audio: the list of the sound tracks open (off, 1) with 1 marked",
        "in the picture subtitles: the list of the subtitle tracks open (off, 1 (deu), 2 (eng), 3 (srt)) with 3 (srt) marked, and the line beside the film in the picture",
    },
}

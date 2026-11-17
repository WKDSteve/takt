-- SPDX-License-Identifier: GPL-3.0-or-later
-- .testing.lua -- what the test sheets of this repository run against (testing.nvim, docs/SHEETS.md there). The sheets
-- in TESTS/sheets only name an app; how it starts is here, under testing.nvim's trust gate. takt has no Lua spec:
--   testing sheets run --all            (or: testing sheets step <id>)
-- Desktop sheets need sheet-driver of appkit ($TESTING_DESKTOP_DRIVER or on the PATH), else they are a skip. They run on
-- the private machine only (a window and a build), never on the workstation and not in the CI.

return {
    sheets = {
        apps = {
            -- The window with the test picture: the build the CI makes (no libmpv, whatever the shell sets; built by
            -- scripts/build-sheet-app.py into target/sheets-plain), the sound simulated, the overlay shown.
            ["takt-test-picture"] = {
                build = { "python", "scripts/build-sheet-app.py", "plain" },
                run = { "target/sheets-plain/debug/takt-player.exe", "--overlay" },
                env = { TAKT_AUDIO = "sim" },
                window = "Takt",
            },
            -- The window playing the test clip through libmpv: built by scripts/build-sheet-app.py into target/sheets-mpv
            -- with libmpv-2.dll beside the program (needs TAKT_LIBMPV_DIR, docs/BUILD.md). The clip is made a minute long
            -- into the folder of the run (scripts/sheet-prepare.py, needs ffmpeg): the three seconds of the file in the
            -- repository are over before a window that starts on a busy machine is paused. Silent: mpv plays on the null
            -- device (TAKT_AUDIO=sim makes the session silent).
            ["takt-file"] = {
                build = { "python", "scripts/build-sheet-app.py", "mpv" },
                prepare = { "python", "scripts/sheet-prepare.py", "film", "${temp}" },
                run = { "target/sheets-mpv/debug/takt-player.exe", "--overlay", "${temp}/film.mkv" },
                env = { TAKT_AUDIO = "sim" },
                window = "Takt",
            },
            -- The same build, started with a text that only has the ending of a film and then the clip: what a drop of the
            -- two did in the hand run.
            ["takt-broken-then-file"] = {
                build = { "python", "scripts/build-sheet-app.py", "mpv" },
                prepare = { "python", "scripts/sheet-prepare.py", "broken", "${temp}" },
                run = { "target/sheets-mpv/debug/takt-player.exe", "${temp}/broken.mkv", "${temp}/film.mkv" },
                env = { TAKT_AUDIO = "sim" },
                window = "Takt",
            },
            -- The same build with the clip and a film.srt beside it.
            ["takt-film-with-sidecar"] = {
                build = { "python", "scripts/build-sheet-app.py", "mpv" },
                prepare = { "python", "scripts/sheet-prepare.py", "sidecar", "${temp}" },
                run = { "target/sheets-mpv/debug/takt-player.exe", "${temp}/film.mkv" },
                env = { TAKT_AUDIO = "sim" },
                window = "Takt",
            },
        },
    },
}

-- SPDX-License-Identifier: GPL-3.0-or-later
-- The counters of an mpv run as one line, for the A/B protocol of docs/reviews/phase-0.md.
--
--   mpv --no-config --script=scripts/ab-mpv-stats.lua --video-sync=display-resample ...
--
-- scripts/ab-measure.py --mpv starts mpv this way and reads the line `takt-ab backend=mpv ...` that this script
-- prints when the player shuts down. The properties are gone once the file is closed, so they are read once a second
-- and the last reading is printed.

local mp = require("mp")

-- name of the key in the line -> property of mpv
local PROPERTIES = {
    { "mode", "video-sync" },
    { "fps", "container-fps" },
    { "hz", "display-fps" },
    { "seconds", "time-pos" },
    { "dropped", "frame-drop-count" },
    { "decoder_dropped", "decoder-frame-drop-count" },
    { "delayed", "vo-delayed-frame-count" },
    { "mistimed", "mistimed-frame-count" },
    { "jitter", "vsync-jitter" },
    { "vsync_ratio", "vsync-ratio" },
    { "speed", "audio-speed-correction" },
}

local last = {}

local function sample()
    for _, pair in ipairs(PROPERTIES) do
        local value = mp.get_property(pair[2])
        if value ~= nil then
            last[pair[1]] = value
        end
    end
end

mp.add_periodic_timer(1, sample)
mp.register_event("end-file", sample)

mp.register_event("shutdown", function()
    local parts = { "takt-ab backend=mpv" }
    for _, pair in ipairs(PROPERTIES) do
        -- A property mpv never gave is `-`; spaces would break the key=value line.
        local value = (last[pair[1]] or "-"):gsub("%s+", "_")
        parts[#parts + 1] = pair[1] .. "=" .. value
    end
    print(table.concat(parts, " "))
end)

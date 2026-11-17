-- SPDX-License-Identifier: GPL-3.0-or-later
-- The window with the test picture drawn through the import path of takt-gpu (`takt-player --imported`):
--   testing sheets step imported-picture-window
-- A D3D11 device on the adapter of the window writes every picture into a shared texture, and the video surface draws
-- that texture as a hardware decoder's picture will be drawn: imported through wgpu-hal, with a fence and a guard. The
-- player draws the first picture both ways into a target of its own and says whether the bytes are the same. What this
-- adds to the GPU test of takt-gpu (the same comparison without a window): the path works on the device eframe made,
-- inside the pass of egui, with a swapchain presenting. Needs sheet-driver of appkit, else it is a skip; Windows on
-- DX12; not part of the CI.
return {
    id = "takt.imported-picture-window",
    feature = "takt.window-imported-picture",
    task = "takt/takt-gpu-compositing-farbraum-tonemapping",
    title = "takt: the test picture through an imported texture, compared with the upload",
    engine = "desktop",
    app = "takt-imported-picture",
    sizes = { { cols = 1280, rows = 800 } },
    steps = {
        { wait = { name = "Pause" }, timeout = 20000, note = "the window is there and plays" },
        {
            wait = { name = "Imported picture: same bytes as the upload" },
            timeout = 20000,
            note = "the first picture was drawn both ways and compared",
        },
        { click = "Pause", note = "pause: the picture shown stays an imported one" },
        { expect = { name = "Play" }, note = "the button says Play" },
        { click = "Play", note = "play again" },
        { expect = { name = "Pause" }, note = "the button says Pause" },
        { expect = { name = "Imported picture: same bytes as the upload" }, note = "the verdict stands" },
        { shot = "imported" },
    },
    judge = {
        -- The picture itself may stay black in the shot (a wgpu surface the capture does not see).
        "in the picture imported: the overlay has a line picture path saying imported texture (D3D11 shared, NV12), same bytes as the upload, with a count of imported pictures that is not 0 and 0 uploaded",
    },
}

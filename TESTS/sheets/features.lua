-- SPDX-License-Identifier: GPL-3.0-or-later
-- TESTS/sheets/features.lua -- the functionalities of takt as its user meets them in the window; `testing sheets report`
-- says which ones have a feature sheet (`feature = "<id>"`) and in which state. Data, read like a sheet (testing.nvim
-- docs/SHEETS.md). The rest of takt is checked by `cargo test`; these are what only the real window shows.
return {
  {
    id = "takt.window-controls",
    title = "The window plays and pauses, mutes and shows the diagnostic overlay (F3)",
    docs = "docs/KEYS.md",
  },
  {
    id = "takt.window-file",
    title = "A file plays in the window through libmpv, with its audio and subtitle tracks in the menus of the bar",
    docs = "docs/PHASE-0.md",
  },
}

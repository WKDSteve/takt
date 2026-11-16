# Programs takt starts

**None.** `takt-player` plays through libmpv inside its own process (`apps/takt-player/src/mpv`),
draws with the GPU in its own window and starts no program: the code has no
`std::process::Command`, `CreateProcess` or `ShellExecute`. **A process started by takt is a
bug.** Checked against a measurement with `app-proc-watch`.

The release build is a window program (GUI subsystem): a double click and the Start menu link
open no console window. Started from a terminal it attaches to the terminal's console
(`AttachConsole`), so its output and errors appear there; the shell does not wait for it.

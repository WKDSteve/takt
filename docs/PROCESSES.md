# Programs takt starts

**`takt-player`: none.** It plays through libmpv inside its own process (`apps/takt-player/src/mpv`),
draws with the GPU in its own window and starts no program: the code has no
`std::process::Command`, `CreateProcess` or `ShellExecute`. **A process started by the player is a
bug**, until the own pipeline is wired in (below). Checked against a measurement with `app-proc-watch`.

The release build is a window program (GUI subsystem): a double click and the Start menu link
open no console window. Started from a terminal it attaches to the terminal's console
(`AttachConsole`), so its output and errors appear there; the shell does not wait for it.

## Helper processes of the own pipeline

From phase 1 on Takt puts what it does not trust or may not link behind a process boundary. One
place in the code starts such a process: `takt_ipc::process::HelperCommand` (`crates/takt-ipc`).
Everything it starts lives under the same rules (RS-04 and ENG-03 of the native rule set, applied
to a process that lives as long as its work):

| Rule | How |
|---|---|
| A watchdog | The helper raises a counter in the shared memory while it works and while it waits. A thread of the parent ends a helper whose counter stands still for 5 s; waiting for an answer has its own deadline, and missing it ends the helper too. |
| Byte limits | A frame on the control pipe above its limit, a record in the ring above its limit, more than 1 MiB on stderr or more than 1024 frames nobody fetched end the helper. Its memory is bounded under Windows (job object) and Linux (`RLIMIT_AS`); macOS enforces neither. |
| An end on every path | Dropping the handle ends and reaps the process. A helper whose parent dies sees its stdin close and exits at once (a thread does nothing but watch for it, so this holds for a helper that hangs too); under Windows a job object ends it as well. Under Windows a helper cannot start processes of its own. |
| No name to open | Data travels through anonymous shared memory with a wait-free ring, control through the helper's stdin and stdout with a versioned protocol; files reach a helper as inherited handles. Nothing has a name in any namespace. |
| No trust | The parent checks every length, counter, offset and timestamp a helper sends, as it would bytes from a file, and ends a helper that sends something impossible. |

No console window opens for a helper (`CREATE_NO_WINDOW`).

| Program | Started by | What it does | Limits beyond the defaults |
|---|---|---|---|
| `takt-demux-helper` | `takt_demux::Demuxer`, one per open file | Parses one container file with libavformat (FFmpeg 9.0, LGPL, dynamically linked) and writes its packets into the ring. It gets the open file as a handle and no path, may use a fixed list of demuxers (Matroska, WebM, the MP4 family) and opens nothing else. | 2 GiB of memory, packets up to 8 MiB, a ring of 16 MiB, 30 s to open a file, 512 MiB of reads without a packet |
| `takt-ipc-probe` | the tests of `takt-ipc` only | A helper that hangs, floods, lies or dies on demand. Not part of any product. | set by each test |

`takt-demux-helper` is not started by `takt-player` yet: the demuxer is a library until the own
pipeline (`takt-decode`, `takt-gpu`) plays through it. The helper is not confined by the operating
system beyond the rules above (no restricted token or AppContainer under Windows, no seccomp or
Landlock under Linux): the open point of `docs/reviews/phase-1.md`.

`takt-demux-helper --about` prints the FFmpeg it runs with. Started by hand without that argument it
says that it is started by Takt and exits with code 2.

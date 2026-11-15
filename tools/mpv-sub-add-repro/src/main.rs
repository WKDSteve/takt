// SPDX-License-Identifier: GPL-3.0-or-later
//! Reproduces, without Takt, the hang of mpv seen with asynchronous `sub-add` (takt task
//! `mpv-sub-add-haenger-ohne-takt-nachstellen`): only the client API of libmpv, and for `vo=libmpv` its render
//! API with the software renderer in a thread of its own.
//!
//!     mpv-sub-add-repro loop <runs> <vo> <all|abort|bad|load> <clip>
//!     mpv-sub-add-repro run <vo> <all|abort|bad|load> <clip>
//!
//! `<vo>` is `null` or `libmpv`, with `+hook` (`null+hook`, `libmpv+hook`) also the hook `on_load` Takt sets for
//! files, answered as Takt answers it. Build: `TAKT_LIBMPV_DIR=<folder with mpv.lib> cargo build --release` on
//! Windows (the DLL on PATH to run it), plain `cargo build --release` with libmpv-dev elsewhere.
//!
//! `loop` starts `run` as a child process `runs` times and kills a child that does not finish within 15 s: that
//! run counts as a hang. A run plays `clip`, then:
//!
//! - `all`: three `sub-add` at once (a subtitle file, a file of EDL content that is no subtitle file, a subtitle
//!   file), waits for their replies, eight more, and `loadfile` at once (the sequence of the Takt test);
//! - `abort`: three good `sub-add` and `mpv_abort_async_command` on each right after;
//! - `bad`: three `sub-add` of the EDL file, one after another;
//! - `load`: eight good `sub-add` and `loadfile` at once.
//!
//! It then waits for every reply (10 s) and asks the core a question (`time-pos`, synchronously): a core that
//! hangs never answers, and the parent kills the run.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::process::{Command, ExitCode};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[repr(C)]
struct Event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

type UpdateCallback = unsafe extern "C" fn(*mut c_void);

unsafe extern "C" {
    fn mpv_create() -> *mut c_void;
    fn mpv_initialize(ctx: *mut c_void) -> c_int;
    fn mpv_set_option_string(ctx: *mut c_void, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command(ctx: *mut c_void, args: *mut *const c_char) -> c_int;
    fn mpv_command_async(ctx: *mut c_void, reply: u64, args: *mut *const c_char) -> c_int;
    fn mpv_abort_async_command(ctx: *mut c_void, reply: u64);
    fn mpv_wait_event(ctx: *mut c_void, timeout: f64) -> *mut Event;
    fn mpv_get_property_string(ctx: *mut c_void, name: *const c_char) -> *mut c_char;
    fn mpv_free(data: *mut c_void);
    fn mpv_terminate_destroy(ctx: *mut c_void);
    fn mpv_hook_add(ctx: *mut c_void, reply: u64, name: *const c_char, priority: c_int) -> c_int;
    fn mpv_hook_continue(ctx: *mut c_void, id: u64) -> c_int;
    fn mpv_render_context_create(
        res: *mut *mut c_void,
        mpv: *mut c_void,
        params: *mut RenderParam,
    ) -> c_int;
    fn mpv_render_context_set_update_callback(
        ctx: *mut c_void,
        cb: UpdateCallback,
        data: *mut c_void,
    );
    fn mpv_render_context_update(ctx: *mut c_void) -> u64;
    fn mpv_render_context_render(ctx: *mut c_void, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_free(ctx: *mut c_void);
}

const EVENT_COMMAND_REPLY: c_int = 5;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_HOOK: c_int = 25;

#[repr(C)]
struct HookEvent {
    name: *const c_char,
    id: u64,
}

/// A hook event as Takt answers it: read the source mpv is about to open, then let it go on.
fn answer_hook(core: &Core, event: &Event) {
    if event.event_id != EVENT_HOOK || event.data.is_null() {
        return;
    }
    // SAFETY: the data of a HOOK event is an mpv_event_hook (client.h).
    let id = unsafe { (*event.data.cast::<HookEvent>()).id };
    // SAFETY: the core lives; the string is freed with mpv_free.
    let name = unsafe { mpv_get_property_string(core.0, c"stream-open-filename".as_ptr()) };
    if !name.is_null() {
        unsafe { mpv_free(name.cast()) };
    }
    // SAFETY: the id of the hook event just read.
    unsafe { mpv_hook_continue(core.0, id) };
}
const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const PARAM_SW_SIZE: c_int = 17;
const PARAM_SW_FORMAT: c_int = 18;
const PARAM_SW_STRIDE: c_int = 19;
const PARAM_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;

struct Core(*mut c_void);
// SAFETY: the client API of libmpv is thread-safe (client.h).
unsafe impl Send for Core {}
unsafe impl Sync for Core {}

fn command(core: &Core, args: &[&str]) -> c_int {
    let owned: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
    let mut raw: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
    raw.push(ptr::null());
    // SAFETY: a NULL-terminated array of NUL-terminated strings that live until the call returns.
    unsafe { mpv_command(core.0, raw.as_mut_ptr()) }
}

fn command_async(core: &Core, reply: u64, args: &[&str]) -> c_int {
    let owned: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
    let mut raw: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
    raw.push(ptr::null());
    // SAFETY: as in `command`; mpv copies the arguments before it returns.
    unsafe { mpv_command_async(core.0, reply, raw.as_mut_ptr()) }
}

/// Waits for an event of `id`, at most `limit`; answers hook events meanwhile.
fn wait_for(core: &Core, id: c_int, limit: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        // SAFETY: the core lives; the event is read before the next call of mpv_wait_event.
        let event = unsafe { &*mpv_wait_event(core.0, 0.05) };
        answer_hook(core, event);
        if event.event_id == id {
            return true;
        }
    }
    false
}

/// Waits for `left` command replies, at most `limit`; answers hook events meanwhile. Returns the replies missing.
fn wait_replies(core: &Core, mut left: usize, limit: Duration) -> usize {
    let started = Instant::now();
    while left > 0 && started.elapsed() < limit {
        // SAFETY: as in `wait_for`.
        let event = unsafe { &*mpv_wait_event(core.0, 0.05) };
        answer_hook(core, event);
        if event.event_id == EVENT_COMMAND_REPLY {
            left -= 1;
        }
    }
    left
}

struct Wake {
    pending: Mutex<bool>,
    changed: Condvar,
}

unsafe extern "C" fn on_update(data: *mut c_void) {
    // SAFETY: `data` is the Wake the render thread keeps alive until the context is freed.
    let wake = unsafe { &*(data as *const Wake) };
    *wake.pending.lock().unwrap() = true;
    wake.changed.notify_one();
}

/// The render thread of `vo=libmpv`: creates the software render context, renders every frame mpv has ready into
/// a buffer of 160x90, until `stop`.
fn render_thread(core: Arc<Core>, stop: Arc<AtomicBool>, ready: Arc<(Mutex<bool>, Condvar)>) {
    let mut api = *b"sw\0";
    let mut params = [
        RenderParam {
            kind: PARAM_API_TYPE,
            data: api.as_mut_ptr().cast(),
        },
        RenderParam {
            kind: PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    let mut ctx = ptr::null_mut();
    // SAFETY: the core lives; the params end with INVALID.
    let code = unsafe { mpv_render_context_create(&mut ctx, core.0, params.as_mut_ptr()) };
    {
        let (lock, cond) = &*ready;
        *lock.lock().unwrap() = true;
        cond.notify_all();
    }
    if code < 0 {
        eprintln!("render context: {code}");
        return;
    }
    let wake = Box::new(Wake {
        pending: Mutex::new(false),
        changed: Condvar::new(),
    });
    let wake_ptr: *const Wake = &*wake;
    // SAFETY: the context lives; `wake` outlives it (freed after mpv_render_context_free).
    unsafe { mpv_render_context_set_update_callback(ctx, on_update, wake_ptr.cast_mut().cast()) };
    let (w, h) = (160_i32, 90_i32);
    let mut pixels = vec![0_u8; (w * h * 4) as usize];
    while !stop.load(Ordering::Acquire) {
        {
            let pending = wake.pending.lock().unwrap();
            let (mut pending, _) = wake
                .changed
                .wait_timeout_while(pending, Duration::from_millis(250), |p| !*p)
                .unwrap();
            *pending = false;
        }
        // SAFETY: the context lives.
        while unsafe { mpv_render_context_update(ctx) } & UPDATE_FRAME != 0 {
            let mut size = [w, h];
            let mut stride: usize = (w * 4) as usize;
            let mut no_wait: c_int = 0;
            let mut params = [
                RenderParam {
                    kind: PARAM_SW_SIZE,
                    data: size.as_mut_ptr().cast(),
                },
                RenderParam {
                    kind: PARAM_SW_FORMAT,
                    data: c"rgb0".as_ptr().cast_mut().cast(),
                },
                RenderParam {
                    kind: PARAM_SW_STRIDE,
                    data: (&raw mut stride).cast(),
                },
                RenderParam {
                    kind: PARAM_SW_POINTER,
                    data: pixels.as_mut_ptr().cast(),
                },
                RenderParam {
                    kind: PARAM_BLOCK_FOR_TARGET_TIME,
                    data: (&raw mut no_wait).cast(),
                },
                RenderParam {
                    kind: PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            // SAFETY: every parameter points at a value of the type render.h names; the buffer holds h * stride.
            unsafe { mpv_render_context_render(ctx, params.as_mut_ptr()) };
            // A question to the core right after the render, as Takt asks for the position of the frame.
            // SAFETY: the core lives; the string is freed with mpv_free.
            let pos = unsafe { mpv_get_property_string(core.0, c"time-pos".as_ptr()) };
            if !pos.is_null() {
                unsafe { mpv_free(pos.cast()) };
            }
        }
    }
    // SAFETY: the context is freed on the thread that used it, before the core goes.
    unsafe { mpv_render_context_free(ctx) };
    drop(wake);
}

fn run(vo: &str, trigger: &str, clip: &str) -> ExitCode {
    // `null+hook` / `libmpv+hook`: also the hook `on_load` Takt sets for files, answered as Takt does.
    let (vo, hook) = match vo.split_once('+') {
        Some((vo, "hook")) => (vo, true),
        _ => (vo, false),
    };
    let dir = std::env::temp_dir().join(format!("mpv-sub-add-repro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.srt");
    std::fs::write(&good, "1\n00:00:00,000 --> 00:00:03,000\nREPRO\n").unwrap();
    let bad = dir.join("bad.srt");
    // EDL content under a subtitle name: mpv fails to read it as subtitles.
    std::fs::write(&bad, format!("# mpv EDL v0\n%{}%{}\n", clip.len(), clip)).unwrap();
    let (good, bad) = (
        good.to_string_lossy().into_owned(),
        bad.to_string_lossy().into_owned(),
    );

    // SAFETY: plain creation; checked for null.
    let raw = unsafe { mpv_create() };
    assert!(!raw.is_null());
    let core = Arc::new(Core(raw));
    for (name, value) in [
        ("config", "no"),
        ("load-scripts", "no"),
        ("vo", vo),
        ("ao", "null"),
        ("hwdec", "no"),
        ("idle", "yes"),
        ("keep-open", "no"),
        ("access-references", "no"),
    ] {
        let (name, value) = (CString::new(name).unwrap(), CString::new(value).unwrap());
        // SAFETY: before initialize; NUL-terminated strings.
        unsafe { mpv_set_option_string(core.0, name.as_ptr(), value.as_ptr()) };
    }
    // SAFETY: the core was created and not yet initialized.
    assert!(unsafe { mpv_initialize(core.0) } >= 0);
    if hook {
        // SAFETY: the core lives; a NUL-terminated name.
        assert!(unsafe { mpv_hook_add(core.0, 0, c"on_load".as_ptr(), 0) } >= 0);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let renderer = (vo == "libmpv").then(|| {
        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let thread = {
            let (core, stop, ready) = (Arc::clone(&core), Arc::clone(&stop), Arc::clone(&ready));
            std::thread::spawn(move || render_thread(core, stop, ready))
        };
        let (lock, cond) = &*ready;
        drop(cond.wait_while(lock.lock().unwrap(), |r| !*r).unwrap());
        thread
    });

    command(&core, &["loadfile", clip]);
    assert!(
        wait_for(&core, EVENT_FILE_LOADED, Duration::from_secs(10)),
        "the clip did not load"
    );
    let _ = wait_for(&core, EVENT_PLAYBACK_RESTART, Duration::from_secs(2));

    let mut next = 1_u64;
    let mut add = |core: &Core, path: &str| {
        let id = next;
        next += 1;
        assert!(command_async(core, id, &["sub-add", path, "select"]) >= 0);
        id
    };
    let asked = match trigger {
        "all" => {
            add(&core, &good);
            add(&core, &bad);
            add(&core, &good);
            let left = wait_replies(&core, 3, Duration::from_secs(10));
            if left > 0 {
                println!("HANG first three: {left} replies missing");
                return ExitCode::from(3);
            }
            for _ in 0..8 {
                add(&core, &good);
            }
            command(&core, &["loadfile", clip]);
            8
        }
        "abort" => {
            for _ in 0..3 {
                let id = add(&core, &good);
                // SAFETY: the core lives; aborting an id that ended already is allowed.
                unsafe { mpv_abort_async_command(core.0, id) };
            }
            3
        }
        "bad" => {
            for _ in 0..3 {
                add(&core, &bad);
            }
            3
        }
        "load" => {
            for _ in 0..8 {
                add(&core, &good);
            }
            command(&core, &["loadfile", clip]);
            8
        }
        other => panic!("unknown trigger {other}"),
    };
    let left = wait_replies(&core, asked, Duration::from_secs(10));
    if left > 0 {
        println!("HANG {left} of {asked} replies missing");
        return ExitCode::from(3);
    }
    // The core answers (a hanging core blocks here; the parent kills the run).
    // SAFETY: the core lives; the string is freed with mpv_free.
    let pos = unsafe { mpv_get_property_string(core.0, c"playlist-count".as_ptr()) };
    if !pos.is_null() {
        // SAFETY: a string of mpv.
        let _ = unsafe { CStr::from_ptr(pos) };
        unsafe { mpv_free(pos.cast()) };
    }
    stop.store(true, Ordering::Release);
    if let Some(thread) = renderer {
        thread.join().unwrap();
    }
    // SAFETY: the render context is freed; nothing uses the core after this.
    unsafe { mpv_terminate_destroy(core.0) };
    let _ = std::fs::remove_dir_all(&dir);
    println!("OK");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => run(&args[2], &args[3], &args[4]),
        Some("loop") => {
            let runs: u32 = args[2].parse().unwrap();
            let (vo, trigger, clip) = (&args[3], &args[4], &args[5]);
            let exe = std::env::current_exe().unwrap();
            let (mut ok, mut hung, mut failed) = (0, 0, 0);
            for _ in 0..runs {
                let mut child = Command::new(&exe)
                    .args(["run", vo, trigger, clip])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let started = Instant::now();
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break Some(status);
                    }
                    if started.elapsed() > Duration::from_secs(15) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                match status {
                    Some(s) if s.success() => ok += 1,
                    Some(s) if s.code() == Some(3) => hung += 1,
                    None => hung += 1,
                    Some(_) => failed += 1,
                }
            }
            println!("vo={vo} trigger={trigger}: {ok} ok, {hung} hung, {failed} failed of {runs}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: mpv-sub-add-repro loop <runs> <null|libmpv>[+hook] <all|abort|bad|load> <clip>"
            );
            ExitCode::from(2)
        }
    }
}

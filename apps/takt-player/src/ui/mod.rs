// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: egui through eframe, rendered with wgpu. A bar with play and
//! pause, the position and the volume, above it the video surface.
//!
//! The source is the moving test picture of [`NullBackend`] (NV12, BT.709,
//! limited range), drawn through the colour path of [`render`].
//!
//! # The render loop
//!
//! eframe calls [`eframe::App::update`] once per display refresh (vsync, no
//! sleep anywhere). Each call asks the source for the frame that is due on the
//! clock and gives the answer to the [`Scheduler`], exactly as the headless
//! loop does, so the measurements of both can be compared. While paused,
//! nothing is asked and nothing counted. The clock is the audio master clock
//! wrapped in a [`SeekableClock`]: a jump moves the media position, resets
//! source and scheduler (their numbers are about the old position), and the
//! device plays on.
//!
//! F3 shows the diagnostic overlay ([`crate::diagnostics`]): presentation
//! counters with the histogram of the errors, anchor age, pool, audio
//! counters, CPU and memory, and what the overlay itself costs per pass. F4
//! copies the same as text to the clipboard and saves it in the temporary
//! directory (`takt-diagnostics-<unix seconds>.txt`), for gate reports.
//!
//! Files and folders dropped on the window become the playlist (Shift
//! appends); `>` and `<` go to the next and the entry before, the end of an
//! entry goes on to the next, F8 shows the playlist (select, remove,
//! shuffle) and `?` the keys of [`crate::app::BINDINGS`]. Until the
//! MpvBackend comes, an entry plays as the test picture of the NullBackend:
//! the list, the keys and the drop work, the file itself is not read.
//!
//! The source learns the display period ([`RefreshMeter`], the median of the
//! passes), so a frame is due at the refresh nearest its timestamp.

use std::path::Path;
use std::sync::Arc;

use std::time::{Duration, Instant};

use takt_backend::{NullBackend, Pattern, Presentation, Scheduler, SeekMode, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, SeekableClock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

use crate::app::{BINDINGS, Command, Key, MOUSE, PlayerState, command_for, seek_target};
use crate::audio::{self, AudioPath};
use crate::diagnostics::{self, AudioCounters, ProcessMeter, Snapshot};
use crate::playlist::{self, Playlist};
use crate::refresh::RefreshMeter;
use crate::render::{self, Shown, gpu::Nv12Image};

/// Length of the test source.
const SOURCE_MILLIS: i64 = 3_600_000;
/// Frames of the pool of the test source.
const POOL_FRAMES: usize = 6;
/// What the overlay shows: when it was read, its lines and the error classes.
type ShownDiagnostics = (Instant, Vec<(&'static str, String)>, [u32; 16]);

/// How often the overlay reads the counters again.
const OVERLAY_INTERVAL: Duration = Duration::from_millis(250);

/// The test source: the moving block of `NullBackend`, 1920x1080 NV12 at the
/// NTSC rate, one hour long.
fn test_source() -> NullBackend {
    let meta = FrameMeta {
        width: 1920,
        height: 1080,
        format: PixelFormat::Nv12,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        range: ColorRange::Limited,
        pts: Timestamp::from_millis(0),
    };
    let mut backend = NullBackend::new(
        meta,
        Rational::lit(24_000, 1_001),
        Timestamp::from_millis(SOURCE_MILLIS),
        Pattern::Motion,
        POOL_FRAMES,
    );
    // NullBackend opens any name.
    let _ = backend.open("null://motion");
    backend
}

/// The player in its window.
struct PlayerApp {
    state: PlayerState,
    clock: SeekableClock<AudioMasterClock>,
    scheduler: Scheduler,
    /// `--seconds N`: close after N seconds (a measuring run).
    close_at: Option<Instant>,
    // Kept for its life; dropping it stops the sound.
    _audio: AudioPath,
    audio_line: String,
    source: NullBackend,
    shown: Option<Shown>,
    refresh: RefreshMeter,
    started: Instant,
    process: ProcessMeter,
    /// What building the overlay costs per pass.
    overlay_cost: takt_audio::DurationHistogram,
    /// The line of the last F4, shown in the overlay.
    saved: Option<String>,
    /// What the overlay shows, with the time it was read.
    shown_diagnostics: Option<ShownDiagnostics>,
    /// The target of the last jump, until the first frame after it is shown.
    seek_probe: Option<i64>,
    /// The last jump: its target and the timestamp of the first frame shown
    /// after it, in milliseconds.
    last_seek: Option<(i64, i64)>,
    /// The position the timeline is dragged to, before the jump.
    scrub: Option<i64>,
    playlist: Playlist,
    /// What the last drop could not take (folders without media, errors).
    drop_problems: Vec<String>,
}

impl PlayerApp {
    fn new(cc: &eframe::CreationContext<'_>, seconds: Option<u64>, overlay: bool) -> Self {
        if let Some(state) = cc.wgpu_render_state.as_ref() {
            render::install(state);
        }
        crate::theme::install(&cc.egui_ctx);
        let time = Arc::new(SystemMonotonic::new());
        let (clock, audio) = audio::start(&time);
        let audio_line = audio.describe();
        let mut state = PlayerState::default();
        if overlay {
            state.apply(Command::ToggleOverlay);
        }
        Self {
            state,
            clock: SeekableClock::new(clock),
            scheduler: Scheduler::new(),
            close_at: seconds.map(|s| Instant::now() + Duration::from_secs(s)),
            _audio: audio,
            audio_line,
            source: test_source(),
            shown: None,
            refresh: RefreshMeter::default(),
            started: Instant::now(),
            process: ProcessMeter::start(),
            overlay_cost: takt_audio::DurationHistogram::default(),
            saved: None,
            shown_diagnostics: None,
            seek_probe: None,
            last_seek: None,
            scrub: None,
            playlist: Playlist::default(),
            drop_problems: Vec::new(),
        }
    }

    /// One pass of the render loop: the frame that is due now, judged by the
    /// scheduler; a new picture when it is shown.
    fn take_due_frame(&mut self) {
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.refresh.pass(elapsed);
        self.source.set_display_period(self.refresh.period_nanos());
        if !self.state.playing() {
            return;
        }
        let frame = match self.source.next_frame(&self.clock) {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("takt-player: source: {e}");
                return;
            }
        };
        let eof = self.source.is_eof();
        if eof && frame.is_none() {
            // The end of an entry: on to the next one, if there is one.
            if let Some(next) = self.playlist.next().map(Path::to_path_buf) {
                self.play(&next);
                return;
            }
        }
        if !matches!(
            self.scheduler.accept(&self.clock, frame.as_ref(), eof),
            Presentation::Shown { .. }
        ) {
            return;
        }
        if let Some(frame) = frame {
            if let Some(target) = self.seek_probe.take() {
                let landed = frame
                    .get()
                    .meta()
                    .pts
                    .rescale(Rational::MILLIS)
                    .map_or(target, Timestamp::ticks);
                self.last_seek = Some((target, landed));
            }
            match Nv12Image::from_frame(frame.get()) {
                Ok(image) => {
                    let generation = self.shown.as_ref().map_or(0, |s| s.generation + 1);
                    self.shown = Some(Shown {
                        image: Arc::new(image),
                        generation,
                    });
                }
                Err(e) => eprintln!("takt-player: frame not shown: {e}"),
            }
        }
    }

    /// Opens `path` and plays it from the start.
    fn play(&mut self, path: &Path) {
        let name = path.to_string_lossy();
        if let Err(e) = self.source.open(&name) {
            eprintln!("takt-player: cannot open {name}: {e}");
            return;
        }
        self.seek_to(0);
        self.shown = None;
    }

    /// Takes the files dropped on the window: they replace the playlist, or
    /// are appended with Shift held.
    fn take_drops(&mut self, ctx: &egui::Context) {
        let (dropped, append) = ctx.input(|i| {
            (
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect::<Vec<_>>(),
                i.modifiers.shift,
            )
        });
        if dropped.is_empty() {
            return;
        }
        let base = std::env::current_dir().unwrap_or_default();
        let (entries, problems) = playlist::expand(&dropped, &base);
        self.drop_problems = problems;
        let start = if append {
            self.playlist.append(entries)
        } else {
            self.playlist.replace(entries)
        }
        .map(Path::to_path_buf);
        if let Some(path) = start {
            self.play(&path);
        }
    }

    /// The playlist window (F8): select, remove, shuffle.
    fn playlist_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut play = None;
        let mut remove = None;
        let mut shuffle = false;
        egui::Window::new("Playlist")
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                if self.playlist.entries().is_empty() {
                    ui.weak("Drop files or folders on the window (Shift appends).");
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (i, entry) in self.playlist.entries().iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("x").on_hover_text("remove").clicked() {
                                    remove = Some(i);
                                }
                                // The name is foreign text: shown, never interpreted.
                                let label = Playlist::label(entry);
                                let current = self.playlist.current() == Some(i);
                                if ui
                                    .selectable_label(current, label)
                                    .on_hover_text(entry.display().to_string())
                                    .clicked()
                                {
                                    play = Some(i);
                                }
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .button("Shuffle")
                        .on_hover_text("the entries after the playing one")
                        .clicked()
                    {
                        shuffle = true;
                    }
                    ui.weak(format!("{} entries", self.playlist.entries().len()));
                });
                for problem in &self.drop_problems {
                    ui.weak(problem);
                }
            });
        if shuffle {
            let seed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(1);
            self.playlist.shuffle(seed);
        }
        if let Some(i) = remove
            && self.playlist.remove(i)
        {
            match self.playlist.current_entry().map(Path::to_path_buf) {
                Some(path) => self.play(&path),
                None => self.shown = None,
            }
        }
        if let Some(path) = play
            .and_then(|i| self.playlist.select(i))
            .map(Path::to_path_buf)
        {
            self.play(&path);
        }
        if !open {
            self.state.apply(Command::TogglePlaylist);
        }
    }

    /// The list of keys (`?`), from the same table as `docs/KEYS.md`.
    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        egui::Window::new("Keys")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("keys")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (_, key, what) in BINDINGS {
                            ui.monospace(key);
                            ui.label(what);
                            ui.end_row();
                        }
                        for (what, effect) in MOUSE {
                            ui.weak(what);
                            ui.label(effect);
                            ui.end_row();
                        }
                    });
            });
        if !open {
            self.state.apply(Command::ToggleHelp);
        }
    }

    /// Jumps to `millis`: clock, source and scheduler.
    fn seek_to(&mut self, millis: i64) {
        let to = Timestamp::from_millis(millis);
        self.clock.seek(to);
        if let Err(e) = self.source.seek(to, SeekMode::Keyframe) {
            eprintln!("takt-player: seek: {e}");
        }
        self.scheduler.reset();
        self.seek_probe = Some(millis);
    }

    fn now_millis(&self) -> i64 {
        self.clock
            .now()
            .rescale(Rational::MILLIS)
            .map_or(0, Timestamp::ticks)
    }

    fn run(&mut self, ctx: &egui::Context, command: Command) {
        if self.state.apply(command) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        match command {
            Command::SeekBy(by) => self.seek_to(seek_target(self.now_millis(), by, SOURCE_MILLIS)),
            Command::SeekStart => self.seek_to(0),
            Command::SeekTo(to) => self.seek_to(to.clamp(0, SOURCE_MILLIS)),
            Command::Next => {
                if let Some(path) = self.playlist.next().map(Path::to_path_buf) {
                    self.play(&path);
                }
            }
            Command::Previous => {
                if let Some(path) = self.playlist.previous().map(Path::to_path_buf) {
                    self.play(&path);
                }
            }
            Command::SaveDiagnostics => self.save_diagnostics(ctx),
            Command::TogglePlay
            | Command::Quit
            | Command::ToggleOverlay
            | Command::VolumeUp
            | Command::VolumeDown
            | Command::ToggleMute
            | Command::TogglePlaylist
            | Command::ToggleHelp => {}
        }
        self.clock.set_paused(!self.state.playing());
        self._audio.set_volume(self.state.effective_volume());
    }

    /// All counters, read now.
    fn snapshot(&mut self) -> Snapshot {
        let clock = self.clock.inner();
        Snapshot {
            scheduler: self.scheduler.stats().clone(),
            dropped: self.source.stats().dropped,
            pool: (self.source.stats().queued, POOL_FRAMES),
            anchor_age_nanos: clock.anchor_age_nanos(),
            max_extrapolation_nanos: clock.max_extrapolation_nanos(),
            display_period_nanos: self.refresh.period_nanos(),
            audio_line: self.audio_line.clone(),
            audio: self._audio.device_stats().map(|s| AudioCounters::read(&s)),
            seek: self.last_seek,
            sample_rate: clock.sample_rate(),
            process: self.process.load(),
            overlay_cost: diagnostics::overlay_cost(&self.overlay_cost.snapshot()),
        }
    }

    /// F4: the diagnostics as text, to the clipboard and into a file.
    fn save_diagnostics(&mut self, ctx: &egui::Context) {
        let text = self.snapshot().report(&format!(
            "takt diagnostics at {}",
            self.clock.now().format_hms()
        ));
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = std::env::temp_dir().join(format!("takt-diagnostics-{secs}.txt"));
        let line = match std::fs::write(&path, &text) {
            Ok(()) => format!("saved: {} (and the clipboard)", path.display()),
            Err(e) => format!("not saved ({e}), in the clipboard"),
        };
        println!("takt-player: diagnostics {line}");
        ctx.copy_text(text);
        self.saved = Some(line);
    }

    /// The overlay over the video, its own cost measured.
    fn overlay(&mut self, ctx: &egui::Context) {
        let start = Instant::now();
        // Read four times a second: numbers that change every pass cannot be
        // read anyway, and unchanged text keeps its layout in egui's cache.
        let stale = self
            .shown_diagnostics
            .as_ref()
            .is_none_or(|(at, _, _)| at.elapsed() >= OVERLAY_INTERVAL);
        if stale {
            let snapshot = self.snapshot();
            self.shown_diagnostics = Some((
                Instant::now(),
                snapshot.lines(),
                snapshot.scheduler.histogram,
            ));
        }
        let Some((_, lines, classes)) = &self.shown_diagnostics else {
            return;
        };
        egui::Area::new(egui::Id::new("diagnostics"))
            .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.strong("Diagnostics (F3 hides, F4 saves)");
                    egui::Grid::new("diagnostics-grid")
                        .num_columns(2)
                        .spacing([12.0, 2.0])
                        .show(ui, |ui| {
                            for (label, value) in lines {
                                ui.weak(*label);
                                ui.monospace(value.as_str());
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.weak("presentation error");
                    histogram(ui, classes);
                    if let Some(saved) = &self.saved {
                        ui.weak(saved);
                    }
                });
            });
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.overlay_cost.record(nanos);
    }

    /// The measurements, as the headless run prints them.
    fn summary(&self) -> String {
        let s = self.scheduler.stats();
        #[expect(clippy::cast_precision_loss, reason = "a display value, not time")]
        let mean = s.mean_abs_error_nanos() as f64 / 1e6;
        #[expect(clippy::cast_precision_loss, reason = "a display value, not time")]
        let refresh = self
            .refresh
            .period_nanos()
            .map_or("-".to_owned(), |p| format!("{:.1} Hz", 1e9 / p as f64));
        format!(
            "shown {}, repeated {} ({:.1} %), dropped {}, mean error {mean:.3} ms, display {refresh}",
            s.shown,
            s.repeated,
            s.repeat_ratio(),
            self.source.stats().dropped
        )
    }
}

/// The keys of this frame, as commands, in the order they came.
fn commands(ctx: &egui::Context) -> Vec<Command> {
    ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } => match key {
                    egui::Key::Space => Some(command_for(Key::Space)),
                    egui::Key::Escape => Some(command_for(Key::Escape)),
                    egui::Key::ArrowLeft => Some(command_for(Key::Left)),
                    egui::Key::ArrowRight => Some(command_for(Key::Right)),
                    egui::Key::Home => Some(command_for(Key::Home)),
                    egui::Key::F3 => Some(command_for(Key::F3)),
                    egui::Key::F4 => Some(command_for(Key::F4)),
                    egui::Key::Num9 => Some(command_for(Key::Nine)),
                    egui::Key::Num0 => Some(command_for(Key::Zero)),
                    egui::Key::M => Some(command_for(Key::M)),
                    egui::Key::F8 => Some(command_for(Key::F8)),
                    _ => None,
                },
                // Characters that sit on different keys per layout come as text.
                egui::Event::Text(text) => match text.as_str() {
                    "?" => Some(command_for(Key::Question)),
                    ">" => Some(command_for(Key::Greater)),
                    "<" => Some(command_for(Key::Less)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    })
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        for command in commands(ctx) {
            self.run(ctx, command);
        }
        if self.close_at.is_some_and(|at| Instant::now() >= at) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.take_drops(ctx);
        self.take_due_frame();
        if self.state.overlay() {
            self.overlay(ctx);
        }
        if self.state.playlist() {
            self.playlist_window(ctx);
        }
        if self.state.help() {
            self.help_window(ctx);
        }
        let uploads = frame.wgpu_render_state().and_then(render::uploads);

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let label = if self.state.playing() {
                    "Pause"
                } else {
                    "Play"
                };
                if ui.button(label).on_hover_text("Space").clicked() {
                    self.run(ctx, Command::TogglePlay);
                }
                // The timeline: the jump happens when the drag ends or on a
                // click, the label follows the drag before that.
                let mut position = self.scrub.unwrap_or_else(|| self.now_millis());
                ui.spacing_mut().slider_width = 320.0;
                let timeline = ui
                    .add(
                        egui::Slider::new(&mut position, 0..=SOURCE_MILLIS)
                            .show_value(false)
                            .trailing_fill(true),
                    )
                    .on_hover_text("click or drag to jump; arrows 5 s, Home to the start");
                if timeline.dragged() {
                    self.scrub = Some(position);
                }
                if timeline.drag_stopped() || (timeline.changed() && !timeline.dragged()) {
                    self.scrub = None;
                    self.run(ctx, Command::SeekTo(position));
                }
                ui.monospace(format!(
                    "{} / {}",
                    Timestamp::from_millis(position).format_hms(),
                    Timestamp::from_millis(SOURCE_MILLIS).format_hms()
                ));
                ui.separator();
                let mute = if self.state.muted() { "Unmute" } else { "Mute" };
                if ui.button(mute).on_hover_text("m").clicked() {
                    self.run(ctx, Command::ToggleMute);
                }
                let mut volume = self.state.volume();
                ui.spacing_mut().slider_width = 100.0;
                // The test source plays silence; the volume goes to the device all the same.
                if ui
                    .add(egui::Slider::new(&mut volume, 0.0..=1.0).text("Volume"))
                    .on_hover_text("9 quieter, 0 louder")
                    .changed()
                {
                    self.state.set_volume(volume);
                    self._audio.set_volume(self.state.effective_volume());
                }
                ui.separator();
                ui.weak(self.summary());
                ui.separator();
                ui.weak(&self.audio_line);
                if let Some(u) = uploads.filter(|u| u.total() > 0) {
                    ui.separator();
                    let p50 = u.quantile_micros(1, 2).unwrap_or(0);
                    ui.weak(format!(
                        "upload p50 <{p50} us, max {} us ({} frames)",
                        u.max_nanos() / 1_000,
                        u.total()
                    ));
                }
            });
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| render::video_surface(ui, self.shown.as_ref()));

        // The position moves on: repaint with the next display refresh.
        ctx.request_repaint();
    }
}

/// The classes of the error histogram as bars, the fullest one wide.
fn histogram(ui: &mut egui::Ui, classes: &[u32; 16]) {
    let peak = classes.iter().copied().max().unwrap_or(0).max(1);
    egui::Grid::new("diagnostics-histogram")
        .num_columns(3)
        .spacing([8.0, 1.0])
        .show(ui, |ui| {
            for (i, &count) in classes.iter().enumerate().filter(|(_, c)| **c > 0) {
                ui.monospace(diagnostics::class_label(i));
                #[expect(clippy::cast_precision_loss, reason = "a bar length, not time")]
                let width = 160.0 * count as f32 / peak as f32;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(160.0, 10.0), egui::Sense::hover());
                let bar = egui::Rect::from_min_size(rect.min, egui::vec2(width.max(1.0), 10.0));
                ui.painter()
                    .rect_filled(bar, 0.0, ui.visuals().selection.bg_fill);
                ui.monospace(count.to_string());
                ui.end_row();
            }
        });
}

impl Drop for PlayerApp {
    /// The measurements of the run, when the window closes.
    fn drop(&mut self) {
        println!("takt-player: {}", self.summary());
        let cost = self.overlay_cost.snapshot();
        if let Some((p50, max)) = diagnostics::overlay_cost(&cost) {
            println!(
                "takt-player: overlay p50 <{p50} us, p99 <{} us, max {} us over {} passes",
                cost.quantile_micros(99, 100).unwrap_or(0),
                max / 1_000,
                cost.total()
            );
        }
    }
}

/// Opens the window and runs until it closes, or for `seconds`.
///
/// # Errors
///
/// When no window or no wgpu device could be created.
pub fn run(seconds: Option<u64>, overlay: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("Takt")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([320.0, 200.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Takt",
        options,
        Box::new(move |cc| Ok(Box::new(PlayerApp::new(cc, seconds, overlay)))),
    )
}

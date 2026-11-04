//! Plays animations and videos in the crop editor's preview.

use std::cell::{Cell, RefCell};
use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::cairo;
use gtk::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::{MAX_FPS, Size};

use super::editor::{Editor, surface_from_bgra};
use crate::media::{Source, SourceKind};
use crate::playback::{self, FrameClock, PauseGate, VideoPreview};
use crate::settings;

thread_local! {
    /// Every player of this (GTK main) thread, so a settings change reaches them all.
    static PLAYERS: RefCell<Vec<Player>> = const { RefCell::new(Vec::new()) };
}

/// Applies the current preview settings to every playing preview, restarting what plays.
pub fn apply_preview_settings() {
    let players: Vec<Player> = PLAYERS.with(|all| all.borrow().clone());
    for player in players {
        player.restart();
    }
}

/// What the player was last asked to play, kept so settings changes can restart it.
#[derive(Clone)]
struct Current {
    path: PathBuf,
    kind: SourceKind,
    size: Size,
    fps_cap: u32,
}

/// What the preview is currently playing.
enum Active {
    Idle,
    Animation {
        surfaces: Vec<cairo::ImageSurface>,
        clock: FrameClock,
        start: Instant,
        shown: usize,
        timer: glib::SourceId,
    },
    Video {
        /// `None` while paused: no ffmpeg runs.
        preview: Option<VideoPreview>,
        gate: PauseGate,
        /// Starting ffmpeg failed; not retried until the next restart.
        failed: bool,
        path: PathBuf,
        size: Size,
        timer: glib::SourceId,
    },
}

/// Drives the editor's frame surface for animations and videos. Cloning shares the player.
#[derive(Clone)]
pub struct Player {
    editor: Editor,
    window: gtk::Window,
    active: Rc<RefCell<Active>>,
    generation: Rc<Cell<u64>>,
    current: Rc<RefCell<Option<Current>>>,
}

impl Player {
    /// A player that draws into `editor` and pauses while `window` is hidden or suspended.
    pub fn new(editor: Editor, window: &impl IsA<gtk::Window>) -> Self {
        let player = Self {
            editor,
            window: window.clone().upcast(),
            active: Rc::new(RefCell::new(Active::Idle)),
            generation: Rc::new(Cell::new(0)),
            current: Rc::new(RefCell::new(None)),
        };
        PLAYERS.with(|all| all.borrow_mut().push(player.clone()));
        player
    }

    /// Stops the current playback and starts playing `source` (a still plays nothing).
    ///
    /// Animations decode in the background and follow `fps_cap`; videos follow the preview
    /// settings. With playing switched off nothing runs and the editor keeps its still frame.
    pub fn play(&self, path: &Path, source: &Source, fps_cap: u32) {
        *self.current.borrow_mut() = Some(Current {
            path: path.to_path_buf(),
            kind: source.kind,
            size: source.size,
            fps_cap,
        });
        self.restart();
    }

    /// Starts (again) what `play` last asked for, under the current preview settings.
    fn restart(&self) {
        self.halt();
        let Some(current) = self.current.borrow().clone() else {
            return;
        };
        if !settings::preview().play {
            return;
        }
        let generation = self.generation.get();
        let Current {
            path,
            kind,
            size,
            fps_cap,
        } = current;
        match kind {
            SourceKind::Still => {}
            SourceKind::Animation => {
                let player = self.clone();
                glib::spawn_future_local(async move {
                    let decoded =
                        gio::spawn_blocking(move || playback::decode_animation(&path)).await;
                    if player.generation.get() != generation {
                        return;
                    }
                    match decoded {
                        Ok(Ok(animation)) => player.start_animation(animation, fps_cap),
                        Ok(Err(error)) => tracing::warn!("cannot play the animation: {error}"),
                        Err(_) => tracing::warn!("the animation decoder panicked"),
                    }
                });
            }
            SourceKind::Video => self.start_video(path, size),
        }
    }

    /// Changes the animation's frame cap; videos keep their fixed rate.
    pub fn set_fps_cap(&self, fps: u32) {
        let mut active = self.active.borrow_mut();
        if let Active::Animation { timer, .. } = &mut *active {
            let new = self.animation_timer(fps);
            mem::replace(timer, new).remove();
        }
    }

    /// Stops playing and ends any ffmpeg child; the editor keeps the last frame.
    pub fn stop(&self) {
        *self.current.borrow_mut() = None;
        self.halt();
    }

    /// Stops the running playback but remembers what to restart on a settings change.
    fn halt(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        let previous = mem::replace(&mut *self.active.borrow_mut(), Active::Idle);
        match previous {
            Active::Idle => {}
            Active::Animation { timer, .. } | Active::Video { timer, .. } => timer.remove(),
        }
    }

    fn visible(window: &gtk::Window) -> bool {
        window.is_visible() && !window.is_suspended()
    }

    fn start_animation(&self, animation: playback::Animation, fps_cap: u32) {
        let (frames, clock) = animation.into_parts();
        let count = frames.len();
        let surfaces: Vec<cairo::ImageSurface> = frames
            .into_iter()
            .filter_map(|frame| surface_from_bgra(frame.bgra, frame.width, frame.height))
            .collect();
        if surfaces.len() != count {
            return;
        }
        let timer = self.animation_timer(fps_cap);
        *self.active.borrow_mut() = Active::Animation {
            surfaces,
            clock,
            start: Instant::now(),
            shown: 0,
            timer,
        };
    }

    /// A timer that shows the frame visible now, every `1 / fps` seconds.
    fn animation_timer(&self, fps: u32) -> glib::SourceId {
        let interval = Duration::from_secs(1) / fps.clamp(1, MAX_FPS);
        let active = Rc::clone(&self.active);
        let editor = self.editor.clone();
        let window = self.window.clone();
        glib::timeout_add_local(interval, move || {
            if !Self::visible(&window) {
                return glib::ControlFlow::Continue;
            }
            let mut active = active.borrow_mut();
            if let Active::Animation {
                surfaces,
                clock,
                start,
                shown,
                ..
            } = &mut *active
            {
                let index = clock.frame_at(start.elapsed());
                if index != *shown
                    && let Some(surface) = surfaces.get(index)
                {
                    *shown = index;
                    editor.set_frame_surface(surface);
                }
            }
            glib::ControlFlow::Continue
        })
    }

    fn start_video(&self, path: PathBuf, size: Size) {
        let settings = settings::preview();
        let interval = Duration::from_secs(1) / settings.clamped_fps();
        let active = Rc::clone(&self.active);
        let editor = self.editor.clone();
        let window = self.window.clone();
        let timer = glib::timeout_add_local(interval, move || {
            let settings = settings::preview();
            let mut active = active.borrow_mut();
            let Active::Video {
                preview,
                gate,
                failed,
                path,
                size,
                ..
            } = &mut *active
            else {
                return glib::ControlFlow::Continue;
            };
            let play = gate.should_play(
                Instant::now(),
                Self::visible(&window),
                window.is_active(),
                settings.pause_unfocused,
            );
            if !play {
                // Dropping the preview kills ffmpeg; the editor keeps the last frame.
                *preview = None;
            } else if preview.is_none() && !*failed {
                match VideoPreview::spawn(path, *size, settings.clamped_fps(), settings.hw_decode) {
                    Ok(started) => *preview = Some(started),
                    Err(error) => {
                        *failed = true;
                        tracing::warn!("cannot resume the video: {error}");
                    }
                }
            }
            if let Some(preview) = preview
                && let Some(frame) = preview.try_frame()
                && let Some(surface) = surface_from_bgra(frame.bgra, frame.width, frame.height)
            {
                editor.set_frame_surface(&surface);
            }
            glib::ControlFlow::Continue
        });
        let mut failed = false;
        let preview = if Self::visible(&self.window) {
            match VideoPreview::spawn(&path, size, settings.clamped_fps(), settings.hw_decode) {
                Ok(preview) => Some(preview),
                Err(error) => {
                    failed = true;
                    tracing::warn!("cannot play the video: {error}");
                    None
                }
            }
        } else {
            None
        };
        *self.active.borrow_mut() = Active::Video {
            preview,
            gate: PauseGate::default(),
            failed,
            path,
            size,
            timer,
        };
    }
}

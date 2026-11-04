//! Plays animations and videos in the crop editor's preview.

use std::cell::{Cell, RefCell};
use std::mem;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::cairo;
use gtk::prelude::*;
use gtk::{gio, glib};
use tt500_lcd_ipc::MAX_FPS;

use super::editor::{Editor, surface_from_bgra};
use crate::media::{Source, SourceKind};
use crate::playback::{self, FrameClock, VIDEO_PREVIEW_FPS, VideoPreview};

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
        preview: VideoPreview,
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
}

impl Player {
    /// A player that draws into `editor` and pauses while `window` is hidden or suspended.
    pub fn new(editor: Editor, window: &impl IsA<gtk::Window>) -> Self {
        Self {
            editor,
            window: window.clone().upcast(),
            active: Rc::new(RefCell::new(Active::Idle)),
            generation: Rc::new(Cell::new(0)),
        }
    }

    /// Stops the current playback and starts playing `source` (a still plays nothing).
    ///
    /// Animations decode in the background and follow `fps_cap`; videos stream at a fixed rate.
    pub fn play(&self, path: &Path, source: &Source, fps_cap: u32) {
        self.stop();
        let generation = self.generation.get();
        match source.kind {
            SourceKind::Still => {}
            SourceKind::Animation => {
                let path = path.to_path_buf();
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
            SourceKind::Video => match VideoPreview::spawn(path, source.size) {
                Ok(preview) => self.start_video(preview),
                Err(error) => tracing::warn!("cannot play the video: {error}"),
            },
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

    fn start_video(&self, preview: VideoPreview) {
        let interval = Duration::from_secs(1) / VIDEO_PREVIEW_FPS;
        let active = Rc::clone(&self.active);
        let editor = self.editor.clone();
        let window = self.window.clone();
        let timer = glib::timeout_add_local(interval, move || {
            if !Self::visible(&window) {
                return glib::ControlFlow::Continue;
            }
            if let Active::Video { preview, .. } = &*active.borrow()
                && let Some(frame) = preview.try_frame()
                && let Some(surface) = surface_from_bgra(frame.bgra, frame.width, frame.height)
            {
                editor.set_frame_surface(&surface);
            }
            glib::ControlFlow::Continue
        });
        *self.active.borrow_mut() = Active::Video { preview, timer };
    }
}

//! Persistence: the current content (copied) and brightness (`state.toml`).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tt500_lcd_ipc::{
    ContentInfo, ContentKind, Crop, ErrorCode, Fit, IpcError, LiveSource, ProfileSource,
    ProfileSpec, ShowLiveParams, Size,
};
use tt500_lcd_protocol::{Brightness, Rotation, StandbyImage};
use tt500_lcd_render::{
    MediaKind, Playlist, RenderError, RenderOptions, RenderedBoot, VideoStream,
};

use crate::dirs::Dirs;
use crate::streamer::Content;

const STATE_FILE: &str = "state.toml";
const STATE_VERSION: u32 = 1;
const CONTENT_STEM: &str = "current";
const FALLBACK_EXTENSION: &str = "img";

fn default_brightness() -> u8 {
    Brightness::default().get()
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    #[serde(default = "default_brightness")]
    brightness: u8,
    /// Rotation step 0..=3; a TOML scalar, so it stays above the `[content]` table.
    #[serde(default)]
    rotation: u8,
    content: Option<StoredContent>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            brightness: default_brightness(),
            rotation: Rotation::default().get(),
            content: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredContent {
    /// Copy in the data dir (images and animations); absent for videos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// The original path; videos are played from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<PathBuf>,
    source: String,
    fit: Fit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crop: Option<Crop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_fps: Option<u32>,
    /// A live source (nothing is copied for it); `max_fps` holds its requested fps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live: Option<LiveSource>,
    /// The requested steady-frames setting of a live source; absent means the source's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    steady: Option<bool>,
}

/// How to load content: what `show` asked for, or what was stored.
#[derive(Debug, Clone)]
struct Settings {
    source: String,
    origin: Option<PathBuf>,
    fit: Fit,
    crop: Option<Crop>,
    max_fps: Option<u32>,
}

/// What [`ContentStore::restore`] found.
#[derive(Debug)]
pub enum Restored {
    /// Nothing was stored.
    Nothing,
    /// The stored content is loaded again.
    Loaded(Content, ContentInfo),
    /// A live source was stored; starting it is up to the caller (the screen may not be up yet).
    Live(ShowLiveParams),
    /// The stored content could not be loaded; the message goes into the status.
    Failed(String),
}

/// Persisted content and brightness.
#[derive(Debug)]
pub struct ContentStore {
    dirs: Dirs,
    lock: Mutex<()>,
    /// Bumped whenever the content is replaced or cleared.
    generation: AtomicU64,
}

impl ContentStore {
    /// Directory for state files, also used for standby dry-run dumps.
    pub fn state_dir(&self) -> &Path {
        &self.dirs.state
    }

    pub fn new(dirs: Dirs) -> Self {
        Self {
            dirs,
            lock: Mutex::new(()),
            generation: AtomicU64::new(0),
        }
    }

    /// Changes whenever the stored content is replaced or cleared.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn state_path(&self) -> PathBuf {
        self.dirs.state.join(STATE_FILE)
    }

    fn read_state(&self) -> State {
        let path = self.state_path();
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!("ignoring unreadable {}: {err}", path.display());
                State::default()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => State::default(),
            Err(err) => {
                tracing::warn!("cannot read {}: {err}", path.display());
                State::default()
            }
        }
    }

    fn write_state(&self, state: &State) -> io::Result<()> {
        fs::create_dir_all(&self.dirs.state)?;
        let text = toml::to_string(state).map_err(io::Error::other)?;
        write_atomic(&self.state_path(), text.as_bytes())
    }

    /// The stored brightness; missing or out of range gives 100.
    pub fn brightness(&self) -> Brightness {
        let _guard = self.lock.lock().expect("lock poisoned");
        Brightness::try_from(self.read_state().brightness).unwrap_or_default()
    }

    /// Persists the brightness, keeping the stored content.
    pub fn set_brightness(&self, brightness: Brightness) -> Result<(), IpcError> {
        let _guard = self.lock.lock().expect("lock poisoned");
        let mut state = self.read_state();
        state.brightness = brightness.get();
        self.write_state(&state)
            .map_err(|err| internal(&format!("cannot save state: {err}")))
    }

    /// The stored rotation step; missing or out of range gives 0.
    pub fn rotation(&self) -> Rotation {
        let _guard = self.lock.lock().expect("lock poisoned");
        Rotation::try_from(self.read_state().rotation).unwrap_or_default()
    }

    /// Persists the rotation, keeping the stored content.
    pub fn set_rotation(&self, rotation: Rotation) -> Result<(), IpcError> {
        let _guard = self.lock.lock().expect("lock poisoned");
        let mut state = self.read_state();
        state.rotation = rotation.get();
        self.write_state(&state)
            .map_err(|err| internal(&format!("cannot save state: {err}")))
    }

    /// Renders `src` (or starts it, for a video) and records it; images and animations are also copied.
    pub fn replace(
        &self,
        src: &Path,
        fit: Fit,
        crop: Option<Crop>,
        max_fps: Option<u32>,
    ) -> Result<(Content, ContentInfo), IpcError> {
        check_source(src)?;
        let source = src
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let settings = Settings {
            source,
            origin: Some(src.to_path_buf()),
            fit,
            crop,
            max_fps,
        };
        let rotation = self.rotation();
        let (content, info) = load(src, settings, rotation).map_err(|err| render_error(&err))?;

        let _guard = self.lock.lock().expect("lock poisoned");
        let file = (info.kind != ContentKind::Video)
            .then(|| format!("{CONTENT_STEM}.{}", extension_of(src)));
        self.persist(src, file.as_deref(), &info)
            .map_err(|err| internal(&format!("cannot persist content: {err}")))?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok((content, info))
    }

    /// Records a live source as the content (no files are kept for it).
    pub fn replace_live(
        &self,
        source: &LiveSource,
        fit: Fit,
        crop: Option<Crop>,
        fps: Option<u32>,
        steady: Option<bool>,
    ) -> Result<(), IpcError> {
        let _guard = self.lock.lock().expect("lock poisoned");
        remove_content_files(&self.dirs.data, None);
        let mut state = self.read_state();
        state.content = Some(StoredContent {
            file: None,
            origin: None,
            source: crate::live::label(source),
            fit,
            crop,
            max_fps: fps,
            live: Some(source.clone()),
            steady,
        });
        self.write_state(&state)
            .map_err(|err| internal(&format!("cannot save state: {err}")))?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Renders the still at `src` as a standby image; nothing is stored.
    pub fn render_standby(
        &self,
        src: &Path,
        fit: Fit,
        crop: Option<Crop>,
    ) -> Result<StandbyImage, IpcError> {
        check_source(src)?;
        let options = render_options(fit, crop, None, self.rotation());
        tt500_lcd_render::render_standby_file(src, &options).map_err(|err| render_error(&err))
    }

    /// Renders the animation at `src` as a boot animation; nothing is stored.
    pub fn render_boot_animation(
        &self,
        src: &Path,
        fit: Fit,
        crop: Option<Crop>,
    ) -> Result<RenderedBoot, IpcError> {
        check_source(src)?;
        let options = render_options(fit, crop, None, self.rotation());
        tt500_lcd_render::render_boot_animation_file(src, &options)
            .map_err(|err| render_error(&err))
    }

    /// Records the content; `file` is the name of its copy in the data dir, if it gets one.
    fn persist(&self, src: &Path, file: Option<&str>, info: &ContentInfo) -> io::Result<()> {
        if let Some(file) = file {
            fs::create_dir_all(&self.dirs.data)?;
            let target = self.dirs.data.join(file);
            let tmp = self.dirs.data.join(format!("{file}.tmp"));
            fs::copy(src, &tmp)?;
            fs::rename(&tmp, &target)?;
        }
        remove_content_files(&self.dirs.data, file.map(OsStr::new));
        let mut state = self.read_state();
        state.content = Some(StoredContent {
            file: file.map(str::to_owned),
            origin: Some(src.to_path_buf()),
            source: info.source.clone(),
            fit: info.fit,
            crop: info.crop,
            max_fps: info.max_fps,
            live: None,
            steady: None,
        });
        self.write_state(&state)
    }

    /// Forgets the content (the brightness stays).
    pub fn clear(&self) {
        let _guard = self.lock.lock().expect("lock poisoned");
        self.clear_locked();
    }

    fn clear_locked(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        remove_content_files(&self.dirs.data, None);
        let mut state = self.read_state();
        if state.content.take().is_some()
            && let Err(err) = self.write_state(&state)
        {
            tracing::warn!("cannot save state: {err}");
        }
    }

    /// The persisted state as a profile spec (brightness, rotation and content), read from
    /// `state.toml` rather than from the live status, which may not be restored yet.
    pub fn stored_spec(&self) -> ProfileSpec {
        let _guard = self.lock.lock().expect("lock poisoned");
        let state = self.read_state();
        let mut spec = ProfileSpec {
            brightness: Some(state.brightness),
            rotation: Some(state.rotation),
            ..ProfileSpec::default()
        };
        let Some(stored) = state.content else {
            spec.source = Some(ProfileSource::Clear);
            return spec;
        };
        spec.fit = stored.fit;
        spec.crop = stored.crop;
        spec.fps = stored.max_fps;
        if let Some(source) = stored.live {
            spec.steady = stored.steady;
            spec.source = Some(ProfileSource::Live { source });
        } else {
            // Without an original path the content cannot be shown again; keep it as it is.
            spec.source = stored.origin.map(|path| ProfileSource::Static { path });
        }
        spec
    }

    /// Reloads the persisted content. A missing video file or a missing ffmpeg keeps the
    /// record (the file may come back, e.g. on a drive mounted later); any other failure
    /// clears it. Every failure is reported for the status.
    pub fn restore(&self) -> Restored {
        let _guard = self.lock.lock().expect("lock poisoned");
        let state = self.read_state();
        let rotation = Rotation::try_from(state.rotation).unwrap_or_default();
        let Some(stored) = state.content else {
            return Restored::Nothing;
        };
        if let Some(source) = stored.live {
            return Restored::Live(ShowLiveParams {
                source,
                fit: stored.fit,
                crop: stored.crop,
                fps: stored.max_fps,
                steady: stored.steady,
            });
        }
        let path = match (&stored.file, &stored.origin) {
            (Some(file), _) => Path::new(file)
                .file_name()
                .map(|name| self.dirs.data.join(name)),
            (None, Some(origin)) => Some(origin.clone()),
            (None, None) => None,
        };
        let Some(path) = path else {
            self.clear_locked();
            return Restored::Nothing;
        };
        if stored.file.is_none() && !path.exists() {
            let message = format!(
                "{} not found at {}: it was moved or deleted; show it again",
                stored.source,
                path.display()
            );
            tracing::warn!("cannot restore: {message}");
            return Restored::Failed(message);
        }
        let settings = Settings {
            source: stored.source.clone(),
            origin: stored.origin.clone(),
            fit: stored.fit,
            crop: stored.crop,
            max_fps: stored.max_fps,
        };
        match load(&path, settings, rotation) {
            Ok((content, info)) => Restored::Loaded(content, info),
            Err(err @ RenderError::MissingFfmpeg) => {
                tracing::warn!("cannot restore {}: {err}", path.display());
                Restored::Failed(format!("cannot play {}: {err}", stored.source))
            }
            Err(err) => {
                tracing::warn!("cannot restore {}: {err}", path.display());
                self.clear_locked();
                Restored::Failed(format!("cannot restore {}: {err}", stored.source))
            }
        }
    }
}

/// The source must be an absolute path to an existing regular file.
fn check_source(src: &Path) -> Result<(), IpcError> {
    if !src.is_absolute() {
        return Err(IpcError {
            code: ErrorCode::BadRequest,
            message: format!("path must be absolute: {}", src.display()),
        });
    }
    let meta = fs::metadata(src).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => IpcError {
            code: ErrorCode::NotFound,
            message: format!("no such file: {}", src.display()),
        },
        _ => IpcError {
            code: ErrorCode::Render,
            message: format!("cannot read {}: {err}", src.display()),
        },
    })?;
    if !meta.is_file() {
        return Err(IpcError {
            code: ErrorCode::BadRequest,
            message: format!("not a file: {}", src.display()),
        });
    }
    Ok(())
}

/// The render options for content shown with these settings on a panel at `rotation`.
pub fn render_options(
    fit: Fit,
    crop: Option<Crop>,
    max_fps: Option<u32>,
    rotation: Rotation,
) -> RenderOptions {
    RenderOptions {
        fit: match fit {
            Fit::Cover => tt500_lcd_render::Fit::Cover,
            Fit::Contain => tt500_lcd_render::Fit::Contain,
            Fit::Stretch => tt500_lcd_render::Fit::Stretch,
        },
        crop: crop.map(|c| tt500_lcd_render::Crop {
            x: c.x,
            y: c.y,
            width: c.width,
            height: c.height,
        }),
        max_fps,
        rotation,
        ..RenderOptions::default()
    }
}

/// Renders an image or animation, or starts a video, and describes the result.
fn load(
    path: &Path,
    settings: Settings,
    rotation: Rotation,
) -> Result<(Content, ContentInfo), RenderError> {
    let Settings {
        source,
        origin,
        fit,
        crop,
        max_fps,
    } = settings;
    let options = render_options(fit, crop, max_fps, rotation);
    match tt500_lcd_render::detect(path)? {
        MediaKind::Image => {
            let rendered = tt500_lcd_render::render_file(path, &options)?;
            let kind = match rendered.playlist {
                Playlist::Still(_) => ContentKind::Still,
                Playlist::Animation(_) => ContentKind::Animation,
            };
            let info = ContentInfo {
                source,
                kind,
                frames: u32::try_from(rendered.playlist.frame_count()).unwrap_or(u32::MAX),
                fit,
                source_size: Some(Size {
                    width: rendered.width,
                    height: rendered.height,
                }),
                crop,
                fps: None,
                duration_ms: None,
                path: origin,
                max_fps: if kind == ContentKind::Animation {
                    max_fps
                } else {
                    None
                },
                live: None,
                steady: None,
            };
            Ok((Content::Playlist(rendered.playlist), info))
        }
        MediaKind::Video => {
            let video = tt500_lcd_render::probe(path)?;
            let stream = VideoStream::spawn(path, &video, &options)?;
            tracing::info!(
                "playing {source}: {}x{} at {} fps via {} (ffmpeg pid {:?})",
                video.width,
                video.height,
                video.playback_fps(options.video_fps_cap()),
                stream.decode_path(),
                stream.process_id()
            );
            let info = ContentInfo {
                source,
                kind: ContentKind::Video,
                frames: video.frames_per_loop(options.video_fps_cap()).unwrap_or(0),
                fit,
                source_size: Some(Size {
                    width: video.width,
                    height: video.height,
                }),
                crop,
                fps: Some(video.playback_fps(options.video_fps_cap())),
                duration_ms: video
                    .duration
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                path: origin,
                max_fps,
                live: None,
                steady: None,
            };
            Ok((Content::Video(stream), info))
        }
    }
}

pub fn render_error(err: &RenderError) -> IpcError {
    let code = match err {
        RenderError::Unsupported => ErrorCode::Unsupported,
        RenderError::TooLarge { .. } | RenderError::TooManyFrames { .. } => ErrorCode::TooLarge,
        RenderError::BadCrop { .. } => ErrorCode::BadCrop,
        RenderError::NotStill(_) | RenderError::NotAnimated(_) => ErrorCode::Unsupported,
        RenderError::MissingFfmpeg => ErrorCode::Unavailable,
        _ => ErrorCode::Render,
    };
    IpcError {
        code,
        message: err.to_string(),
    }
}

fn internal(message: &str) -> IpcError {
    IpcError {
        code: ErrorCode::Internal,
        message: message.to_owned(),
    }
}

/// Lowercased extension of `src` if it is short and alphanumeric, else `img`.
fn extension_of(src: &Path) -> String {
    src.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| FALLBACK_EXTENSION.to_owned())
}

fn remove_content_files(data: &Path, keep: Option<&OsStr>) {
    let Ok(entries) = fs::read_dir(data) else {
        return;
    };
    let prefix = format!("{CONTENT_STEM}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) && Some(name.as_os_str()) != keep {
            if let Err(err) = fs::remove_file(entry.path()) {
                tracing::warn!("cannot remove {}: {err}", entry.path().display());
            }
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn store(tmp: &Path) -> ContentStore {
        ContentStore::new(Dirs {
            data: tmp.join("data"),
            state: tmp.join("state"),
            socket: tmp.join("sock"),
        })
    }

    fn loaded(restored: Restored) -> Option<(Content, ContentInfo)> {
        match restored {
            Restored::Loaded(content, info) => Some((content, info)),
            Restored::Nothing | Restored::Live(_) | Restored::Failed(_) => None,
        }
    }

    fn png(tmp: &Path, name: &str) -> PathBuf {
        let img = image::RgbImage::from_fn(200, 100, |x, y| image::Rgb([x as u8, y as u8, 7]));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let path = tmp.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn brightness_defaults_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        assert_eq!(store.brightness().get(), 100);
        store
            .set_brightness(Brightness::try_from(40).unwrap())
            .unwrap();
        assert_eq!(store.brightness().get(), 40);
    }

    #[test]
    fn rotation_defaults_persists_and_survives_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        assert_eq!(store.rotation().get(), 0);
        store.set_rotation(Rotation::try_from(2).unwrap()).unwrap();
        assert_eq!(store.rotation().get(), 2);
        store
            .set_brightness(Brightness::try_from(40).unwrap())
            .unwrap();
        assert_eq!(store.rotation().get(), 2);
        let text = fs::read_to_string(store.state_path()).unwrap();
        assert!(text.contains("rotation = 2"), "{text}");
    }

    #[test]
    fn out_of_range_rotation_falls_back_to_0() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        fs::create_dir_all(tmp.path().join("state")).ok();
        let text = "version = 1\nrotation = 9\n";
        fs::write(store.state_path(), text).unwrap();
        assert_eq!(store.rotation().get(), 0);
    }

    #[test]
    fn out_of_range_brightness_falls_back_to_100() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        fs::create_dir_all(tmp.path().join("state")).unwrap();
        fs::write(
            tmp.path().join("state").join(STATE_FILE),
            "version = 1\nbrightness = 250\n",
        )
        .unwrap();
        assert_eq!(store.brightness().get(), 100);
    }

    #[test]
    fn replace_restore_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        store
            .set_brightness(Brightness::try_from(30).unwrap())
            .unwrap();
        let src = png(tmp.path(), "Cat.PNG");
        let (content, info) = store.replace(&src, Fit::Contain, None, None).unwrap();
        assert!(matches!(content, Content::Playlist(Playlist::Still(_))));
        assert_eq!(
            info.source_size,
            Some(Size {
                width: 200,
                height: 100
            })
        );
        assert_eq!(info.source, "Cat.PNG");
        assert_eq!(info.fit, Fit::Contain);
        assert!(tmp.path().join("data/current.png").exists());

        // The original may go away.
        fs::remove_file(&src).unwrap();
        let (_, restored) = loaded(store.restore()).unwrap();
        assert_eq!(restored, info);

        store.clear();
        assert!(!tmp.path().join("data/current.png").exists());
        assert!(loaded(store.restore()).is_none());
        assert_eq!(store.brightness().get(), 30);
    }

    #[test]
    fn replace_removes_other_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let a = png(tmp.path(), "a.png");
        let b = png(tmp.path(), "b.bin");
        store.replace(&a, Fit::Cover, None, None).unwrap();
        store.replace(&b, Fit::Cover, None, None).unwrap();
        assert!(!tmp.path().join("data/current.png").exists());
        assert!(tmp.path().join("data/current.bin").exists());
    }

    #[test]
    fn replace_errors_map_to_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let err = store
            .replace(Path::new("rel.png"), Fit::Cover, None, None)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
        let err = store
            .replace(&tmp.path().join("missing.png"), Fit::Cover, None, None)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let junk = tmp.path().join("junk.txt");
        fs::write(&junk, b"not an image").unwrap();
        let err = store.replace(&junk, Fit::Cover, None, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        // A failed replace keeps nothing.
        assert!(loaded(store.restore()).is_none());
    }

    #[test]
    fn restore_with_missing_file_clears_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        store
            .replace(&png(tmp.path(), "a.png"), Fit::Cover, None, None)
            .unwrap();
        fs::remove_file(tmp.path().join("data/current.png")).unwrap();
        assert!(loaded(store.restore()).is_none());
        assert!(store.read_state().content.is_none());
    }

    #[test]
    fn crop_is_validated_persisted_and_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let src = png(tmp.path(), "a.png"); // 200x100
        let bad = Crop {
            x: 150,
            y: 0,
            width: 100,
            height: 50,
        };
        let err = store
            .replace(&src, Fit::Cover, Some(bad), None)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadCrop);
        assert!(loaded(store.restore()).is_none());

        let crop = Crop {
            x: 10,
            y: 20,
            width: 120,
            height: 32,
        };
        let (_, info) = store.replace(&src, Fit::Cover, Some(crop), None).unwrap();
        assert_eq!(info.crop, Some(crop));
        let (_, restored) = loaded(store.restore()).unwrap();
        assert_eq!(restored, info);
    }

    #[test]
    fn broken_video_is_an_error_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let path = tmp.path().join("bad.mp4");
        let mut bytes = b"\0\0\0\x18ftypisom\0\0\0\0isom".to_vec();
        bytes.extend([0x55; 200]);
        fs::write(&path, bytes).unwrap();
        let err = store.replace(&path, Fit::Cover, None, None).unwrap_err();
        // Render with ffmpeg installed, Unavailable without it.
        assert!(
            matches!(err.code, ErrorCode::Render | ErrorCode::Unavailable),
            "{err}"
        );
        assert!(loaded(store.restore()).is_none());
    }

    #[test]
    fn old_state_file_still_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        fs::create_dir_all(tmp.path().join("data")).unwrap();
        fs::create_dir_all(tmp.path().join("state")).unwrap();
        fs::copy(
            png(tmp.path(), "src.png"),
            tmp.path().join("data/current.png"),
        )
        .unwrap();
        fs::write(
            tmp.path().join("state").join(STATE_FILE),
            "version = 1\nbrightness = 80\n\n[content]\nfile = \"current.png\"\nsource = \"old.png\"\nfit = \"cover\"\n",
        )
        .unwrap();
        let (_, info) = loaded(store.restore()).expect("old record restores");
        assert_eq!(info.source, "old.png");
        assert_eq!(info.path, None);
    }

    #[test]
    fn missing_referenced_video_is_reported_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        fs::create_dir_all(tmp.path().join("state")).unwrap();
        let origin = tmp.path().join("gone.mp4");
        fs::write(
            tmp.path().join("state").join(STATE_FILE),
            format!(
                "version = 1\nbrightness = 80\n\n[content]\norigin = {:?}\nsource = \"gone.mp4\"\nfit = \"cover\"\n",
                origin.display().to_string()
            ),
        )
        .unwrap();
        for _ in 0..2 {
            match store.restore() {
                Restored::Failed(msg) => assert!(msg.contains("gone.mp4"), "{msg}"),
                other => panic!("expected Failed, got {other:?}"),
            }
        }
        store.clear();
        assert!(matches!(store.restore(), Restored::Nothing));
    }

    #[test]
    fn fps_constants_match_ipc() {
        assert_eq!(tt500_lcd_render::DEFAULT_FPS, tt500_lcd_ipc::DEFAULT_FPS);
        assert_eq!(tt500_lcd_render::MAX_FPS, tt500_lcd_ipc::MAX_FPS);
    }

    #[test]
    fn brightness_default_matches_ipc() {
        assert_eq!(
            Brightness::default().get(),
            tt500_lcd_ipc::DEFAULT_BRIGHTNESS
        );
    }

    #[test]
    fn video_is_referenced_not_copied() {
        let have = |p: &str| {
            std::process::Command::new(p)
                .arg("-version")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipped: ffmpeg/ffprobe not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let clip = tmp.path().join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: ffmpeg cannot encode an mpeg4 clip");
            return;
        }
        let store = store(tmp.path());
        let (_, info) = store.replace(&clip, Fit::Cover, None, None).unwrap();
        assert_eq!(info.kind, ContentKind::Video);
        assert_eq!(info.path.as_deref(), Some(clip.as_path()));
        assert_eq!(info.max_fps, None);
        let copies = fs::read_dir(tmp.path().join("data"))
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        assert_eq!(copies, 0, "no copy in the data dir");
        assert!(loaded(store.restore()).is_some());
    }

    #[test]
    fn live_source_round_trips_and_bumps_the_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let source = LiveSource::Screen {
            output: Some("DP-1".to_owned()),
        };
        let before = store.generation();
        store
            .replace_live(&source, Fit::Contain, None, Some(20), Some(false))
            .unwrap();
        assert!(store.generation() > before);
        let Restored::Live(params) = store.restore() else {
            panic!("expected a live source");
        };
        assert_eq!(params.source, source);
        assert_eq!(params.fit, Fit::Contain);
        assert_eq!(params.steady, Some(false));
        assert_eq!(params.fps, Some(20));
        let before = store.generation();
        store.clear();
        assert!(store.generation() > before);
        assert!(matches!(store.restore(), Restored::Nothing));
    }

    #[test]
    fn state_without_live_still_parses() {
        let text = "version = 1\nbrightness = 50\n[content]\nsource = \"a.png\"\nfit = \"cover\"\n";
        let state: State = toml::from_str(text).unwrap();
        assert!(state.content.unwrap().live.is_none());
    }

    #[test]
    fn stored_spec_follows_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        store.set_rotation(Rotation::try_from(2).unwrap()).unwrap();
        let spec = store.stored_spec();
        assert_eq!(spec.source, Some(ProfileSource::Clear));
        assert_eq!(spec.brightness, Some(100));
        assert_eq!(spec.rotation, Some(2));
        let path = png(tmp.path(), "a.png");
        store.replace(&path, Fit::Cover, None, None).unwrap();
        let spec = store.stored_spec();
        assert_eq!(spec.fit, Fit::Cover);
        assert_eq!(spec.source, Some(ProfileSource::Static { path }));
    }
}

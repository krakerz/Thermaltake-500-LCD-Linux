//! The IPC server: one thread per connection.

use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, BufReader, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tt500_lcd_ipc::{
    ContentInfo, ContentKind, DEFAULT_FPS, DryRunReply, ErrorCode, Event, EventEnvelope,
    FramingError, IpcError, LiveSource, MAX_FPS, Outcome, PreviewReply, Request, RequestEnvelope,
    Response, ShowLiveParams, Size, Status, UploadBootAnimationParams, UploadStandbyParams,
    read_message, write_message,
};
use tt500_lcd_protocol::Brightness;

use crate::boot::write_boot_dry_run;
use crate::hub::StatusHub;
use crate::live::{self, LiveControl};
use crate::standby::write_dry_run;
use crate::store::{self, ContentStore};
use crate::streamer::{Command, Content};

/// How long a request waits for the streamer to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// A boot animation upload takes seconds (start-up plus about 0.3 s per block).
const BOOT_REPLY_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a subscription waits for a change before checking whether its client left.
const PEER_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Read timeout of that check.
const PEER_PROBE_TIMEOUT: Duration = Duration::from_millis(10);
/// Widest preview image; the crop editor shows it at most this large.
const PREVIEW_MAX_WIDTH: u32 = 960;
/// File name of the preview inside the live run directory.
const PREVIEW_FILE: &str = "preview.jpg";

/// What connection threads share.
pub struct ServerContext {
    pub store: ContentStore,
    pub commands: Sender<Command>,
    pub status: Arc<StatusHub>,
    /// The running live source, if any. Held while one starts, which serialises starts.
    pub live: Mutex<Option<LiveControl>>,
    /// Where live sources keep their files (virtual desktop, preview).
    pub run_dir: PathBuf,
}

/// Creates the socket (dir 0700, socket 0600), replacing a stale one.
///
/// # Errors
/// Fails if a live daemon owns the socket or the socket cannot be created.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("tt500-lcdd already running (socket {})", path.display());
        }
        fs::remove_file(path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", path.display()))?;
    Ok(listener)
}

/// Accepts connections forever.
pub fn serve(listener: &UnixListener, ctx: &Arc<ServerContext>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let ctx = Arc::clone(ctx);
                let spawned = thread::Builder::new()
                    .name("ipc-connection".into())
                    .spawn(move || handle_connection(&stream, &ctx));
                if let Err(err) = spawned {
                    tracing::warn!("cannot spawn connection thread: {err}");
                }
            }
            Err(err) => tracing::warn!("accept failed: {err}"),
        }
    }
}

fn handle_connection(stream: &UnixStream, ctx: &ServerContext) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut buf = String::new();
    loop {
        let envelope = match read_message::<RequestEnvelope>(&mut reader, &mut buf) {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(FramingError::Io(_)) => return,
            Err(err) => {
                let outcome = Outcome::Error(IpcError {
                    code: ErrorCode::BadRequest,
                    message: err.to_string(),
                });
                let _ = write_message(&mut writer, &Response { id: 0, outcome });
                return;
            }
        };
        if matches!(envelope.request, Request::Subscribe) {
            stream_events(&mut reader, writer, ctx, envelope.id);
            return;
        }
        let outcome = match dispatch(ctx, envelope.request) {
            Ok(outcome) => outcome,
            Err(err) => Outcome::Error(err),
        };
        let response = Response {
            id: envelope.id,
            outcome,
        };
        if write_message(&mut writer, &response).is_err() {
            return;
        }
    }
}

/// Answers `subscribe` with the current status, then pushes every change until the client leaves.
fn stream_events(
    reader: &mut BufReader<UnixStream>,
    mut writer: &UnixStream,
    ctx: &ServerContext,
    id: u64,
) {
    let (status, changes) = ctx.status.subscribe();
    let response = Response {
        id,
        outcome: Outcome::Result(status),
    };
    if write_message(&mut writer, &response).is_err()
        || reader
            .get_ref()
            .set_read_timeout(Some(PEER_PROBE_TIMEOUT))
            .is_err()
    {
        return;
    }
    let mut scratch = [0_u8; 512];
    loop {
        match changes.recv_timeout(PEER_CHECK_INTERVAL) {
            Ok(status) => {
                let event = EventEnvelope {
                    event: Event::Status(status),
                };
                if write_message(&mut writer, &event).is_err() {
                    return;
                }
            }
            // Input after `subscribe` is ignored; EOF means the client left.
            Err(RecvTimeoutError::Timeout) => match reader.get_mut().read(&mut scratch) {
                Ok(0) => return,
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            },
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn dispatch(ctx: &ServerContext, request: Request) -> Result<Outcome, IpcError> {
    let status = match request {
        Request::Status => Ok(ctx.status.get()),
        Request::Show(params) => {
            check_fps(params.fps)?;
            let (content, info) =
                ctx.store
                    .replace(&params.path, params.fit, params.crop, params.fps)?;
            *ctx.live.lock().expect("lock poisoned") = None;
            ask(ctx, |reply| Command::Show {
                content,
                info,
                reply,
            })
        }
        Request::Clear => {
            ctx.store.clear();
            *ctx.live.lock().expect("lock poisoned") = None;
            ask(ctx, |reply| Command::Clear { reply })
        }
        Request::SetBrightness(params) => {
            let value = Brightness::try_from(params.value).map_err(|err| IpcError {
                code: ErrorCode::BadRequest,
                message: err.to_string(),
            })?;
            ctx.store.set_brightness(value)?;
            ask(ctx, |reply| Command::SetBrightness { value, reply })
        }
        Request::UploadStandby(params) => return upload_standby(ctx, &params),
        Request::UploadBootAnimation(params) => return upload_boot_animation(ctx, &params),
        Request::ShowLive(params) => show_live(ctx, params),
        Request::Preview => return preview(ctx),
        _ => Err(IpcError {
            code: ErrorCode::Unsupported,
            message: "unsupported request".to_owned(),
        }),
    };
    status.map(Outcome::Result)
}

fn check_fps(fps: Option<u32>) -> Result<(), IpcError> {
    match fps {
        Some(fps) if !(1..=MAX_FPS).contains(&fps) => Err(IpcError {
            code: ErrorCode::BadRequest,
            message: format!("fps must be 1..={MAX_FPS}, got {fps}"),
        }),
        _ => Ok(()),
    }
}

/// Describes a live source for the status; `size` is the source frame size.
fn live_info(params: &ShowLiveParams, fps: u32, steady: bool, size: Option<Size>) -> ContentInfo {
    ContentInfo {
        source: live::label(&params.source),
        kind: ContentKind::Live,
        frames: 0,
        fit: params.fit,
        source_size: size,
        crop: params.crop,
        fps: Some(fps),
        duration_ms: None,
        path: None,
        max_fps: params.fps,
        live: Some(params.source.clone()),
        steady: Some(steady),
    }
}

/// Starts a live source, or, when it is the one already running, only changes how it is shaped.
///
/// # Errors
/// `BadRequest` for a bad fps or desktop spec, `Unavailable` when the source can't be reached
/// and `Unsupported` for the portal.
pub fn show_live(ctx: &ServerContext, params: ShowLiveParams) -> Result<Status, IpcError> {
    check_fps(params.fps)?;
    if let LiveSource::Desktop(spec) = &params.source {
        spec.validate().map_err(|message| IpcError {
            code: ErrorCode::BadRequest,
            message,
        })?;
    }
    let fps = params.fps.unwrap_or(DEFAULT_FPS);
    let steady = params
        .steady
        .unwrap_or_else(|| live::default_steady(&params.source));
    let options = store::render_options(params.fit, params.crop, params.fps);
    let mut slot = ctx.live.lock().expect("lock poisoned");
    if let Some(control) = slot.as_ref()
        && control.spec() == &params.source
        && control.is_alive()
    {
        control.set_options(options, fps, steady);
        let size = control.latest().map(|(img, _)| Size {
            width: img.width(),
            height: img.height(),
        });
        let info = live_info(&params, fps, steady, size);
        ctx.store.replace_live(
            &params.source,
            params.fit,
            params.crop,
            params.fps,
            params.steady,
        )?;
        return ask(ctx, |reply| Command::UpdateInfo { info, reply });
    }
    let (stream, control, size) =
        live::start(params.source.clone(), options, fps, steady, &ctx.run_dir)?;
    let info = live_info(&params, fps, steady, Some(size));
    ctx.store.replace_live(
        &params.source,
        params.fit,
        params.crop,
        params.fps,
        params.steady,
    )?;
    *slot = Some(control);
    ask(ctx, |reply| Command::Show {
        content: Content::Live(stream),
        info,
        reply,
    })
}

/// Writes the latest full live frame to `preview.jpg` for the crop editor.
fn preview(ctx: &ServerContext) -> Result<Outcome, IpcError> {
    let control = match ctx.live.try_lock() {
        Ok(slot) => slot.clone(),
        Err(TryLockError::WouldBlock) => {
            return Err(IpcError {
                code: ErrorCode::Busy,
                message: "a live source is starting".to_owned(),
            });
        }
        Err(TryLockError::Poisoned(_)) => panic!("lock poisoned"),
    };
    let Some((img, seq)) = control.and_then(|control| control.latest()) else {
        return Err(IpcError {
            code: ErrorCode::NotFound,
            message: "no live source is running".to_owned(),
        });
    };
    let jpeg = tt500_lcd_render::encode_preview_jpeg(&img, PREVIEW_MAX_WIDTH)
        .map_err(|err| store::render_error(&err))?;
    let internal = |what: &str, err: io::Error| IpcError {
        code: ErrorCode::Internal,
        message: format!("{what}: {err}"),
    };
    fs::create_dir_all(&ctx.run_dir).map_err(|err| internal("cannot create the live dir", err))?;
    let path = ctx.run_dir.join(PREVIEW_FILE);
    store::write_atomic(&path, &jpeg).map_err(|err| internal("cannot write the preview", err))?;
    Ok(Outcome::Preview(PreviewReply {
        path,
        size: Size {
            width: img.width(),
            height: img.height(),
        },
        seq,
    }))
}

/// Renders the still. A dry run writes the packets to disk and stops there; only a real
/// upload reaches the streamer (and so the panel).
fn upload_standby(ctx: &ServerContext, params: &UploadStandbyParams) -> Result<Outcome, IpcError> {
    let image = ctx
        .store
        .render_standby(&params.path, params.fit, params.crop)?;
    if params.dry_run {
        let files = write_dry_run(ctx.store.state_dir(), &image).map_err(|err| IpcError {
            code: ErrorCode::Internal,
            message: format!("cannot write the dry-run files: {err}"),
        })?;
        tracing::info!(
            "standby dry run: {} bytes JPEG, packets in {}; nothing sent",
            image.byte_len(),
            files.packets.display()
        );
        return Ok(Outcome::DryRun(DryRunReply {
            status: ctx.status.get(),
            packets: files.packets,
            summary: files.summary,
        }));
    }
    ask_result(ctx, |reply| Command::UploadStandby { image, reply }).map(Outcome::Result)
}

/// Renders the animation. A dry run writes the packets to disk and stops there; only a real
/// upload reaches the streamer (and so the panel).
fn upload_boot_animation(
    ctx: &ServerContext,
    params: &UploadBootAnimationParams,
) -> Result<Outcome, IpcError> {
    let rendered = ctx
        .store
        .render_boot_animation(&params.path, params.fit, params.crop)?;
    if rendered.truncated() {
        tracing::warn!(
            "boot animation cut from {} to {} frames",
            rendered.resampled_frames,
            rendered.animation.frame_count()
        );
    }
    if params.dry_run {
        let files =
            write_boot_dry_run(ctx.store.state_dir(), &rendered).map_err(|err| IpcError {
                code: ErrorCode::Internal,
                message: format!("cannot write the dry-run files: {err}"),
            })?;
        tracing::info!(
            "boot animation dry run: {} frames at {} ms ({:.1} fps), {} bytes JPEG, packets in {}; \
             nothing sent",
            rendered.animation.frame_count(),
            rendered.animation.frame_delay_ms(),
            1000.0 / f64::from(rendered.animation.frame_delay_ms()),
            rendered.animation.data_len(),
            files.packets.display()
        );
        return Ok(Outcome::DryRun(DryRunReply {
            status: ctx.status.get(),
            packets: files.packets,
            summary: files.summary,
        }));
    }
    let animation = rendered.animation;
    ask_result_within(ctx, BOOT_REPLY_TIMEOUT, |reply| {
        Command::UploadBootAnimation { animation, reply }
    })
    .map(Outcome::Result)
}

fn ask_result(
    ctx: &ServerContext,
    make: impl FnOnce(Sender<Result<Status, IpcError>>) -> Command,
) -> Result<Status, IpcError> {
    ask_result_within(ctx, REPLY_TIMEOUT, make)
}

fn ask_result_within(
    ctx: &ServerContext,
    timeout: Duration,
    make: impl FnOnce(Sender<Result<Status, IpcError>>) -> Command,
) -> Result<Status, IpcError> {
    let (reply, answer) = mpsc::channel();
    ctx.commands.send(make(reply)).map_err(|_| IpcError {
        code: ErrorCode::Internal,
        message: "streamer is not running".to_owned(),
    })?;
    answer.recv_timeout(timeout).map_err(|_| IpcError {
        code: ErrorCode::Busy,
        message: "the streamer did not answer in time".to_owned(),
    })?
}

fn ask(
    ctx: &ServerContext,
    make: impl FnOnce(Sender<Status>) -> Command,
) -> Result<Status, IpcError> {
    let (reply, answer) = mpsc::channel();
    ctx.commands.send(make(reply)).map_err(|_| IpcError {
        code: ErrorCode::Internal,
        message: "streamer is not running".to_owned(),
    })?;
    answer.recv_timeout(REPLY_TIMEOUT).map_err(|_| IpcError {
        code: ErrorCode::Busy,
        message: "the streamer did not answer in time".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use tt500_lcd_ipc::{DeviceState, IPC_VERSION};

    use super::*;
    use crate::dirs::Dirs;
    use crate::live::LiveStream;
    use crate::live::fake::FakeCapturer;

    fn context(tmp: &Path) -> (ServerContext, mpsc::Receiver<Command>) {
        let store = ContentStore::new(Dirs {
            data: tmp.join("data"),
            state: tmp.join("state"),
            socket: tmp.join("sock"),
        });
        let status = Arc::new(StatusHub::new(Status {
            daemon_version: String::new(),
            ipc_version: IPC_VERSION,
            device: DeviceState::Idle,
            content: None,
            brightness: 100,
            content_error: None,
        }));
        let (commands, receiver) = mpsc::channel();
        let ctx = ServerContext {
            store,
            commands,
            status,
            live: Mutex::new(None),
            run_dir: tmp.join("live"),
        };
        (ctx, receiver)
    }

    #[test]
    fn preview_without_a_live_source_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _commands) = context(tmp.path());
        let err = preview(&ctx).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn the_same_source_only_swaps_the_shaping() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, commands) = context(tmp.path());
        let source = LiveSource::Screen { output: None };
        let fake = FakeCapturer::new(960, 256);
        let counter = fake.counter();
        let (_stream, control, _) =
            LiveStream::from_capturer(source.clone(), fake, Default::default(), 30).unwrap();
        *ctx.live.lock().unwrap() = Some(control.clone());
        let answered = std::thread::spawn(move || {
            let Command::UpdateInfo { info, reply } = commands.recv().unwrap() else {
                panic!("expected UpdateInfo");
            };
            reply
                .send(Status {
                    daemon_version: String::new(),
                    ipc_version: IPC_VERSION,
                    device: DeviceState::Idle,
                    content: Some(info.clone()),
                    brightness: 100,
                    content_error: None,
                })
                .unwrap();
            info
        });
        let crop = tt500_lcd_ipc::Crop {
            x: 0,
            y: 0,
            width: 480,
            height: 128,
        };
        show_live(
            &ctx,
            ShowLiveParams {
                source,
                fit: tt500_lcd_ipc::Fit::Contain,
                crop: Some(crop),
                fps: Some(10),
                steady: None,
            },
        )
        .unwrap();
        let info = answered.join().unwrap();
        assert_eq!(info.crop, Some(crop));
        assert_eq!(info.fps, Some(10));
        assert_eq!(info.kind, ContentKind::Live);
        assert!(control.is_alive(), "the worker was not restarted");
        assert!(counter.load(std::sync::atomic::Ordering::SeqCst) >= 1);

        let Outcome::Preview(reply) = preview(&ctx).unwrap() else {
            panic!("expected a preview");
        };
        assert_eq!((reply.size.width, reply.size.height), (960, 256));
        assert!(reply.path.is_file());
    }

    #[test]
    fn a_bad_desktop_spec_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _commands) = context(tmp.path());
        let err = show_live(
            &ctx,
            ShowLiveParams {
                source: LiveSource::Desktop(tt500_lcd_ipc::DesktopSpec {
                    commands: Vec::new(),
                    size: tt500_lcd_ipc::DESKTOP_DEFAULT_SIZE,
                    scale: 2,
                }),
                fit: tt500_lcd_ipc::Fit::Cover,
                crop: None,
                fps: None,
                steady: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
    }
}

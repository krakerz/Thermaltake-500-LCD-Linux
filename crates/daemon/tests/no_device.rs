//! End-to-end tests against the daemon binary in `--no-device` mode (opens no hardware).

#![allow(clippy::unwrap_used)] // helpers in a test crate

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use image::codecs::gif::GifEncoder;
use image::{Delay, Frame, ImageFormat, Rgba, RgbaImage};
use tt500_lcd_ipc::{
    BrightnessParams, Client, ClientError, ContentKind, DeviceState, ErrorCode, Fit, Request,
    ShowParams, Status, UploadBootAnimationParams, UploadStandbyParams,
};

struct Daemon {
    child: Child,
}

impl Daemon {
    fn spawn(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_tt500-lcdd"))
            .arg("--no-device")
            .env("TT500_LCD_SOCKET", socket(root))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        Self { child }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn socket(root: &Path) -> PathBuf {
    root.join("run/tt500-lcd/daemon.sock")
}

fn connect(root: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Client::connect(&socket(root)) {
            Ok(client) => return client,
            Err(err) if Instant::now() > deadline => panic!("daemon not reachable: {err}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Polls `status` until `done` accepts it.
fn wait_for(client: &mut Client, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = client.call(Request::Status).expect("status");
        if done(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out, last status {status:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn connected(status: &Status) -> bool {
    matches!(status.device, DeviceState::Connected { .. })
}

fn write_png(dir: &Path) -> PathBuf {
    let img = RgbaImage::from_fn(300, 200, |x, y| Rgba([x as u8, y as u8, 80, 255]));
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    let path = dir.join("pic.png");
    fs::write(&path, bytes).unwrap();
    path
}

fn write_gif(dir: &Path) -> PathBuf {
    let path = dir.join("anim.gif");
    let mut encoder = GifEncoder::new(fs::File::create(&path).unwrap());
    for shade in [0u8, 120, 240] {
        let img = RgbaImage::from_pixel(64, 32, Rgba([shade, 40, 200 - shade / 2, 255]));
        let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(100, 1));
        encoder.encode_frame(frame).unwrap();
    }
    drop(encoder);
    path
}

fn show(client: &mut Client, path: PathBuf) -> Result<Status, ClientError> {
    client.call(Request::Show(ShowParams {
        path,
        fit: Fit::Cover,
        crop: None,
        fps: None,
    }))
}

fn show_fps(client: &mut Client, path: PathBuf, fps: u32) -> Result<Status, ClientError> {
    client.call(Request::Show(ShowParams {
        path,
        fit: Fit::Cover,
        crop: None,
        fps: Some(fps),
    }))
}

fn brightness(client: &mut Client, value: u8) -> Result<Status, ClientError> {
    client.call(Request::SetBrightness(BrightnessParams { value }))
}

fn code(err: ClientError) -> ErrorCode {
    match err {
        ClientError::Daemon(e) => e.code,
        other => panic!("expected a daemon error, got {other}"),
    }
}

#[test]
fn full_session() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let png = write_png(root);
    let gif = write_gif(root);

    let mut daemon = Daemon::spawn(root);
    let mut client = connect(root);
    let status = client.call(Request::Status).unwrap();
    assert_eq!(status.device, DeviceState::Idle);
    assert_eq!(status.brightness, 100);

    show(&mut client, png).unwrap();
    let status = wait_for(&mut client, connected);
    let content = status.content.expect("content");
    assert_eq!(content.kind, ContentKind::Still);

    show(&mut client, gif).unwrap();
    let status = wait_for(&mut client, connected);
    let content = status.content.expect("content");
    assert_eq!(content.kind, ContentKind::Animation);
    assert_eq!(content.frames, 3);

    assert_eq!(brightness(&mut client, 40).unwrap().brightness, 40);
    assert_eq!(
        code(brightness(&mut client, 101).unwrap_err()),
        ErrorCode::BadRequest
    );
    assert_eq!(client.call(Request::Status).unwrap().brightness, 40);

    // Restart: content and brightness come back from disk.
    drop(client);
    daemon.kill();
    let mut daemon = Daemon::spawn(root);
    let mut client = connect(root);
    let status = wait_for(&mut client, connected);
    assert_eq!(status.brightness, 40);
    let content = status.content.expect("restored content");
    assert_eq!(content.kind, ContentKind::Animation);
    assert_eq!(content.frames, 3);

    // A second daemon on the same socket refuses to start.
    let mut second = Daemon::spawn(root);
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        if let Some(exit) = second.child.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "second daemon kept running");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(!exit.success());

    let status = client.call(Request::Clear).unwrap();
    assert_eq!(status.device, DeviceState::Idle);
    assert!(status.content.is_none());
    assert_eq!(status.brightness, 40);
    let leftovers: Vec<_> = fs::read_dir(root.join("data/tt500-lcd"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("current."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    daemon.kill();
}

#[test]
fn bad_requests_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);
    let err = show(&mut client, root.join("missing.png")).unwrap_err();
    assert_eq!(code(err), ErrorCode::NotFound);
    let err = show(&mut client, PathBuf::from("relative.png")).unwrap_err();
    assert_eq!(code(err), ErrorCode::BadRequest);
}

#[test]
fn command_line_flags() {
    let bin = env!("CARGO_BIN_EXE_tt500-lcdd");
    for flag in ["-h", "--help", "-v", "--version"] {
        let out = Command::new(bin).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("tt500-lcdd"));
    }
    let out = Command::new(bin).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn subscribe_pushes_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let png = write_png(root);
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);

    let (initial, mut subscription) = connect(root).subscribe().unwrap();
    assert!(initial.content.is_none());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(Some(status)) = subscription.next_status() {
            if tx.send(status).is_err() {
                return;
            }
        }
    });
    let next = || rx.recv_timeout(Duration::from_secs(10)).expect("event");

    show(&mut client, png).unwrap();
    loop {
        if next().content.is_some() {
            break;
        }
    }
    client.call(Request::Clear).unwrap();
    loop {
        if next().content.is_none() {
            break;
        }
    }
}

#[test]
fn show_with_bad_fps_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let gif = write_gif(root);
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);
    let err = show_fps(&mut client, gif.clone(), 31).unwrap_err();
    assert_eq!(code(err), ErrorCode::BadRequest);
    let err = show_fps(&mut client, gif.clone(), 0).unwrap_err();
    assert_eq!(code(err), ErrorCode::BadRequest);
    let status = show_fps(&mut client, gif, 30).unwrap();
    assert_eq!(status.content.unwrap().max_fps, Some(30));
}

#[test]
fn standby_dry_run_writes_packets_and_needs_no_device() {
    let tmp = tempfile::tempdir().unwrap();
    let _daemon = Daemon::spawn(tmp.path());
    let mut client = connect(tmp.path());
    let png = write_png(tmp.path());
    let params = |path: PathBuf, dry_run| UploadStandbyParams {
        path,
        fit: Fit::Cover,
        crop: None,
        dry_run,
    };

    let (status, dry) = client.upload_standby(params(png, true)).unwrap();
    assert_eq!(status.device, DeviceState::Idle);
    let dry = dry.expect("dry run reply");
    let raw = fs::read(&dry.packets).unwrap();
    assert!(!raw.is_empty() && raw.len() % 1024 == 0);
    let packets: Vec<&[u8]> = raw.chunks(1024).collect();
    assert_eq!(packets.len() % 2, 0);
    let per_slot = packets.len() / 2;
    assert!(
        packets[..per_slot]
            .iter()
            .all(|p| p[..2] == [0x02, 0x0d] && p[2] == 1)
    );
    assert!(packets[per_slot..].iter().all(|p| p[2] == 0));
    assert_eq!(packets[0][16..24], [0, 0, 0, 0, 0xe0, 0x01, 0x80, 0x00]);
    assert!(
        fs::read_to_string(&dry.summary)
            .unwrap()
            .contains("packets total")
    );
    // The dry run did not touch the (null) device.
    assert_eq!(
        client.call(Request::Status).unwrap().device,
        DeviceState::Idle
    );

    // Animations are refused, not reduced to a frame.
    let err = client
        .upload_standby(params(write_gif(tmp.path()), true))
        .unwrap_err();
    assert!(matches!(err, ClientError::Daemon(e) if e.code == ErrorCode::Unsupported));
    let err = client
        .upload_standby(params("relative.png".into(), true))
        .unwrap_err();
    assert!(matches!(err, ClientError::Daemon(e) if e.code == ErrorCode::BadRequest));
}

#[test]
fn boot_animation_dry_run_and_null_device_upload() {
    let tmp = tempfile::tempdir().unwrap();
    let _daemon = Daemon::spawn(tmp.path());
    let mut client = connect(tmp.path());
    let params = |path: PathBuf, dry_run| UploadBootAnimationParams {
        path,
        fit: Fit::Cover,
        crop: None,
        dry_run,
    };

    let (_, dry) = client
        .upload_boot_animation(params(write_gif(tmp.path()), true))
        .unwrap();
    let dry = dry.expect("dry run reply");
    let raw = fs::read(&dry.packets).unwrap();
    let packets: Vec<&[u8]> = raw.chunks(1024).collect();
    assert_eq!(raw.len() % 1024, 0);
    // Table (3 frames at 100 ms), then one final data block.
    assert_eq!(
        packets[0][..16],
        [2, 0x0c, 0, 0, 0, 0, 1, 0, 0, 16, 0, 0, 0x28, 0, 0xa2, 0x8a]
    );
    assert_eq!(packets[0][16..20], [3, 0, 100, 0]);
    assert!(packets[1..].iter().all(|p| p[2] == 1 && p[11] == 1));
    let text = fs::read_to_string(&dry.summary).unwrap();
    assert!(text.contains("frames: 3"), "{text}");
    assert!(text.contains("truncated: no"), "{text}");

    // A real upload goes to the null device (the daemon runs with --no-device).
    let (_, dry) = client
        .upload_boot_animation(params(write_gif(tmp.path()), false))
        .unwrap();
    assert!(dry.is_none());

    let err = client
        .upload_boot_animation(params(write_png(tmp.path()), true))
        .unwrap_err();
    assert!(matches!(err, ClientError::Daemon(e) if e.code == ErrorCode::Unsupported));
}

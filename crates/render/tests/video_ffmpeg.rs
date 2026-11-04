//! Needs ffmpeg and ffprobe on PATH; every test passes with a "skipped" note without them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tt500_lcd_protocol::Frame;
use tt500_lcd_render::{
    Crop, DecodePath, HWDEC_ENV, MediaKind, Playlist, RenderError, RenderOptions, VideoPoll,
    VideoStream, detect, probe, render_file,
};

fn have_ffmpeg() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|program| {
        Command::new(program)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Encodes 1 s of lavfi testsrc (320x240, 25 fps) to `dir/name`; `None` if this ffmpeg can't.
fn make(dir: &Path, name: &str, args: &[&str]) -> Option<PathBuf> {
    let out = dir.join(name);
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=1",
        ])
        .args(args)
        .arg(&out)
        .status()
        .is_ok_and(|s| s.success())
        .then_some(out)
}

fn make_mp4(dir: &Path) -> PathBuf {
    make(dir, "clip.mp4", &["-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
        .expect("ffmpeg can encode an mpeg4 clip")
}

#[test]
fn mp4_probes_plays_loops_and_stops() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = make_mp4(dir.path());
    assert_eq!(detect(&clip).unwrap(), MediaKind::Video);
    let info = probe(&clip).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert_eq!(info.playback_fps(15), 15);
    let secs = info.duration.expect("duration known").as_secs_f64();
    assert!((secs - 1.0).abs() < 0.2, "duration {secs}");

    let options = RenderOptions {
        crop: Some(Crop {
            x: 0,
            y: 60,
            width: 320,
            height: 120,
        }),
        ..RenderOptions::default()
    };
    let stream = VideoStream::spawn(&clip, &info, &options).unwrap();
    Frame::from_jpeg(stream.first_frame().as_bytes().to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut frames = 0;
    // More than the 15 frames of one loop, so this proves looping at EOF.
    while frames < 20 {
        match stream.poll() {
            VideoPoll::Frame(_) => frames += 1,
            VideoPoll::Pending => {
                assert!(Instant::now() < deadline, "only {frames} frames in time");
                thread::sleep(Duration::from_millis(5));
            }
            VideoPoll::Ended(reason) => panic!("stream ended early: {reason}"),
        }
    }
    // The 1 s clip is a cached loop by now: ffmpeg was stopped and reaped, and the replay goes on.
    assert_eq!(
        stream.process_id(),
        None,
        "ffmpeg still running after the loop"
    );
    assert!(matches!(stream.poll(), VideoPoll::Frame(_)));
    drop(stream);
}

#[test]
fn uncached_stream_reaps_ffmpeg_on_drop() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = make_mp4(dir.path());
    let mut info = probe(&clip).unwrap();
    info.duration = None; // unknown length: never cached
    let stream = VideoStream::spawn(&clip, &info, &RenderOptions::default()).unwrap();
    let pid = stream.process_id().unwrap();
    drop(stream);
    // A zombie would still have a /proc entry.
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "ffmpeg {pid} left behind"
    );
}

fn vaapi_h264_available() -> bool {
    let node = std::fs::read_dir("/dev/dri").is_ok_and(|mut d| {
        d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("renderD")))
    });
    node && Command::new("vainfo")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("H264High"))
}

#[test]
fn h264_uses_vaapi_when_available_and_always_plays() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(clip) = make(
        dir.path(),
        "h264.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
    ) else {
        eprintln!("skipped: this ffmpeg has no libx264 encoder");
        return;
    };
    let mut info = probe(&clip).unwrap();
    info.duration = None; // keep ffmpeg running; the cache is covered elsewhere
    let stream = VideoStream::spawn(&clip, &info, &RenderOptions::default()).unwrap();
    Frame::from_jpeg(stream.first_frame().as_bytes().to_vec()).unwrap();
    let forced_software = std::env::var(HWDEC_ENV).is_ok_and(|v| v == "0");
    if forced_software {
        assert_eq!(stream.decode_path(), &DecodePath::Software);
    } else if vaapi_h264_available() {
        assert_eq!(stream.decode_path(), &DecodePath::Vaapi);
    } else {
        eprintln!(
            "VA-API not available here; decoded via {}",
            stream.decode_path()
        );
    }
}

#[test]
fn bad_crop_is_rejected() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = make_mp4(dir.path());
    let info = probe(&clip).unwrap();
    let options = RenderOptions {
        crop: Some(Crop {
            x: 300,
            y: 0,
            width: 100,
            height: 10,
        }),
        ..RenderOptions::default()
    };
    assert!(matches!(
        VideoStream::spawn(&clip, &info, &options),
        Err(RenderError::BadCrop {
            width: 320,
            height: 240,
            ..
        })
    ));
}

#[test]
fn broken_video_fails_cleanly() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.mp4");
    let mut bytes = b"\0\0\0\x18ftypisom\0\0\0\0isom".to_vec();
    bytes.extend([0x55; 200]);
    std::fs::write(&bad, bytes).unwrap();
    assert_eq!(detect(&bad).unwrap(), MediaKind::Video);
    assert!(matches!(probe(&bad), Err(RenderError::Video(_))));
}

#[test]
fn apng_and_animated_webp_animate() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let animates = |path: &Path| {
        let rendered = render_file(path, &RenderOptions::default()).unwrap();
        matches!(rendered.playlist, Playlist::Animation(ref f) if f.len() > 1)
    };
    let apng = make(dir.path(), "a.apng", &["-plays", "0", "-f", "apng"])
        .expect("ffmpeg can encode an APNG");
    assert!(animates(&apng), "APNG should animate");
    match make(
        dir.path(),
        "a.webp",
        &["-c:v", "libwebp_anim", "-loop", "0"],
    ) {
        Some(webp) => assert!(animates(&webp), "animated WebP should animate"),
        None => eprintln!("skipped animated WebP: this ffmpeg has no libwebp_anim encoder"),
    }
}

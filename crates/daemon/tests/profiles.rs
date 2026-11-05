//! End-to-end tests of profiles against the daemon binary in `--no-device` mode.
//! No time-based or `/proc` assertions: the scheduler is covered by unit tests.

#![allow(clippy::unwrap_used)] // helpers in a test crate

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use image::{ImageFormat, Rgba, RgbaImage};
use tt500_lcd_ipc::{
    Client, ClientError, ErrorCode, Fit, Profile, ProfileNameParams, ProfileRenameParams,
    ProfileReorderParams, ProfileSaveParams, ProfileSource, ProfileSpec, ProfileTriggersParams,
    Request, ScheduleEntry, ShowParams, Status, TimeOfDay, Triggers,
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

fn write_png(dir: &Path, name: &str, shade: u8) -> PathBuf {
    let img = RgbaImage::from_fn(300, 200, |x, y| Rgba([x as u8, y as u8, shade, 255]));
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn show(client: &mut Client, path: &Path) -> Status {
    client
        .call(Request::Show(ShowParams {
            path: path.to_path_buf(),
            fit: Fit::Cover,
            crop: None,
            fps: None,
        }))
        .unwrap()
}

fn static_spec(path: &Path, brightness: u8) -> ProfileSpec {
    ProfileSpec {
        brightness: Some(brightness),
        source: Some(ProfileSource::Static {
            path: path.to_path_buf(),
        }),
        ..ProfileSpec::default()
    }
}

fn save(client: &mut Client, name: &str, spec: Option<ProfileSpec>) -> Result<Status, ClientError> {
    client.call(Request::ProfileSave(ProfileSaveParams {
        name: name.to_owned(),
        spec,
    }))
}

fn activate(client: &mut Client, name: &str) -> Result<Status, ClientError> {
    client.call(Request::ProfileActivate(ProfileNameParams {
        name: name.to_owned(),
    }))
}

fn list(client: &mut Client) -> Vec<Profile> {
    client.profiles().unwrap().profiles
}

fn names(client: &mut Client) -> Vec<String> {
    list(client).into_iter().map(|p| p.name).collect()
}

fn code(err: ClientError) -> ErrorCode {
    match err {
        ClientError::Daemon(e) => e.code,
        other => panic!("expected a daemon error, got {other}"),
    }
}

fn shown_path(status: &Status) -> Option<PathBuf> {
    status.content.as_ref().and_then(|c| c.path.clone())
}

#[test]
fn profile_session() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = write_png(root, "a.png", 10);
    let b = write_png(root, "b.png", 200);
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);

    // Fresh daemon: profiles supported, only the built-in Default, which is marked.
    let first = client.profiles().unwrap().status.profile.unwrap();
    assert_eq!(first.active.unwrap().name, "Default");
    assert_eq!(names(&mut client), ["Default"]);

    save(&mut client, "Day", Some(static_spec(&a, 90))).unwrap();
    save(&mut client, "Night", Some(static_spec(&b, 20))).unwrap();
    assert_eq!(names(&mut client), ["Default", "Day", "Night"]);

    // Activate.
    let status = activate(&mut client, "night").unwrap();
    let profile = status.profile.clone().unwrap();
    assert_eq!(profile.active.unwrap().name, "Night");
    assert_eq!(shown_path(&status), Some(b.clone()));
    assert_eq!(status.brightness, 20);

    // A missing file fails and keeps content and marker.
    let gone = root.join("gone.png");
    save(&mut client, "Broken", Some(static_spec(&gone, 50))).unwrap();
    let err = activate(&mut client, "Broken").unwrap_err();
    assert_eq!(code(err), ErrorCode::NotFound);
    let status = client.call(Request::Status).unwrap();
    assert_eq!(shown_path(&status), Some(b.clone()));
    assert_eq!(status.profile.unwrap().active.unwrap().name, "Night");

    // A direct Show keeps the marker and flags it as changed.
    show(&mut client, &a);
    let status = client.call(Request::Status).unwrap();
    let active = status.profile.unwrap().active.unwrap();
    assert_eq!(active.name, "Night");
    assert!(active.modified);

    // Save the current content (no spec) keeps the source path.
    save(&mut client, "Snap", None).unwrap();
    let snap = list(&mut client)
        .into_iter()
        .find(|p| p.name == "Snap")
        .unwrap();
    assert_eq!(
        snap.spec.source,
        Some(ProfileSource::Static { path: a.clone() })
    );

    // Rename, reorder, delete.
    client
        .call(Request::ProfileRename(ProfileRenameParams {
            from: "Snap".to_owned(),
            to: "Current".to_owned(),
        }))
        .unwrap();
    assert_eq!(
        names(&mut client),
        ["Default", "Day", "Night", "Broken", "Current"]
    );
    let order: Vec<String> = ["Current", "Default", "Day", "Night", "Broken"]
        .map(str::to_owned)
        .into();
    client
        .call(Request::ProfileReorder(ProfileReorderParams {
            names: order.clone(),
        }))
        .unwrap();
    assert_eq!(names(&mut client), order);
    let stale = client
        .call(Request::ProfileReorder(ProfileReorderParams {
            names: vec!["Day".to_owned()],
        }))
        .unwrap_err();
    assert_eq!(code(stale), ErrorCode::BadRequest);
    assert_eq!(names(&mut client), order);
    client
        .call(Request::ProfileDelete(ProfileNameParams {
            name: "Broken".to_owned(),
        }))
        .unwrap();
    assert_eq!(names(&mut client), ["Current", "Default", "Day", "Night"]);
    let missing = activate(&mut client, "Broken").unwrap_err();
    assert_eq!(code(missing), ErrorCode::NotFound);

    // Triggers.
    let triggers = Triggers {
        processes: vec!["game.exe".to_owned()],
        ranges: Vec::new(),
        schedule: vec![ScheduleEntry {
            time: TimeOfDay::new(22, 30).unwrap(),
            days: Vec::new(),
        }],
    };
    client
        .call(Request::ProfileSetTriggers(ProfileTriggersParams {
            name: "Night".to_owned(),
            triggers: triggers.clone(),
        }))
        .unwrap();
    let night = list(&mut client)
        .into_iter()
        .find(|p| p.name == "Night")
        .unwrap();
    assert_eq!(night.triggers, triggers);
}

#[test]
fn profiles_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = write_png(root, "a.png", 10);
    let before = {
        let _daemon = Daemon::spawn(root);
        let mut client = connect(root);
        save(&mut client, "One", Some(static_spec(&a, 40))).unwrap();
        save(&mut client, "Two", Some(static_spec(&a, 60))).unwrap();
        client
            .call(Request::ProfileSetTriggers(ProfileTriggersParams {
                name: "Two".to_owned(),
                triggers: Triggers {
                    processes: vec!["sleep".to_owned()],
                    ranges: Vec::new(),
                    schedule: Vec::new(),
                },
            }))
            .unwrap();
        client
            .call(Request::ProfileReorder(ProfileReorderParams {
                names: ["Two", "One", "Default"].map(str::to_owned).into(),
            }))
            .unwrap();
        activate(&mut client, "One").unwrap();
        list(&mut client)
    };
    // The first daemon is killed on drop; the socket file may linger, the new one replaces it.
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);
    assert_eq!(list(&mut client), before);
    assert_eq!(names(&mut client), ["Two", "One", "Default"]);
}

fn named(client: &mut Client, name: &str) -> Result<Status, ClientError> {
    client.call(Request::ProfileDelete(ProfileNameParams {
        name: name.to_owned(),
    }))
}

#[test]
fn default_is_protected_and_resettable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = write_png(root, "a.png", 10);
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);

    // Delete, rename and shadowing are refused.
    assert_eq!(
        code(named(&mut client, "default").unwrap_err()),
        ErrorCode::BadRequest
    );
    let rename = |client: &mut Client, from: &str, to: &str| {
        client.call(Request::ProfileRename(ProfileRenameParams {
            from: from.to_owned(),
            to: to.to_owned(),
        }))
    };
    assert_eq!(
        code(rename(&mut client, "Default", "Home").unwrap_err()),
        ErrorCode::BadRequest
    );
    save(&mut client, "Mine", Some(static_spec(&a, 30))).unwrap();
    assert_eq!(
        code(rename(&mut client, "Mine", "default").unwrap_err()),
        ErrorCode::BadRequest
    );

    // Only Default can be reset.
    let reset = |client: &mut Client, name: &str| {
        client.call(Request::ProfileReset(ProfileNameParams {
            name: name.to_owned(),
        }))
    };
    assert_eq!(
        code(reset(&mut client, "Mine").unwrap_err()),
        ErrorCode::BadRequest
    );

    // Show something, change brightness and rotation, then reset the active Default.
    show(&mut client, &a);
    client
        .call(Request::SetBrightness(tt500_lcd_ipc::BrightnessParams {
            value: 40,
        }))
        .unwrap();
    client
        .call(Request::SetRotation(tt500_lcd_ipc::RotationParams {
            value: 2,
        }))
        .unwrap();
    let status = client.call(Request::Status).unwrap();
    let active = status.profile.unwrap().active.unwrap();
    assert_eq!(active.name, "Default");
    assert!(active.modified);
    let status = reset(&mut client, "Default").unwrap();
    assert!(status.content.is_none());
    assert_eq!(status.brightness, 100);
    assert_eq!(status.rotation, Some(0));
    let active = status.profile.unwrap().active.unwrap();
    assert_eq!(active.name, "Default");
    assert!(!active.modified);
    let default = list(&mut client).remove(0);
    assert_eq!(default.spec.source, Some(ProfileSource::Clear));
}

#[test]
fn migration_renames_an_existing_default_and_seeds_from_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state/tt500-lcd");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("state.toml"),
        "version = 1\nbrightness = 35\nrotation = 1\n",
    )
    .unwrap();
    fs::write(
        state.join("profiles.toml"),
        "version = 1\n\n[[profile]]\nname = \"default\"\n[profile.spec]\nbrightness = 7\n",
    )
    .unwrap();
    let _daemon = Daemon::spawn(root);
    let mut client = connect(root);
    let profiles = list(&mut client);
    let listed: Vec<_> = profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(listed, ["Default", "Default (2)"]);
    assert_eq!(profiles[0].spec.brightness, Some(35));
    assert_eq!(profiles[0].spec.rotation, Some(1));
    assert_eq!(profiles[0].spec.source, Some(ProfileSource::Clear));
    assert_eq!(profiles[1].spec.brightness, Some(7));
}

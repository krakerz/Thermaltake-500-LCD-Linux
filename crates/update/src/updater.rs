use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tt500_lcd_setup::{DAEMON_BINARY, GUI_BINARY, Runner, SystemRunner, config_home};

use crate::error::UpdateError;
use crate::files::{self, Staged};
use crate::http::{Http, UreqHttp, get_bytes};
use crate::install::{APPIMAGE_SUFFIX, ARCHIVE_SUFFIX, InstallKind, UPDATE_URL_ENV};
use crate::release::{Asset, Release, digest_of, sum_in};
use crate::version::Version;

/// The GitHub API call that returns the newest published (never draft) release.
const LATEST_URL: &str =
    "https://api.github.com/repos/krakerz/Thermaltake-500-LCD-Linux/releases/latest";
/// Name of the file listing the release's SHA-256 sums.
const SUMS_NAME: &str = "SHA256SUMS";
/// The CLI is the binary asked to prove a new archive install starts.
const CLI_BINARY: &str = "tt500-lcd";

/// How often the automatic check may run.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Whether the automatic check is due: never run, or at least a day ago.
#[must_use]
pub fn check_due(last_checked: Option<u64>, now: u64) -> bool {
    last_checked.is_none_or(|last| now.saturating_sub(last) >= CHECK_INTERVAL_SECS)
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What an installed update left to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The service was running and has been restarted on the new binaries.
    pub service_restarted: bool,
    /// The restart failed; the update itself is in place.
    pub restart_error: Option<String>,
    /// The GUI to start again, if this copy can be started again.
    pub gui: Option<PathBuf>,
}

/// Checks for and installs updates.
#[derive(Debug)]
pub struct Updater<H, R> {
    http: H,
    runner: R,
    kind: InstallKind,
    current: Version,
    api_url: String,
    staging: PathBuf,
    config_home: PathBuf,
}

impl Updater<UreqHttp, SystemRunner> {
    /// An updater for this running copy: real HTTP, real `systemctl`, the cache directory for
    /// downloads, `TT500_UPDATE_URL` as the API address when set.
    ///
    /// # Errors
    ///
    /// Fails when there is no usable config directory.
    pub fn for_this_install() -> Result<Self, UpdateError> {
        let cache = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|home| home.join(".cache"))
            })
            .unwrap_or_else(env::temp_dir);
        Ok(Self::new(
            UreqHttp::default(),
            SystemRunner,
            InstallKind::current(),
            Version::current(),
            env::var(UPDATE_URL_ENV).unwrap_or_else(|_| LATEST_URL.to_owned()),
            cache.join("tt500-lcd/update"),
            config_home()?,
        ))
    }
}

impl<H: Http, R: Runner> Updater<H, R> {
    /// Builds an updater from its parts.
    #[must_use]
    pub fn new(
        http: H,
        runner: R,
        kind: InstallKind,
        current: Version,
        api_url: String,
        staging: PathBuf,
        config_home: PathBuf,
    ) -> Self {
        Self {
            http,
            runner,
            kind,
            current,
            api_url,
            staging,
            config_home,
        }
    }

    /// How this copy was installed.
    #[must_use]
    pub fn kind(&self) -> &InstallKind {
        &self.kind
    }

    /// The running version.
    #[must_use]
    pub fn current(&self) -> Version {
        self.current
    }

    /// Asks GitHub for the newest published release; `Some` only when it is newer than this one.
    ///
    /// # Errors
    ///
    /// Fails for source builds, network errors and malformed answers.
    pub fn check(&self) -> Result<Option<Release>, UpdateError> {
        if !self.kind.can_update() {
            return Err(UpdateError::FromSource);
        }
        let json = get_bytes(&self.http, &self.api_url)?;
        let release = Release::from_json(&json)?;
        Ok((release.version() > self.current).then_some(release))
    }

    /// Downloads `release`, verifies it and replaces every installed binary, all or nothing,
    /// then restarts the service. `progress(received, total)` follows the download.
    ///
    /// # Errors
    ///
    /// Fails for source builds, a missing asset, download or checksum errors, and file errors.
    /// On any failure the previous version is still installed.
    pub fn apply(
        &self,
        release: &Release,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Applied, UpdateError> {
        let result = match &self.kind {
            InstallKind::Archive { prefix } => self.apply_archive(release, prefix, progress),
            InstallKind::AppImage { path } => self.apply_appimage(release, path, progress),
            InstallKind::Source => Err(UpdateError::FromSource),
        };
        let _ = fs::remove_dir_all(&self.staging);
        result?;
        let (service_restarted, restart_error) =
            match tt500_lcd_setup::restart_service(&self.runner, &self.config_home) {
                Ok(restarted) => (restarted, None),
                Err(error) => (false, Some(error.to_string())),
            };
        Ok(Applied {
            service_restarted,
            restart_error,
            gui: self.kind.gui_path(),
        })
    }

    /// The SHA-256 published for `asset`: the API's digest, else the release's `SHA256SUMS`.
    fn expected_sum(
        &self,
        release: &Release,
        asset: &Asset,
    ) -> Result<Option<String>, UpdateError> {
        if let Some(digest) = digest_of(asset) {
            return Ok(Some(digest));
        }
        let Some(sums) = release.asset_named(SUMS_NAME) else {
            return Ok(None);
        };
        let text = get_bytes(&self.http, &sums.browser_download_url)?;
        Ok(sum_in(&String::from_utf8_lossy(&text), &asset.name))
    }

    fn apply_archive(
        &self,
        release: &Release,
        prefix: &Path,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), UpdateError> {
        let asset = release.asset_ending(ARCHIVE_SUFFIX)?;
        let expected = self.expected_sum(release, asset)?;
        let archive = self.staging.join(&asset.name);
        files::download(
            &self.http,
            &asset.browser_download_url,
            &archive,
            expected.as_deref(),
            progress,
        )?;
        let unpacked = self.staging.join("unpacked");
        files::extract(&archive, &unpacked)?;
        let bundle = files::find_bundle(&unpacked, DAEMON_BINARY)?;
        let bin = prefix.join("bin");
        let mut staged: Vec<Staged> = Vec::new();
        for name in [DAEMON_BINARY, CLI_BINARY, GUI_BINARY] {
            let source = bundle.join("bin").join(name);
            let item = if source.is_file() {
                files::stage(&source, &bin.join(name))
            } else {
                Err(UpdateError::MissingInArchive(format!("bin/{name}")))
            };
            match item {
                Ok(item) => staged.push(item),
                Err(error) => {
                    files::discard_staged(&staged);
                    return Err(error);
                }
            }
        }
        self.swap_and_verify(&staged, &bin.join(CLI_BINARY), &["--version"])
    }

    fn apply_appimage(
        &self,
        release: &Release,
        path: &Path,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), UpdateError> {
        let asset = release.asset_ending(APPIMAGE_SUFFIX)?;
        let expected = self.expected_sum(release, asset)?;
        // Beside the original, so the final rename stays on one filesystem and is atomic.
        let part = path.with_extension("AppImage.part");
        files::download(
            &self.http,
            &asset.browser_download_url,
            &part,
            expected.as_deref(),
            progress,
        )?;
        let staged = files::stage(&part, path);
        let _ = fs::remove_file(&part);
        // The new AppRun refreshes the daemon copy in ~/.local/lib/tt500-lcd when it starts.
        self.swap_and_verify(&[staged?], path, &["--cli", "--version"])
    }

    /// Swaps all staged files in, then runs `probe args` to prove the new copy starts; if it
    /// does not, everything is put back.
    fn swap_and_verify(
        &self,
        staged: &[Staged],
        probe: &Path,
        args: &[&str],
    ) -> Result<(), UpdateError> {
        let swaps = files::replace_all(staged)?;
        let broken = match self.runner.run(&probe.to_string_lossy(), args) {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(output.status.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(reason) = broken {
            files::undo(&swaps);
            return Err(UpdateError::NewCopyBroken(reason));
        }
        files::discard(&swaps);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::install::MARKER;

    #[derive(Default)]
    struct FakeHttp {
        routes: Vec<(String, Vec<u8>)>,
    }

    impl Http for FakeHttp {
        fn get(
            &self,
            url: &str,
            sink: &mut dyn Write,
            progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), UpdateError> {
            let (_, body) = self
                .routes
                .iter()
                .find(|(u, _)| u == url)
                .ok_or_else(|| UpdateError::Http(format!("404 {url}")))?;
            for chunk in body.chunks(7) {
                sink.write_all(chunk)
                    .map_err(|e| UpdateError::Http(e.to_string()))?;
                progress(chunk.len() as u64, Some(body.len() as u64));
            }
            Ok(())
        }
    }

    /// Records calls; programs ending in `broken` fail to start.
    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<String>>,
        broken: Option<&'static str>,
    }

    impl Runner for &FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            let fails = self.broken.is_some_and(|b| program.ends_with(b));
            Ok(Output {
                status: ExitStatus::from_raw(if fails { 1 << 8 } else { 0 }),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (path, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o755);
            builder
                .append_data(&mut header, path, text.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn release_json(tag: &str, assets: &[(&str, &str)]) -> Vec<u8> {
        let assets: Vec<String> = assets
            .iter()
            .map(|(name, digest)| {
                format!(
                    r#"{{"name":"{name}","browser_download_url":"http://fake/{name}","digest":"{digest}"}}"#
                )
            })
            .collect();
        format!(r#"{{"tag_name":"{tag}","assets":[{}]}}"#, assets.join(",")).into_bytes()
    }

    struct Setup {
        _dir: tempfile::TempDir,
        prefix: PathBuf,
        config: PathBuf,
        staging: PathBuf,
    }

    fn installed_prefix() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        for name in [DAEMON_BINARY, CLI_BINARY, GUI_BINARY] {
            fs::write(prefix.join("bin").join(name), format!("old {name}")).unwrap();
        }
        fs::create_dir_all(prefix.join(MARKER).parent().unwrap()).unwrap();
        fs::write(prefix.join(MARKER), "").unwrap();
        Setup {
            config: dir.path().join("config"),
            staging: dir.path().join("cache/update"),
            prefix,
            _dir: dir,
        }
    }

    fn updater<'a>(
        setup: &Setup,
        http: FakeHttp,
        runner: &'a FakeRunner,
        kind: InstallKind,
    ) -> Updater<FakeHttp, &'a FakeRunner> {
        Updater::new(
            http,
            runner,
            kind,
            Version::parse("0.5.0").unwrap(),
            "http://fake/latest".to_owned(),
            setup.staging.clone(),
            setup.config.clone(),
        )
    }

    fn archive_kind(setup: &Setup) -> InstallKind {
        InstallKind::Archive {
            prefix: setup.prefix.clone(),
        }
    }

    const ARCHIVE_NAME: &str = "tt500-lcd-0.6.0-linux-x86_64.tar.gz";

    fn new_archive() -> Vec<u8> {
        tarball(&[
            (
                "tt500-lcd-0.6.0-linux-x86_64/bin/tt500-lcdd",
                "new tt500-lcdd",
            ),
            (
                "tt500-lcd-0.6.0-linux-x86_64/bin/tt500-lcd",
                "new tt500-lcd",
            ),
            (
                "tt500-lcd-0.6.0-linux-x86_64/bin/tt500-lcd-gui",
                "new tt500-lcd-gui",
            ),
        ])
    }

    fn archive_http(archive: Vec<u8>, digest: &str) -> FakeHttp {
        FakeHttp {
            routes: vec![
                (
                    "http://fake/latest".into(),
                    release_json("v0.6.0", &[(ARCHIVE_NAME, digest)]),
                ),
                (format!("http://fake/{ARCHIVE_NAME}"), archive),
            ],
        }
    }

    fn bin_text(setup: &Setup, name: &str) -> String {
        fs::read_to_string(setup.prefix.join("bin").join(name)).unwrap()
    }

    #[test]
    fn same_older_and_source_get_nothing() {
        let setup = installed_prefix();
        let runner = FakeRunner::default();
        for tag in ["v0.5.0", "v0.4.9"] {
            let http = FakeHttp {
                routes: vec![("http://fake/latest".into(), release_json(tag, &[]))],
            };
            let up = updater(&setup, http, &runner, archive_kind(&setup));
            assert!(up.check().unwrap().is_none(), "{tag}");
        }
        let up = updater(&setup, FakeHttp::default(), &runner, InstallKind::Source);
        assert!(matches!(up.check(), Err(UpdateError::FromSource)));
    }

    #[test]
    fn archive_update_replaces_all_binaries() {
        let setup = installed_prefix();
        let runner = FakeRunner::default();
        let archive = new_archive();
        let digest = format!("sha256:{}", sha(&archive));
        let up = updater(
            &setup,
            archive_http(archive, &digest),
            &runner,
            archive_kind(&setup),
        );
        let release = up.check().unwrap().expect("newer release");
        let mut last = (0, None);
        let applied = up.apply(&release, &mut |r, t| last = (r, t)).unwrap();
        for bin in [DAEMON_BINARY, CLI_BINARY, GUI_BINARY] {
            assert_eq!(bin_text(&setup, bin), format!("new {bin}"));
        }
        assert!(last.1.is_some());
        assert!(!setup.staging.exists());
        assert_eq!(fs::read_dir(setup.prefix.join("bin")).unwrap().count(), 3);
        assert_eq!(applied.gui, Some(setup.prefix.join("bin/tt500-lcd-gui")));
        // No unit file exists in the temp config dir, so no restart happens.
        assert!(!applied.service_restarted && applied.restart_error.is_none());
    }

    #[test]
    fn restarts_an_installed_running_service() {
        let setup = installed_prefix();
        let unit = setup.config.join("systemd/user/tt500-lcd.service");
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, "").unwrap();
        let runner = FakeRunner::default();
        let up = updater(
            &setup,
            archive_http(new_archive(), ""),
            &runner,
            archive_kind(&setup),
        );
        let release = up.check().unwrap().unwrap();
        let applied = up.apply(&release, &mut |_, _| {}).unwrap();
        assert!(applied.service_restarted);
        assert_eq!(
            runner.calls.borrow().last().unwrap(),
            "systemctl --user restart tt500-lcd.service"
        );
    }

    #[test]
    fn checksum_mismatch_touches_nothing() {
        let setup = installed_prefix();
        let runner = FakeRunner::default();
        let digest = format!("sha256:{}", sha(b"other"));
        let up = updater(
            &setup,
            archive_http(new_archive(), &digest),
            &runner,
            archive_kind(&setup),
        );
        let release = up.check().unwrap().unwrap();
        let err = up.apply(&release, &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }), "{err}");
        assert_eq!(bin_text(&setup, CLI_BINARY), "old tt500-lcd");
        assert!(!setup.staging.join(ARCHIVE_NAME).exists());
    }

    #[test]
    fn a_new_copy_that_does_not_start_is_rolled_back() {
        let setup = installed_prefix();
        let runner = FakeRunner {
            broken: Some("tt500-lcd"),
            ..FakeRunner::default()
        };
        let up = updater(
            &setup,
            archive_http(new_archive(), ""),
            &runner,
            archive_kind(&setup),
        );
        let release = up.check().unwrap().unwrap();
        let err = up.apply(&release, &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, UpdateError::NewCopyBroken(_)), "{err}");
        for bin in [DAEMON_BINARY, CLI_BINARY, GUI_BINARY] {
            assert_eq!(bin_text(&setup, bin), format!("old {bin}"));
        }
        assert_eq!(fs::read_dir(setup.prefix.join("bin")).unwrap().count(), 3);
    }

    #[test]
    fn an_archive_missing_a_binary_changes_nothing() {
        let setup = installed_prefix();
        let runner = FakeRunner::default();
        let archive = tarball(&[("x-0.6.0/bin/tt500-lcdd", "new")]);
        let up = updater(
            &setup,
            archive_http(archive, ""),
            &runner,
            archive_kind(&setup),
        );
        let release = up.check().unwrap().unwrap();
        let err = up.apply(&release, &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, UpdateError::MissingInArchive(_)), "{err}");
        assert_eq!(bin_text(&setup, DAEMON_BINARY), "old tt500-lcdd");
        assert_eq!(fs::read_dir(setup.prefix.join("bin")).unwrap().count(), 3);
    }

    #[test]
    fn appimage_update_replaces_the_file_and_probes_it() {
        let setup = installed_prefix();
        let image = setup.prefix.join("TT500.AppImage");
        fs::write(&image, "old image").unwrap();
        let runner = FakeRunner::default();
        let name = "TT500-LCD-0.6.0-x86_64.AppImage";
        let sums = format!("{}  {name}\n", sha(b"new image"));
        let http = FakeHttp {
            routes: vec![
                (
                    "http://fake/latest".into(),
                    release_json("v0.6.0", &[(name, ""), ("SHA256SUMS", "")]),
                ),
                (format!("http://fake/{name}"), b"new image".to_vec()),
                ("http://fake/SHA256SUMS".into(), sums.into_bytes()),
            ],
        };
        let kind = InstallKind::AppImage {
            path: image.clone(),
        };
        let up = updater(&setup, http, &runner, kind);
        let release = up.check().unwrap().unwrap();
        let applied = up.apply(&release, &mut |_, _| {}).unwrap();
        assert_eq!(fs::read_to_string(&image).unwrap(), "new image");
        assert_eq!(applied.gui, Some(image.clone()));
        assert_eq!(
            runner.calls.borrow()[0],
            format!("{} --cli --version", image.display())
        );
        assert!(!image.with_extension("AppImage.part").exists());
    }

    #[test]
    fn the_automatic_check_runs_at_most_daily() {
        assert!(check_due(None, 1_000));
        assert!(!check_due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(check_due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS));
    }
}

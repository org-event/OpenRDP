//! Checks GitHub for a newer release and installs it.
//!
//! The default is to ask with a dialog and install only after confirmation.
//! `--update auto` installs without asking. `--update off` (or `--no-update`)
//! does not check. A postponed version is asked about again the next day, and
//! a newer tag is asked about straight away.
//!
//! Two channels: stable, the default, takes GitHub's latest release, which is
//! never a pre-release; beta takes the newest release of all, betas
//! (`v1.7.0-beta.1`, published as pre-releases) included. The choice is kept in
//! `update-channel` in the support directory, set from the app's window, and
//! `--update-channel` overrides it for one process. A beta build on the stable
//! channel waits for the next stable release instead of going back.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use tracing::{info, warn};

const LATEST: &str = "https://api.github.com/repos/org-event/OpenRDP/releases/latest";
/// The newest releases first, pre-releases included; drafts need a token.
const RELEASES: &str = "https://api.github.com/repos/org-event/OpenRDP/releases?per_page=30";
const DOWNLOAD_PREFIX: &str = "https://github.com/org-event/OpenRDP/releases/download/";
const FIRST_WAIT: Duration = Duration::from_secs(20);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const ASK_AGAIN_AFTER: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Auto,
    Off,
}

pub fn mode(disabled: bool, flag: Option<&str>) -> anyhow::Result<Mode> {
    if disabled {
        return Ok(Mode::Off);
    }
    match flag {
        Some("ask") => Ok(Mode::Ask),
        Some("auto") => Ok(Mode::Auto),
        Some("off") => Ok(Mode::Off),
        Some(other) => bail!("unknown --update {other} (ask, auto, off)"),
        None => Ok(mode_from_env()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
}

impl std::str::FromStr for Channel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            other => Err(format!("unknown update channel {other} (stable, beta)")),
        }
    }
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}

/// `--update-channel`, for this process only.
static CHANNEL_OVERRIDE: std::sync::OnceLock<Channel> = std::sync::OnceLock::new();

pub fn override_channel(channel: Channel) {
    let _ = CHANNEL_OVERRIDE.set(channel);
}

fn channel_path() -> anyhow::Result<PathBuf> {
    Ok(crate::tls::support_dir()?.join("update-channel"))
}

/// The channel in use: `--update-channel`, else the saved choice, else stable.
pub fn channel() -> Channel {
    if let Some(channel) = CHANNEL_OVERRIDE.get() {
        return *channel;
    }
    channel_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or(Channel::Stable)
}

/// Saves the choice; the next check uses it.
pub fn set_channel(channel: Channel) -> anyhow::Result<()> {
    let path = channel_path()?;
    if channel == Channel::Stable {
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("remove {}", path.display()));
            }
            _ => {}
        }
    } else {
        std::fs::write(&path, format!("{}\n", channel.name()))
            .with_context(|| format!("write {}", path.display()))?;
    }
    info!(channel = channel.name(), "update channel");
    Ok(())
}

fn mode_from_env() -> Mode {
    match std::env::var("OPENRDP_UPDATE").ok().as_deref() {
        Some("auto") => Mode::Auto,
        Some("off" | "0") => Mode::Off,
        _ => Mode::Ask,
    }
}

/// Checks soon after startup, then every six hours, until the process exits.
pub fn watch(mode: Mode) {
    if mode == Mode::Off {
        return;
    }
    sleep(FIRST_WAIT);
    loop {
        if let Err(err) = check_once(mode) {
            warn!(%err, "update check failed");
        }
        sleep(INTERVAL);
    }
}

/// The latest release's tag when it is newer than this build, asked now.
/// For the installed app's "Check for updates".
pub fn newer_release() -> anyhow::Result<Option<String>> {
    Ok(fetch_latest()?
        .map(|release| release.tag)
        .filter(|tag| is_newer(tag, env!("CARGO_PKG_VERSION"))))
}

/// Downloads, checks and installs the latest release into OpenRDP.app, as
/// the automatic update does, and restarts the server. Returns its tag.
pub fn install_newer() -> anyhow::Result<String> {
    let release = fetch_latest()?.context("the latest release has no Apple Silicon archive")?;
    anyhow::ensure!(
        is_newer(&release.tag, env!("CARGO_PKG_VERSION")),
        "{} is already installed",
        env!("CARGO_PKG_VERSION")
    );
    anyhow::ensure!(
        crate::service::running_from_app(),
        "only the installed OpenRDP.app updates itself"
    );
    install_release(&release)?;
    Ok(release.tag)
}

fn check_once(mode: Mode) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest()?;
    let Some(release) = release else {
        return Ok(());
    };
    if !is_newer(&release.tag, current) {
        info!(current, latest = %release.tag, "no update");
        return Ok(());
    }
    let state = State::load()?;
    let now = unix_now();
    if mode == Mode::Ask
        && state.skipped.as_deref() == Some(release.tag.as_str())
        && now.saturating_sub(state.skipped_at) < ASK_AGAIN_AFTER
    {
        info!(version = %release.tag, "update postponed");
        return Ok(());
    }
    info!(version = %release.tag, current, "update available");
    let install = match mode {
        Mode::Auto => true,
        Mode::Off => false,
        Mode::Ask => match ask(&release.tag, current)? {
            Some(true) => true,
            Some(false) => {
                State {
                    skipped: Some(release.tag),
                    skipped_at: now,
                }
                .save()?;
                info!("update postponed from the dialog");
                return Ok(());
            }
            None => {
                warn!("no dialog, so the update was not installed");
                return Ok(());
            }
        },
    };
    if install {
        install_release(&release)?;
    }
    Ok(())
}

struct Release {
    tag: String,
    archive: String,
    checksum: String,
}

/// The newest release of the channel in use.
fn fetch_latest() -> anyhow::Result<Option<Release>> {
    match channel() {
        Channel::Stable => {
            let body = curl(LATEST).context("fetch the latest release")?;
            release_from(&body)
        }
        Channel::Beta => {
            let body = curl(RELEASES).context("fetch the releases")?;
            newest_release(&body)
        }
    }
}

/// The highest version in a list of releases that has the Apple Silicon archive.
fn newest_release(json: &str) -> anyhow::Result<Option<Release>> {
    let mut best: Option<Release> = None;
    for part in release_parts(json) {
        if json_bool(part, "draft") == Some(true) {
            continue;
        }
        let Ok(Some(release)) = release_from(part) else {
            continue;
        };
        if parse_version(&release.tag).is_some()
            && best
                .as_ref()
                .is_none_or(|best| is_newer(&release.tag, &best.tag))
        {
            best = Some(release);
        }
    }
    Ok(best)
}

/// Splits a release list at each `"tag_name"`: every part holds one release's
/// tag, flags and assets, which follow the tag in GitHub's answer.
fn release_parts(json: &str) -> Vec<&str> {
    let key = "\"tag_name\"";
    let starts: Vec<usize> = json.match_indices(key).map(|(i, _)| i).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| &json[start..starts.get(n + 1).copied().unwrap_or(json.len())])
        .collect()
}

fn json_bool(json: &str, key: &str) -> Option<bool> {
    let pattern = format!("\"{key}\"");
    let rest = json.get(json.find(&pattern)? + pattern.len()..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn release_from(body: &str) -> anyhow::Result<Option<Release>> {
    let Some(tag) = json_string(body, "tag_name").map(str::to_owned) else {
        bail!("latest release has no tag");
    };
    let Some((archive, checksum)) = asset_urls(body) else {
        warn!(%tag, "latest release has no Apple Silicon archive");
        return Ok(None);
    };
    if !archive.starts_with(DOWNLOAD_PREFIX) || !checksum.starts_with(DOWNLOAD_PREFIX) {
        bail!("release download URL is not from this repository");
    }
    Ok(Some(Release {
        tag,
        archive,
        checksum,
    }))
}

fn install_release(release: &Release) -> anyhow::Result<()> {
    let version = release.tag.trim_start_matches('v');
    let dir = crate::tls::support_dir()?.join("update");
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let archive = dir.join("openrdp.tar.gz");
    curl_to(&release.archive, &archive).context("download the release")?;
    let expected = curl(&release.checksum).context("download the checksum")?;
    let expected = checksum_hex(&expected).context("read the checksum")?;
    let actual = sha256(&archive)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("checksum mismatch for {}", archive.display());
    }
    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    crate::service::run(
        Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked),
    )
    .context("unpack the release")?;
    let binary = find_binary(&unpacked)?;
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&binary)
        .status();
    if crate::service::running_from_app() {
        info!(version, "installing the update into OpenRDP.app");
        crate::service::apply_update(&binary, version)?;
    } else {
        info!(version, "installing the update and restarting");
        reinstall_detached(&binary)?;
        std::process::exit(0);
    }
    Ok(())
}

/// Starts `binary install <this process's arguments>` in its own session, after
/// a pause so this process can release the port.
fn reinstall_detached(binary: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("sleep 2; exec \"$0\" \"$@\"")
        .arg(binary)
        .arg("install");
    for arg in std::env::args().skip(1) {
        cmd.arg(arg);
    }
    cmd.env("OPENRDP_UPDATE", "off");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.process_group(0);
    cmd.spawn().context("start the installer")?;
    Ok(())
}

/// `Some(true)` install, `Some(false)` later, `None` when there is no dialog.
fn ask(tag: &str, current: &str) -> anyhow::Result<Option<bool>> {
    let version = tag.trim_start_matches('v');
    let (message, later, install) = if crate::dialog::russian() {
        (
            format!(
                "Доступен OpenRDP {version} (сейчас {current}). Подключение на минуту прервётся. Установить?"
            ),
            "Позже",
            "Установить",
        )
    } else {
        (
            format!(
                "OpenRDP {version} is available (this one is {current}). The connection drops for a moment. Install?"
            ),
            "Later",
            "Install",
        )
    };
    let script = format!(
        "display dialog {} buttons {{{}, {}}} default button {} with title \"OpenRDP\"",
        crate::dialog::apple(&message),
        crate::dialog::apple(later),
        crate::dialog::apple(install),
        crate::dialog::apple(later),
    );
    let output = Command::new("osascript").arg("-e").arg(script).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Ok(Some(answer.contains(install)))
}

fn curl(url: &str) -> anyhow::Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: openrdp",
            url,
        ])
        .output()
        .context("run curl")?;
    if !output.status.success() {
        bail!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn curl_to(url: &str, dest: &Path) -> anyhow::Result<()> {
    crate::service::run(
        Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "180",
                "-H",
                "User-Agent: openrdp",
                "-o",
            ])
            .arg(dest)
            .arg(url),
    )
}

fn sha256(path: &Path) -> anyhow::Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .context("run shasum")?;
    if !output.status.success() {
        bail!("shasum failed");
    }
    checksum_hex(&String::from_utf8(output.stdout)?).context("read shasum")
}

fn find_binary(root: &Path) -> anyhow::Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "openrdp") {
                return Ok(path);
            }
        }
    }
    bail!("release archive has no openrdp binary")
}

struct State {
    skipped: Option<String>,
    skipped_at: u64,
}

impl State {
    fn load() -> anyhow::Result<Self> {
        let path = state_path()?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut state = Self {
            skipped: None,
            skipped_at: 0,
        };
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("skipped ") {
                state.skipped = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("skipped_at ") {
                state.skipped_at = value.parse().unwrap_or(0);
            }
        }
        Ok(state)
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = state_path()?;
        let skipped = self.skipped.as_deref().unwrap_or("");
        let body = format!("skipped {skipped}\nskipped_at {}\n", self.skipped_at);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, body)?;
        Ok(())
    }
}

fn state_path() -> anyhow::Result<PathBuf> {
    Ok(crate::tls::support_dir()?.join("update.state"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let rest = json.get(json.find(&pattern)? + pattern.len()..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn asset_urls(json: &str) -> Option<(String, String)> {
    let mut archive = None;
    let mut checksum = None;
    let key = "\"browser_download_url\"";
    let mut rest = json;
    while let Some(index) = rest.find(key) {
        rest = rest.get(index + key.len()..)?;
        let value = rest.trim_start().strip_prefix(':')?.trim_start();
        let value = value.strip_prefix('"')?;
        let end = value.find('"')?;
        let url = value[..end].replace("\\/", "/");
        if url.contains("aarch64-apple-darwin.tar.gz.sha256") {
            checksum = Some(url);
        } else if url.contains("aarch64-apple-darwin.tar.gz") {
            archive = Some(url);
        }
        rest = value.get(end..)?;
    }
    Some((archive?, checksum?))
}

fn checksum_hex(text: &str) -> Option<String> {
    let hex = text.split_whitespace().next()?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_owned())
}

/// `tag` is the release (`v1.2.0` or `1.2.0`), `current` is `CARGO_PKG_VERSION`.
fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// A version as tags carry it: `1.7.0`, or `1.7.0-beta.2` before it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    numbers: (u64, u64, u64),
    /// No pre-release sorts after every pre-release of the same numbers.
    release: bool,
    /// `beta` and its number; `rc` sorts after `beta`.
    pre: (String, u64),
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim().trim_start_matches('v');
    let (numbers, pre) = match text.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (text, None),
    };
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let pre = match pre {
        None => (String::new(), 0),
        Some(pre) => {
            let (label, number) = pre.split_once('.').unwrap_or((pre, "0"));
            if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphabetic()) {
                return None;
            }
            (label.to_ascii_lowercase(), number.parse().ok()?)
        }
    };
    Some(Version {
        numbers: (major, minor, patch),
        release: pre.0.is_empty(),
        pre,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_release_compares_by_numbers() {
        assert!(is_newer("v1.2.0", "1.0.0"));
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.2.0"));
        assert!(!is_newer("not-a-version", "1.0.0"));
    }

    #[test]
    fn release_json_yields_the_apple_silicon_archive() {
        let json = r#"{"tag_name":"v1.2.0","assets":[
            {"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/v1.2.0/openrdp-v1.2.0-aarch64-apple-darwin.tar.gz.sha256"},
            {"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/v1.2.0/openrdp-v1.2.0-aarch64-apple-darwin.tar.gz"}
        ]}"#;
        assert_eq!(json_string(json, "tag_name"), Some("v1.2.0"));
        let (archive, checksum) = asset_urls(json).unwrap();
        assert!(archive.ends_with("aarch64-apple-darwin.tar.gz"));
        assert!(checksum.ends_with(".sha256"));
        assert_eq!(
            checksum_hex(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  name\n"
            )
            .as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn betas_sort_before_their_release() {
        assert!(is_newer("v1.7.0-beta.1", "1.6.9"));
        assert!(is_newer("v1.7.0-beta.2", "1.7.0-beta.1"));
        assert!(is_newer("v1.7.0-beta.10", "1.7.0-beta.9"));
        assert!(is_newer("v1.7.0-rc.1", "1.7.0-beta.3"));
        assert!(is_newer("v1.7.0", "1.7.0-beta.3"));
        assert!(!is_newer("v1.7.0-beta.1", "1.7.0"));
        // A beta on the stable channel waits for the next stable release.
        assert!(!is_newer("v1.6.9", "1.7.0-beta.1"));
        assert!(parse_version("1.7.0-").is_none());
        assert!(parse_version("1.7.0-beta.x").is_none());
    }

    #[test]
    fn the_beta_channel_takes_the_newest_release_with_an_archive() {
        let asset = |tag: &str| {
            format!(
                r#"{{"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/{tag}/openrdp-{tag}-aarch64-apple-darwin.tar.gz"}},
                {{"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/{tag}/openrdp-{tag}-aarch64-apple-darwin.tar.gz.sha256"}}"#
            )
        };
        let json = format!(
            r#"[{{"url":"a","tag_name":"v1.7.0-beta.2","draft":false,"prerelease":true,"assets":[]}},
            {{"url":"b","tag_name":"v1.7.0-beta.1","draft":false,"prerelease":true,"assets":[{}]}},
            {{"url":"c","tag_name":"v1.8.0","draft":true,"prerelease":false,"assets":[{}]}},
            {{"url":"d","tag_name":"v1.6.9","draft":false,"prerelease":false,"assets":[{}]}}]"#,
            asset("v1.7.0-beta.1"),
            asset("v1.8.0"),
            asset("v1.6.9"),
        );
        let release = newest_release(&json).unwrap().unwrap();
        assert_eq!(release.tag, "v1.7.0-beta.1");
        assert!(release.archive.contains("/v1.7.0-beta.1/"));
        assert_eq!("beta".parse::<Channel>(), Ok(Channel::Beta));
        assert!("nightly".parse::<Channel>().is_err());
    }

    /// Reads GitHub's real release list; needs the network.
    #[test]
    #[ignore]
    fn the_beta_channel_reads_github() {
        let release = newest_release(&curl(RELEASES).unwrap()).unwrap().unwrap();
        let stable = release_from(&curl(LATEST).unwrap()).unwrap().unwrap();
        assert!(
            !is_newer(&stable.tag, &release.tag),
            "{} {}",
            stable.tag,
            release.tag
        );
        assert!(release.archive.starts_with(DOWNLOAD_PREFIX));
    }

    #[test]
    fn update_mode_prefers_the_flag() {
        assert_eq!(mode(true, Some("auto")).unwrap(), Mode::Off);
        assert_eq!(mode(false, Some("auto")).unwrap(), Mode::Auto);
        assert_eq!(mode(false, Some("ask")).unwrap(), Mode::Ask);
        assert!(mode(false, Some("sometimes")).is_err());
    }
}

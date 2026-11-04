//! The login window.
//!
//! A LaunchAgent runs only in a logged-in Aqua session, so the port is closed
//! while the login window is up. `com.openrdp.broker` is a LaunchDaemon. It
//! starts at boot as root and never captures the screen or posts events itself.
//! The server itself is `com.openrdp.loginwindow`, a LaunchAgent limited to the
//! LoginWindow session: launchd starts it, as root, whenever the login window
//! is up, inside that window's GUI session. `launchctl bsexec` on `loginwindow`
//! is not enough on current macOS: such a process sees no display and powerd
//! rejects it. The server logs to, and reads its TLS identity from, the home of
//! the user who ran `install`.
//!
//! Screen Recording and Accessibility sit in the system TCC database and are
//! granted to the signed app, so the grants given after install apply here too.
//!
//! The daemon watches the owner of `/dev/console`; root means the login window.
//! The stream is the physical display. When somebody logs in, the server exits
//! (the daemon stops it if it lingers) and the per-user LaunchAgent — launchd starts it with the Aqua session —
//! binds the port. The client connects again. Logging out runs the same path
//! back. The FileVault screen before macOS boots is not a session this daemon
//! can join; RDP starts once the disk is unlocked and the login window is up.
//!
//! The server reads the RDP accounts from that home too (see `users`). The
//! password in `/Library/Application Support/openrdp/broker.conf`, mode 600,
//! root-owned, only stands in when that file is missing. The daemon plist
//! itself has no secret.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::service::{escape, run as run_command, write_private};

const LABEL: &str = "com.openrdp.broker";
const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.openrdp.broker.plist";
const AGENT_LABEL: &str = "com.openrdp.loginwindow";
const AGENT_PLIST: &str = "/Library/LaunchAgents/com.openrdp.loginwindow.plist";
const SYSTEM_DIR: &str = "/Library/Application Support/openrdp";
const CONFIG_PATH: &str = "/Library/Application Support/openrdp/broker.conf";
const PID_PATH: &str = "/Library/Application Support/openrdp/login-window.pid";
const BROKER_LOG: &str = "/Library/Logs/openrdp-broker.log";
const FORMAT_VERSION: &str = "openrdp-broker 1";

const SIGTERM: i32 = 15;
const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// Polled often enough that a login wins the port before the agent's first bind retry gives up.
const POLL: Duration = Duration::from_millis(200);
/// How long a freshly started login-window server waits for root to take the console.
const CONSOLE_WAIT: Duration = Duration::from_secs(10);

static STOP: AtomicBool = AtomicBool::new(false);
/// Pid of the login-window server, so a signal can stop it without taking a lock.
static LOGIN_PID: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" {
    fn getuid() -> u32;
    fn geteuid() -> u32;
    fn getgid() -> u32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn dup2(src: i32, dst: i32) -> i32;
    fn signal(sig: i32, handler: unsafe extern "C" fn(i32)) -> usize;
}

unsafe extern "C" fn request_stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
    let pid = LOGIN_PID.load(Ordering::Relaxed);
    if pid > 1 {
        unsafe {
            kill(-pid, SIGTERM);
            kill(pid, SIGTERM);
        }
    }
}

/// What the broker should do for this console owner. Root owns `/dev/console` at the login window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    Serve,
    Stop,
}

fn watch(console_uid: u32) -> Watch {
    if console_uid == 0 {
        Watch::Serve
    } else {
        Watch::Stop
    }
}

struct Config {
    uid: u32,
    gid: u32,
    user: String,
    home: PathBuf,
    binary: PathBuf,
    args: Vec<String>,
    password: String,
}

impl Config {
    fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut text = String::new();
        text.push_str(FORMAT_VERSION);
        text.push('\n');
        push_field(&mut text, "uid", &self.uid.to_string())?;
        push_field(&mut text, "gid", &self.gid.to_string())?;
        push_field(&mut text, "user", &self.user)?;
        push_field(&mut text, "home", path_str(&self.home)?)?;
        push_field(&mut text, "binary", path_str(&self.binary)?)?;
        for arg in &self.args {
            push_field(&mut text, "arg", arg)?;
        }
        let password = self.password.as_bytes();
        text.push_str(&format!("password {}\n", password.len()));
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(password);
        Ok(bytes)
    }

    fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut offset = 0;
        let mut password_at = None;
        let mut header = Vec::new();
        while offset < bytes.len() {
            let rest = &bytes[offset..];
            let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
                bail!("truncated broker config");
            };
            let line =
                std::str::from_utf8(&rest[..newline]).context("broker config is not utf-8")?;
            offset += newline + 1;
            if let Some(length) = line.strip_prefix("password ") {
                password_at = Some((offset, length.parse::<usize>().context("password length")?));
                break;
            }
            header.push(line.to_owned());
        }
        let Some((start, length)) = password_at else {
            bail!("broker config has no password");
        };
        let rest = &bytes[start..];
        anyhow::ensure!(
            rest.len() == length,
            "password is {} bytes, config says {length}",
            rest.len()
        );
        let password = String::from_utf8(rest.to_vec()).context("password is not utf-8")?;

        let mut lines = header.into_iter();
        let version = lines.next().context("empty broker config")?;
        anyhow::ensure!(
            version == FORMAT_VERSION,
            "unknown broker config: {version}"
        );

        let mut uid = None;
        let mut gid = None;
        let mut user = None;
        let mut home = None;
        let mut binary = None;
        let mut args = Vec::new();
        for line in lines {
            let Some((key, value)) = line.split_once(' ') else {
                bail!("broker config line has no value: {line}");
            };
            match key {
                "uid" => uid = Some(value.parse()?),
                "gid" => gid = Some(value.parse()?),
                "user" => user = Some(value.to_owned()),
                "home" => home = Some(PathBuf::from(value)),
                "binary" => binary = Some(PathBuf::from(value)),
                "arg" => args.push(value.to_owned()),
                other => bail!("unknown broker config field {other}"),
            }
        }
        Ok(Self {
            uid: uid.context("uid missing")?,
            gid: gid.context("gid missing")?,
            user: user.context("user missing")?,
            home: home.context("home missing")?,
            binary: binary.context("binary missing")?,
            args,
            password,
        })
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, self.to_bytes()?)
    }

    fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("parse {}", path.display()))
    }
}

fn push_field(text: &mut String, key: &str, value: &str) -> anyhow::Result<()> {
    if value.contains('\n') {
        bail!("{key} contains a newline");
    }
    text.push_str(key);
    text.push(' ');
    text.push_str(value);
    text.push('\n');
    Ok(())
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().context("path is not utf-8")
}

fn daemon_plist(binary: &Path) -> anyhow::Result<String> {
    let binary = escape(path_str(binary)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>broker</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>{BROKER_LOG}</string>
    <key>StandardErrorPath</key><string>{BROKER_LOG}</string>
</dict>
</plist>
"#
    ))
}

/// Launchd loads it into each LoginWindow session and starts it there as root.
/// A clean exit (somebody logged in) is final; a crash restarts it.
fn agent_plist(binary: &Path) -> anyhow::Result<String> {
    let binary = escape(path_str(binary)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{AGENT_LABEL}</string>
    <key>LimitLoadToSessionType</key><string>LoginWindow</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>login-window</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key><false/>
    </dict>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>{BROKER_LOG}</string>
    <key>StandardErrorPath</key><string>{BROKER_LOG}</string>
</dict>
</plist>
"#
    ))
}

/// Writes the broker config and installs the daemon. Asks for an administrator
/// password. A failure here leaves the per-user agent in place.
pub(crate) fn request_install(
    binary: &Path,
    server_args: &[String],
    password: &str,
) -> anyhow::Result<()> {
    // SAFETY: getuid and getgid have no preconditions.
    let (uid, gid) = unsafe { (getuid(), getgid()) };
    if uid == 0 {
        eprintln!(
            "Skipping the login-window service: run install as the user who logs in at this Mac, not as root."
        );
        return Ok(());
    }
    let config = Config {
        uid,
        gid,
        user: std::env::var("USER").context("USER is not set")?,
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?,
        binary: binary.to_owned(),
        args: server_args.to_vec(),
        password: password.to_owned(),
    };
    let staged = config
        .home
        .join("Library/Application Support/openrdp/broker.staged");
    config.save(&staged)?;
    println!("Installing the login-window service (administrator password).");
    let status = elevate(
        binary,
        Some(&staged),
        "install-system",
        crate::dialog::admin_prompt(),
    )?;
    let _ = std::fs::remove_file(&staged);
    if status.success() {
        println!(
            "The login window is served by {LABEL}. Log out and connect as usual; after you log in, connect again."
        );
    } else if crate::dialog::from_console() {
        eprintln!(
            "The login-window service was not installed. The server still starts after you log in.\n\
             Run this install again in a terminal and enter the administrator password when asked."
        );
    } else {
        let _ = crate::dialog::note(crate::dialog::broker_skipped());
    }
    Ok(())
}

/// Removes the daemon. Quiet when it was never installed.
pub(crate) fn request_uninstall(binary: &Path) -> anyhow::Result<()> {
    if !Path::new(DAEMON_PLIST).exists() {
        return Ok(());
    }
    println!("Removing the login-window service (administrator password).");
    let status = elevate(
        binary,
        None,
        "uninstall-system",
        crate::dialog::remove_prompt(),
    )?;
    if !status.success() {
        eprintln!(
            "The login-window service is still installed. Remove it with:\n  sudo {} uninstall-system",
            binary.display()
        );
    }
    Ok(())
}

/// Asks for an administrator password: in the terminal with `sudo`, otherwise
/// with the system dialog, which a double-clicked app can show.
fn elevate(
    binary: &Path,
    staged: Option<&Path>,
    command: &str,
    prompt: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    if crate::dialog::from_console() {
        let mut sudo = Command::new("/usr/bin/sudo");
        sudo.arg("--").arg(binary).arg(command);
        if let Some(staged) = staged {
            sudo.arg(staged);
        }
        return sudo.status().context("ask for an administrator password");
    }
    let mut shell = crate::dialog::shell_single(&binary.display().to_string());
    shell.push(' ');
    shell.push_str(command);
    if let Some(staged) = staged {
        shell.push(' ');
        shell.push_str(&crate::dialog::shell_single(&staged.display().to_string()));
    }
    crate::dialog::authorize(&shell, prompt).context("ask for an administrator password")
}

/// Copies the staged config into place and bootstraps the daemon. Root only.
pub(crate) fn install_system(staged: &Path) -> anyhow::Result<()> {
    require_root("install-system")?;
    let config = Config::load(staged)?;
    anyhow::ensure!(
        config.uid != 0,
        "the login-window server cannot run as root"
    );
    anyhow::ensure!(
        config.binary.is_file(),
        "missing {}",
        config.binary.display()
    );
    std::fs::create_dir_all(SYSTEM_DIR).with_context(|| format!("create {SYSTEM_DIR}"))?;
    config.save(Path::new(CONFIG_PATH))?;
    let _ = std::fs::remove_file(staged);

    let plist = daemon_plist(&config.binary)?;
    std::fs::write(DAEMON_PLIST, &plist).with_context(|| format!("write {DAEMON_PLIST}"))?;
    std::fs::set_permissions(DAEMON_PLIST, std::fs::Permissions::from_mode(0o644))?;
    // Picked up by the next login window; nothing to bootstrap from a user session.
    let agent = agent_plist(&config.binary)?;
    std::fs::write(AGENT_PLIST, &agent).with_context(|| format!("write {AGENT_PLIST}"))?;
    std::fs::set_permissions(AGENT_PLIST, std::fs::Permissions::from_mode(0o644))?;

    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    run_command(Command::new("/bin/launchctl").args(["enable", &format!("system/{LABEL}")]))
        .context("enable the login-window service")?;
    // Bootout returns before the old daemon is gone, and bootstrap fails until it is.
    let mut attempt = 1;
    loop {
        match run_command(Command::new("/bin/launchctl").args([
            "bootstrap",
            "system",
            DAEMON_PLIST,
        ])) {
            Ok(_) => break,
            Err(_) if attempt < 10 => {
                attempt += 1;
                sleep(Duration::from_millis(500));
            }
            Err(err) => return Err(err).context("start the login-window service"),
        }
    }
    println!("Installed {DAEMON_PLIST}.");
    prepare_remote_power_on(&config.binary);
    Ok(())
}

/// Settings a Mac reached only over the network needs after a power cut,
/// set while the install has root anyway, so none of them has to be found by
/// hand later. None of them fails the install: each one says how it went.
///
/// - start up again by itself when the power comes back (`autorestart`);
/// - wake for network access, so a client can wake it (`womp`);
/// - Remote Login (SSH): macOS 26 takes the FileVault password over it, and
///   OpenRDP Client's Unlock uses that;
/// - Screen Sharing, the client's fallback when the login window does not
///   come up over RDP;
/// - the application firewall lets the server accept connections, instead of
///   asking at the Mac the first time a client connects.
fn prepare_remote_power_on(binary: &Path) {
    let step = |what: &str, command: &mut Command| {
        let ok = command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        println!("{what}: {}", if ok { "on" } else { "could not turn it on" });
    };
    step(
        "Start up after a power failure",
        Command::new("/usr/bin/pmset").args(["-a", "autorestart", "1"]),
    );
    step(
        "Wake for network access",
        Command::new("/usr/bin/pmset").args(["-a", "womp", "1"]),
    );
    for (what, label, plist) in [
        (
            "Remote Login (SSH)",
            "com.openssh.sshd",
            "/System/Library/LaunchDaemons/ssh.plist",
        ),
        (
            "Screen Sharing",
            "com.apple.screensharing",
            "/System/Library/LaunchDaemons/com.apple.screensharing.plist",
        ),
    ] {
        let _ = Command::new("/bin/launchctl")
            .args(["enable", &format!("system/{label}")])
            .status();
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &format!("system/{label}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if loaded {
            println!("{what}: on");
        } else {
            step(
                what,
                Command::new("/bin/launchctl").args(["bootstrap", "system", plist]),
            );
        }
    }
    let firewall = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    if Path::new(firewall).exists() {
        let _ = Command::new(firewall).arg("--add").arg(binary).status();
        step(
            "Firewall lets OpenRDP accept connections",
            Command::new(firewall).arg("--unblockapp").arg(binary),
        );
    }
}

/// Stops the daemon and deletes its config. Root only.
pub(crate) fn uninstall_system() -> anyhow::Result<()> {
    require_root("uninstall-system")?;
    stop_login_server();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    // The firewall rule goes with the app. The power, SSH and Screen Sharing
    // settings stay: they are the Mac's, and may have been on before.
    if let Ok(config) = Config::load(Path::new(CONFIG_PATH)) {
        let _ = Command::new("/usr/libexec/ApplicationFirewall/socketfilterfw")
            .arg("--remove")
            .arg(&config.binary)
            .stdout(std::process::Stdio::null())
            .status();
    }
    for path in [AGENT_PLIST, DAEMON_PLIST, CONFIG_PATH, PID_PATH] {
        if Path::new(path).exists() {
            std::fs::remove_file(path).with_context(|| format!("remove {path}"))?;
        }
    }
    println!("Removed the login-window service.");
    Ok(())
}

fn require_root(command: &str) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { geteuid() } == 0 {
        Ok(())
    } else {
        bail!("{command} must run as root")
    }
}

/// Broker process: root, for the life of the boot. Launchd restarts it.
pub(crate) fn run() -> anyhow::Result<()> {
    require_root("broker")?;
    let config = Config::load(Path::new(CONFIG_PATH))?;
    // SAFETY: the handler only stores a flag and signals the login-window pid.
    unsafe {
        signal(SIGTERM, request_stop);
        signal(SIGINT, request_stop);
    }
    tracing::info!(user = %config.user, "watching the console");

    let mut last_uid = None;

    while !STOP.load(Ordering::Relaxed) {
        let uid = console_uid();
        if last_uid != uid {
            tracing::info!(?uid, "console user");
            last_uid = uid;
        }
        // Launchd starts the server at the login window; the broker only makes sure
        // it is gone once a user has the console.
        match uid.map(watch) {
            Some(Watch::Serve) => note_login_pid(),
            Some(Watch::Stop) => {
                if login_server_running() {
                    tracing::info!("a user is at the console; stopping the login-window server");
                }
                stop_login_server();
            }
            // A failed stat must not tear the login-window server down.
            None => {}
        }

        for _ in 0..5 {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            sleep(POLL);
        }
    }

    stop_login_server();
    tracing::info!("broker stopped");
    Ok(())
}

fn console_uid() -> Option<u32> {
    std::fs::metadata("/dev/console")
        .ok()
        .map(|meta| meta.uid())
}

/// Set up the login-window server and return its arguments.
///
/// Called by launchd's LoginWindow agent before any other thread exists. The process
/// stays root, as every LoginWindow agent does. Screen Recording and Accessibility live
/// in the system TCC database and follow the signed app, not the user.
///
/// A login ends it with a successful exit, so launchd does not start it again.
pub(crate) fn prepare_login_window() -> anyhow::Result<Vec<OsString>> {
    require_root("login-window")?;
    let config = Config::load(Path::new(CONFIG_PATH))?;
    // Our own process group, so the broker can signal the whole server.
    unsafe { setpgid(0, 0) };
    write_private(Path::new(PID_PATH), std::process::id().to_string())?;

    let home = path_str(&config.home)?.to_owned();
    let user = config.user.clone();
    let password = config.password.clone();
    // SAFETY: this runs at process start, before the runtime or any other thread.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USER", &user);
        std::env::set_var("LOGNAME", &user);
        std::env::set_var("OPENRDP_PASSWORD", &password);
        std::env::set_var("OPENRDP_LOGIN_WINDOW", "1");
    }
    if let Err(err) = std::env::set_current_dir(&home) {
        eprintln!("cannot enter {home}: {err}");
    }
    redirect_log(Path::new(&home), config.uid, config.gid);
    std::thread::Builder::new()
        .name("openrdp-console".into())
        .spawn(exit_on_login)
        .context("spawn console watcher")?;
    Ok(config.args.into_iter().map(OsString::from).collect())
}

/// Hands the port to the user's agent as soon as somebody logs in.
///
/// After a logout launchd starts this a moment before root takes `/dev/console`, so a
/// user at the console counts only once root has had it. A console that never turns
/// root (fast user switching left a user there) ends the server too.
fn exit_on_login() {
    let started = Instant::now();
    let mut seen_root = false;
    loop {
        sleep(POLL);
        match console_uid().map(watch) {
            Some(Watch::Serve) => seen_root = true,
            Some(Watch::Stop) if seen_root || started.elapsed() >= CONSOLE_WAIT => {
                eprintln!("a user is at the console; the login-window server exits");
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

pub(crate) fn login_window_requested() -> bool {
    std::env::var_os("OPENRDP_LOGIN_WINDOW").is_some_and(|value| value == "1")
}

/// Appends to the user's log; a log this root process creates is handed to the user,
/// so the LaunchAgent can still write it after login.
fn redirect_log(home: &Path, uid: u32, gid: u32) {
    let dir = home.join("Library/Logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("openrdp.log");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    if file.metadata().is_ok_and(|meta| meta.uid() == 0) {
        let _ = std::os::unix::fs::chown(&path, Some(uid), Some(gid));
    }
    let fd = file.as_raw_fd();
    unsafe {
        dup2(fd, 1);
        dup2(fd, 2);
    }
}

fn note_login_pid() {
    let pid = read_pid().filter(|pid| is_login_server(*pid)).unwrap_or(0);
    LOGIN_PID.store(pid, Ordering::Relaxed);
}

fn login_server_running() -> bool {
    read_pid().is_some_and(is_login_server)
}

fn read_pid() -> Option<i32> {
    let text = std::fs::read_to_string(PID_PATH).ok()?;
    let pid = text.trim().parse().ok()?;
    (pid > 1).then_some(pid)
}

fn is_login_server(pid: i32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).contains("login-window")
}

fn stop_login_server() {
    let Some(pid) = read_pid() else {
        LOGIN_PID.store(0, Ordering::Relaxed);
        return;
    };
    if !is_login_server(pid) {
        LOGIN_PID.store(0, Ordering::Relaxed);
        let _ = std::fs::remove_file(PID_PATH);
        return;
    }
    LOGIN_PID.store(pid, Ordering::Relaxed);
    unsafe {
        kill(-pid, SIGTERM);
        kill(pid, SIGTERM);
    }
    for _ in 0..20 {
        if !is_login_server(pid) {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    if is_login_server(pid) {
        unsafe {
            kill(-pid, SIGKILL);
            kill(pid, SIGKILL);
        }
    }
    LOGIN_PID.store(0, Ordering::Relaxed);
    let _ = std::fs::remove_file(PID_PATH);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            uid: 501,
            gid: 20,
            user: "me".into(),
            home: PathBuf::from("/Users/me"),
            binary: PathBuf::from("/Users/me/Applications/OpenRDP.app/Contents/MacOS/openrdp"),
            args: vec![
                "--user".into(),
                "rdp<user>".into(),
                "--bind".into(),
                "0.0.0.0:3389".into(),
            ],
            password: "p<&>\nsecret".into(),
        }
    }

    #[test]
    fn config_roundtrip_keeps_a_password_with_a_newline() {
        let config = sample();
        let parsed = Config::parse(&config.to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.uid, 501);
        assert_eq!(parsed.gid, 20);
        assert_eq!(parsed.user, "me");
        assert_eq!(parsed.home, PathBuf::from("/Users/me"));
        assert_eq!(parsed.args, config.args);
        assert_eq!(parsed.password, "p<&>\nsecret");
    }

    #[test]
    fn config_rejects_a_newline_in_an_argument() {
        let mut config = sample();
        config.args.push("bad\narg".into());
        assert!(config.to_bytes().is_err());
    }

    #[test]
    fn config_rejects_a_short_password() {
        let mut bytes = sample().to_bytes().unwrap();
        bytes.pop();
        assert!(Config::parse(&bytes).is_err());
    }

    #[test]
    fn login_window_is_console_root() {
        assert_eq!(watch(0), Watch::Serve);
        assert_eq!(watch(501), Watch::Stop);
    }

    #[test]
    fn agent_plist_runs_only_at_the_login_window() {
        let plist = agent_plist(Path::new("/Users/me/OpenRDP.app/Contents/MacOS/openrdp")).unwrap();
        assert!(plist.contains("<key>LimitLoadToSessionType</key><string>LoginWindow</string>"));
        assert!(plist.contains("<string>login-window</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(!plist.contains("password"));
    }

    #[test]
    fn daemon_plist_has_no_secret() {
        let plist =
            daemon_plist(Path::new("/Users/a&b/OpenRDP.app/Contents/MacOS/openrdp")).unwrap();
        assert!(plist.contains("<string>broker</string>"));
        assert!(plist.contains("com.openrdp.broker"));
        assert!(plist.contains("/Users/a&amp;b/OpenRDP.app/Contents/MacOS/openrdp"));
        assert!(!plist.contains("password"));
        assert!(!plist.contains("secret"));
    }
}

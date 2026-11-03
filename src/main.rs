//! OpenRDP: an RDP server for macOS built on IronRDP.

mod avc444;
mod broker;
mod capture;
mod clipboard;
mod clipboard_files;
mod cursor;
mod curtain;
mod dialog;
mod display;
mod encoder;
mod input;
mod input_source;
mod power;
mod rate;
mod service;
mod setup;
mod signing;
mod sound;
mod tls;
mod update;
mod users;
mod virtual_display;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ironrdp_server::{RdpServer, ServerError, ServerErrorKind};
use objc2_core_graphics::CGMainDisplayID;
use tracing::{info, warn};

use crate::display::{DisplayConfig, GfxFactory, MacDisplay, Screen, ScreenState, Shared};
use crate::input::MacInput;

const HELP: &str = "\
openrdp — RDP server for macOS

USAGE:
  openrdp [--user <NAME> --pass <PASSWORD>] [OPTIONS]
  openrdp install --user <NAME> --pass <PASSWORD> [OPTIONS]
                      run at login as ~/Applications/OpenRDP.app (LaunchAgent)
                      and serve the login window (asks for an administrator password)
  openrdp user <list|set|remove|rename>
                      the RDP accounts; see openrdp user --help
  openrdp                  without a terminal: ask for the account, then for
                           the administrator password, then Screen Recording and
                           Accessibility, and start only after both are on
  openrdp uninstall    remove the agent, the app and the login-window service

OPTIONS:
  --bind <ADDR>       listen address [default: 0.0.0.0:3389]
  --sec <hybrid|tls>  hybrid = NLA (CredSSP), tls = TLS only [default: hybrid]
  --fps <N>           capture frame rate cap [default: 60]
  --bitrate <MBPS>    average H.264 bitrate [default: 20]
  --avc444            send AVC444 to every client that negotiates it: sharper
                      text and colour edges, about twice the encoding. By
                      default only FreeRDP-based clients get it: Windows App
                      for Mac negotiates it, then drops the connection
  --no-avc444         never send AVC444
  --fixed-size        stream the main display as it is instead of a virtual
                      display sized to the client
  --max-size <WxH>    largest virtual display by area; bigger clients get a
                      proportionally smaller one they stretch. Clients that
                      resize the session to their window get up to 4096 a side
                      [default: 3072x1920]
  --keep-display <SEC>
                      keep the virtual display after a disconnect, so windows
                      stay put for a reconnect; 0 removes it at once
                      [default: 300]
  --hidpi             with --fixed-size: stream in pixels instead of points
  --curtain <client|always|never>
                      darken the Mac's own displays while a client is
                      connected: when the client asks (its alternate shell is
                      `curtain`, e.g. FreeRDP /shell:curtain), always, or
                      never. The keyboard and mouse there still work
                      [default: client]
  --no-sound          do not play the Mac's sound on the client
  --no-clipboard      do not share the clipboard (Universal Clipboard between
                      Macs on one Apple ID fights with it)
  --test-pattern      send a synthetic picture instead of the screen
  --cert <PEM> --key <PEM>
                      TLS identity [default: self-signed, kept in
                      ~/Library/Application Support/openrdp]
  --update <ask|auto|off>
                      ask before installing a newer GitHub release [default: ask];
                      auto installs it, off does not check
  --no-update         same as --update off

The accounts live in ~/Library/Application Support/openrdp/users; --user and
--pass only add the first one. Password may also come from OPENRDP_PASSWORD.
Logs: RUST_LOG=openrdp=debug.
";

struct Args {
    bind: SocketAddr,
    hybrid: bool,
    /// The first account, for a server that has none yet.
    user: Option<String>,
    pass: Option<String>,
    fps: i32,
    bitrate_mbps: u32,
    avc444: display::Avc444,
    fixed_size: bool,
    max_pixels: u32,
    keep_display: u64,
    hidpi: bool,
    test_pattern: bool,
    clipboard: bool,
    sound: bool,
    curtain: curtain::Policy,
    cert: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
    update: update::Mode,
}

/// `WxH` as a pixel count.
fn parse_area(value: &str) -> anyhow::Result<u32> {
    let (w, h) = value
        .split_once('x')
        .context("expected WxH, like 2560x1600")?;
    let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
    w.checked_mul(h)
        .filter(|&area| area > 0)
        .context("size out of range")
}

fn parse_args(args: Vec<std::ffi::OsString>) -> anyhow::Result<Option<Args>> {
    let mut args = pico_args::Arguments::from_vec(args);
    if args.contains(["-h", "--help"]) {
        return Ok(None);
    }
    let sec: String = args
        .opt_value_from_str("--sec")?
        .unwrap_or_else(|| "hybrid".into());
    let hybrid = match sec.as_str() {
        "hybrid" => true,
        "tls" => false,
        other => bail!("unknown --sec {other}"),
    };
    let pass = match args.opt_value_from_str("--pass")? {
        Some(pass) => Some(pass),
        None => std::env::var("OPENRDP_PASSWORD").ok(),
    };
    let parsed = Args {
        bind: args
            .opt_value_from_str("--bind")?
            .unwrap_or_else(|| "0.0.0.0:3389".parse().expect("valid address")),
        hybrid,
        user: args.opt_value_from_str("--user")?,
        pass,
        fps: args.opt_value_from_str("--fps")?.unwrap_or(60),
        bitrate_mbps: args.opt_value_from_str("--bitrate")?.unwrap_or(20),
        avc444: match (args.contains("--avc444"), args.contains("--no-avc444")) {
            (true, true) => bail!("--avc444 and --no-avc444 contradict each other"),
            (true, false) => display::Avc444::On,
            (false, true) => display::Avc444::Off,
            (false, false) => display::Avc444::Auto,
        },
        fixed_size: args.contains("--fixed-size"),
        max_pixels: args
            .opt_value_from_fn("--max-size", parse_area)?
            .unwrap_or(3072 * 1920),
        keep_display: args.opt_value_from_str("--keep-display")?.unwrap_or(300),
        hidpi: args.contains("--hidpi"),
        test_pattern: args.contains("--test-pattern"),
        clipboard: !args.contains("--no-clipboard"),
        sound: !args.contains("--no-sound"),
        curtain: args
            .opt_value_from_str("--curtain")?
            .unwrap_or(curtain::Policy::Client),
        cert: args.opt_value_from_str("--cert")?,
        key: args.opt_value_from_str("--key")?,
        update: {
            let disabled = args.contains("--no-update");
            let flag: Option<String> = args.opt_value_from_str("--update")?;
            update::mode(disabled, flag.as_deref())?
        },
    };
    let rest = args.finish();
    if !rest.is_empty() {
        bail!("unexpected arguments: {rest:?}");
    }
    Ok(Some(parsed))
}

/// Validates the server options, then installs the agent with them, keeping
/// the password out of its command line.
fn install(argv: Vec<std::ffi::OsString>) -> anyhow::Result<()> {
    let Some(args) = parse_args(argv.clone()).inspect_err(|_| eprintln!("{HELP}"))? else {
        print!("{HELP}");
        return Ok(());
    };
    let mut server_args = Vec::new();
    let mut argv = argv.into_iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = argv.next() {
        if arg == "--pass" {
            argv.next();
        } else if !arg.starts_with("--pass=") {
            server_args.push(arg);
        }
    }
    let (Some(user), Some(pass)) = (&args.user, &args.pass) else {
        bail!("install needs --user and --pass (or OPENRDP_PASSWORD)");
    };
    service::install(&server_args, user, pass)
}

/// The accounts the server accepts: the account file, or `--user` and `--pass`
/// for a server that has none yet, which the user agent then writes there.
fn accounts(
    path: &std::path::Path,
    user: Option<&str>,
    pass: Option<&str>,
) -> anyhow::Result<Vec<users::Account>> {
    if let Some(accounts) = users::load(path)?.filter(|accounts| !accounts.is_empty()) {
        info!(accounts = accounts.len(), "accounts");
        return Ok(accounts);
    }
    let (Some(user), Some(pass)) = (user, pass) else {
        bail!("no RDP accounts: add one with `openrdp user set <NAME>`, or pass --user and --pass");
    };
    let mut accounts = Vec::new();
    users::set(&mut accounts, user, pass)?;
    // The login-window server runs as root and would leave a file the user
    // cannot read; the user agent writes it at its first start.
    if !broker::login_window_requested() {
        users::save(path, &accounts)?;
        info!(path = %path.display(), "wrote the account file");
    }
    Ok(accounts)
}

fn main() -> anyhow::Result<()> {
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // The login-window process joins that session as root, then drops to the
    // installing user. That has to happen before any other thread exists.
    match argv.first().and_then(|arg| arg.to_str()) {
        Some("login-window") => argv = broker::prepare_login_window()?,
        Some("broker") => {
            init_tracing();
            return broker::run();
        }
        Some("install-system") => {
            init_tracing();
            let path = argv
                .get(1)
                .context("install-system needs the staged config path")?;
            return broker::install_system(std::path::Path::new(path));
        }
        Some("uninstall-system") => {
            init_tracing();
            return broker::uninstall_system();
        }
        Some("uninstall") => {
            init_tracing();
            return service::uninstall();
        }
        Some("install") => {
            init_tracing();
            argv.remove(0);
            return install(argv);
        }
        Some("user") => {
            argv.remove(0);
            return users::run(argv);
        }
        Some("--setup") => {
            init_tracing();
            return setup::run();
        }
        _ => {
            if argv.is_empty() && !dialog::from_console() {
                init_tracing();
                return gui_install();
            }
        }
    }

    init_tracing();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("runtime")?;
    runtime.block_on(serve(argv))
}

/// First launch of the app, with nobody at a terminal to type the flags.
fn gui_install() -> anyhow::Result<()> {
    if !service::installed() {
        let Some(user) = dialog::ask(dialog::account_prompt(), false, &console_user())? else {
            return Ok(());
        };
        let Some(pass) = dialog::ask(dialog::password_prompt(), true, "")? else {
            return Ok(());
        };
        if user.is_empty() || pass.is_empty() {
            return dialog::note(dialog::need_account());
        }
        dialog::note(dialog::admin_coming())?;
        service::install_waiting(&["--user".into(), user.clone()], &user, &pass)?;
    }
    handoff_setup()
}

/// Permission prompts belong to the signed app. A copy dragged from the disk
/// image only collects the account, then hands the rest over.
fn handoff_setup() -> anyhow::Result<()> {
    if service::running_from_app() {
        return setup::run();
    }
    let status = std::process::Command::new(service::installed_binary()?)
        .arg("--setup")
        .status()
        .context("continue the install")?;
    if status.success() {
        Ok(())
    } else {
        bail!("the install did not finish")
    }
}

fn console_user() -> String {
    std::env::var("USER").unwrap_or_default()
}

fn init_tracing() {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openrdp=info,warn".into()),
        )
        .init();
}

async fn serve(argv: Vec<std::ffi::OsString>) -> anyhow::Result<()> {
    let Some(mut args) = parse_args(argv).inspect_err(|_| eprintln!("{HELP}"))? else {
        print!("{HELP}");
        return Ok(());
    };
    // No virtual display and no update dialog: the login window is the physical
    // screen, and nobody is there to answer a prompt. The pasteboard is not the
    // user's yet.
    if broker::login_window_requested() {
        args.fixed_size = true;
        args.clipboard = false;
        args.sound = false;
        args.curtain = curtain::Policy::Never;
        args.update = update::Mode::Off;
        info!("login window");
    }

    if !args.test_pattern {
        check_permissions();
    }

    let screen = Screen::of_display(CGMainDisplayID(), args.hidpi)?;
    let config = DisplayConfig {
        fps: args.fps,
        bitrate: args.bitrate_mbps.saturating_mul(1_000_000),
        avc444: if args.test_pattern {
            display::Avc444::Off
        } else {
            args.avc444
        },
        hidpi: args.hidpi,
        match_client: !args.fixed_size,
        curtain: args.curtain,
        max_pixels: args.max_pixels,
        keep_display: std::time::Duration::from_secs(args.keep_display),
        test_pattern: args.test_pattern,
    };
    info!(?screen, match_client = config.match_client, "display");

    let identity = tls::identity(args.cert.as_deref(), args.key.as_deref())?;
    let acceptor = identity.make_acceptor().context("TLS acceptor")?;

    let screen = Arc::new(ScreenState::new(screen));
    let advanced_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let curtain_asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = Arc::new(Shared::new(
        Arc::clone(&screen),
        Arc::clone(&advanced_input),
        Arc::clone(&curtain_asked),
    ));
    let builder = RdpServer::builder().with_addr(args.bind);
    let builder = if args.hybrid {
        builder.with_hybrid(acceptor, identity.pub_key.clone())
    } else {
        builder.with_tls(acceptor)
    };
    let _awake = power::Awake::system();
    let mut server = builder
        .with_input_handler(MacInput::new(screen, advanced_input))
        .with_display_handler(MacDisplay::new(config, Arc::clone(&shared)))
        .with_gfx_factory(Some(Box::new(GfxFactory::new(shared))))
        .with_cliprdr_factory(
            args.clipboard
                .then(|| Box::new(clipboard::MacClipboardFactory::default()) as Box<_>),
        )
        .with_connection_handler(Some(Box::new(curtain::ClientRequest(curtain_asked))))
        .with_sound_factory(
            args.sound
                .then(|| Box::new(sound::MacSoundFactory::default()) as Box<_>),
        )
        .build();
    let accounts_path = users::path()?;
    let accounts = accounts(&accounts_path, args.user.as_deref(), args.pass.as_deref())?;
    server.set_credential_list(users::credentials(&accounts));
    users::watch(accounts_path, server.event_sender().clone());

    info!(bind = %args.bind, hybrid = args.hybrid, "listening");
    let update = args.update;
    tokio::task::spawn_blocking(move || update::watch(update));
    // The login-window process and the user agent overlap for a moment at login
    // and logout. Wait out "address already in use" instead of exiting: launchd
    // would then pause the agent for its throttle interval.
    let mut logged = false;
    for _ in 0..40 {
        match server.run().await {
            Ok(()) => return Ok(()),
            Err(err) if is_addr_in_use(&err) => {
                if !logged {
                    warn!(bind = %args.bind, "port is in use, waiting for it");
                    logged = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(err) => return Err(err).context("server"),
        }
    }
    bail!("port {} stayed in use", args.bind)
}

fn is_addr_in_use(err: &ServerError) -> bool {
    matches!(
        err.kind(),
        ServerErrorKind::Io(io) if io.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// Logs both grants on every start (`openrdp-doctor` reads that line) and asks
/// for the missing ones. The graphical install has already walked through them;
/// a terminal install still hits these prompts.
fn check_permissions() {
    let screen_recording = setup::capture_granted();
    if !screen_recording {
        warn!("Screen Recording permission missing; requesting it (restart after granting)");
        setup::request_capture();
    }
    // Asking with the prompt option also adds the app that launched us to the
    // Accessibility list, so the user only has to switch it on.
    let accessibility = setup::accessibility_granted(true);
    if !accessibility {
        warn!(
            "Accessibility permission missing; requested it (restart after granting): remote keyboard and mouse will be ignored"
        );
    }
    // What CGEventPost checks; Accessibility normally implies it.
    let post_events = objc2_core_graphics::CGPreflightPostEventAccess();
    info!(screen_recording, accessibility, post_events, "permissions");
}

#[cfg(test)]
mod tests {
    use ironrdp_server::ServerErrorExt as _;

    use super::*;

    #[test]
    fn only_a_busy_port_is_retried() {
        let busy = ServerError::io(
            "bind listen address",
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use"),
        );
        let denied = ServerError::io(
            "bind listen address",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no"),
        );
        assert!(is_addr_in_use(&busy));
        assert!(!is_addr_in_use(&denied));
    }
}

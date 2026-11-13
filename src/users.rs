//! RDP accounts, apart from the macOS ones.
//!
//! They live in `~/Library/Application Support/openrdp/users`, mode 600, in
//! the home of the user who installed the server. The user agent reads it, and
//! so does the login-window server, which runs as root with that home. NLA
//! (CredSSP with NTLM) checks the password itself, so the file holds it in the
//! clear and its mode is what protects it, as the agent plist did before.
//!
//! The server reloads the file when it changes: a new account or password
//! works from the next connection, and an open session keeps running.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, bail, ensure};
use ironrdp_server::{Credentials, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

const FORMAT_VERSION: &str = "openrdp-users 1";
/// How often the server looks for a changed account file.
const WATCH: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub password: String,
}

pub fn path() -> anyhow::Result<PathBuf> {
    Ok(crate::tls::support_dir()?.join("users"))
}

/// The accounts in `path`, or `None` when there is no such file.
pub fn load(path: &Path) -> anyhow::Result<Option<Vec<Account>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text)
            .map(Some)
            .with_context(|| format!("read {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

pub fn save(path: &Path, accounts: &[Account]) -> anyhow::Result<()> {
    crate::service::write_private(path, format(accounts)?)
}

fn format(accounts: &[Account]) -> anyhow::Result<String> {
    let mut text = format!("{FORMAT_VERSION}\n");
    for account in accounts {
        check_name(&account.name)?;
        check_password(&account.password)?;
        text.push_str(&format!("{}\t{}\n", account.name, account.password));
    }
    Ok(text)
}

fn parse(text: &str) -> anyhow::Result<Vec<Account>> {
    let mut lines = text.lines();
    let version = lines.next().context("empty account file")?;
    ensure!(version == FORMAT_VERSION, "unknown account file: {version}");
    let mut accounts: Vec<Account> = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, password) = line
            .split_once('\t')
            .context("account line has no password")?;
        check_name(name)?;
        ensure!(
            !accounts.iter().any(|a| same_name(&a.name, name)),
            "account {name} is listed twice"
        );
        accounts.push(Account {
            name: name.to_owned(),
            password: password.to_owned(),
        });
    }
    Ok(accounts)
}

/// Windows user names ignore case, and so do the clients.
fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn check_name(name: &str) -> anyhow::Result<()> {
    ensure!(!name.is_empty(), "the account name is empty");
    ensure!(
        !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '@' | '/' | ':')),
        "account name {name:?} has a space or one of \\ @ / :"
    );
    Ok(())
}

fn check_password(password: &str) -> anyhow::Result<()> {
    ensure!(!password.is_empty(), "the password is empty");
    ensure!(
        !password.contains(['\n', '\r']),
        "the password has a line break"
    );
    Ok(())
}

/// Removes the account `name`. The last one stays, or nobody could connect.
pub fn remove(accounts: &mut Vec<Account>, name: &str) -> anyhow::Result<()> {
    let index = accounts
        .iter()
        .position(|a| same_name(&a.name, name))
        .with_context(|| format!("there is no account {name}"))?;
    ensure!(
        accounts.len() > 1,
        "{name} is the last account; add another first"
    );
    accounts.remove(index);
    Ok(())
}

/// Renames the account `old` to `new`, keeping its password.
pub fn rename(accounts: &mut [Account], old: &str, new: &str) -> anyhow::Result<()> {
    check_name(new)?;
    ensure!(
        same_name(old, new) || !accounts.iter().any(|a| same_name(&a.name, new)),
        "there is already an account {new}"
    );
    let account = accounts
        .iter_mut()
        .find(|a| same_name(&a.name, old))
        .with_context(|| format!("there is no account {old}"))?;
    account.name = new.to_owned();
    Ok(())
}

/// Adds the account, or changes its password; `true` when it is new.
pub fn set(accounts: &mut Vec<Account>, name: &str, password: &str) -> anyhow::Result<bool> {
    check_name(name)?;
    check_password(password)?;
    if let Some(account) = accounts.iter_mut().find(|a| same_name(&a.name, name)) {
        account.password = password.to_owned();
        return Ok(false);
    }
    accounts.push(Account {
        name: name.to_owned(),
        password: password.to_owned(),
    });
    Ok(true)
}

pub fn credentials(accounts: &[Account]) -> Vec<Credentials> {
    accounts
        .iter()
        .map(|account| Credentials {
            username: account.name.clone(),
            password: account.password.clone(),
            domain: None,
        })
        .collect()
}

/// Hands the server the accounts again whenever the file changes.
pub fn watch(path: PathBuf, events: UnboundedSender<ServerEvent>) {
    tokio::spawn(async move {
        let modified =
            |path: &Path| -> Option<SystemTime> { std::fs::metadata(path).ok()?.modified().ok() };
        let mut seen = modified(&path);
        loop {
            tokio::time::sleep(WATCH).await;
            let now = modified(&path);
            if now == seen {
                continue;
            }
            seen = now;
            match load(&path) {
                Ok(Some(accounts)) if !accounts.is_empty() => {
                    info!(accounts = accounts.len(), "accounts changed");
                    if events
                        .send(ServerEvent::SetCredentialList(credentials(&accounts)))
                        .is_err()
                    {
                        return;
                    }
                }
                // Keep the accounts in use rather than lock everybody out.
                Ok(_) => warn!(path = %path.display(), "no accounts left; keeping the old ones"),
                Err(err) => warn!(%err, "cannot read the accounts; keeping the old ones"),
            }
        }
    });
}

pub const HELP: &str = "\
openrdp user — RDP accounts (not macOS accounts)

USAGE:
  openrdp user list            the account names
  openrdp user set <NAME>      add the account or change its password
  openrdp user remove <NAME>   remove the account; the last one stays
  openrdp user rename <OLD> <NEW>

The password comes from --pass, OPENRDP_PASSWORD, or a prompt. Changes apply
to the next connection; an open session keeps running.
";

/// `openrdp user …`
pub fn run(argv: Vec<std::ffi::OsString>) -> anyhow::Result<()> {
    let mut args = pico_args::Arguments::from_vec(argv);
    if args.contains(["-h", "--help"]) {
        print!("{HELP}");
        return Ok(());
    }
    let pass: Option<String> = args.opt_value_from_str("--pass")?;
    let command: Option<String> = args.opt_free_from_str()?;
    let path = path()?;
    let mut accounts = load(&path)?.unwrap_or_default();
    match command.as_deref() {
        Some("list") | None => {
            if accounts.is_empty() {
                println!("No accounts. Add one: openrdp user set <NAME>");
            }
            for account in &accounts {
                println!("{}", account.name);
            }
            return Ok(());
        }
        Some("set") => {
            let name: String = args.free_from_str().context("which account?")?;
            check_name(&name)?;
            let password = match pass.or_else(|| std::env::var("OPENRDP_PASSWORD").ok()) {
                Some(password) => password,
                None => prompt_password(&name)?,
            };
            let new = set(&mut accounts, &name, &password)?;
            save(&path, &accounts)?;
            println!(
                "{} {name}.",
                if new {
                    "Added"
                } else {
                    "Changed the password of"
                }
            );
        }
        Some("remove") => {
            let name: String = args.free_from_str().context("which account?")?;
            remove(&mut accounts, &name)?;
            save(&path, &accounts)?;
            println!("Removed {name}.");
        }
        Some("rename") => {
            let old: String = args.free_from_str().context("which account?")?;
            let new: String = args.free_from_str().context("the new name?")?;
            rename(&mut accounts, &old, &new)?;
            save(&path, &accounts)?;
            println!("Renamed {old} to {new}.");
        }
        Some(other) => {
            eprint!("{HELP}");
            bail!("unknown command {other}");
        }
    }
    let rest = args.finish();
    ensure!(rest.is_empty(), "unexpected arguments: {rest:?}");
    Ok(())
}

/// Asks twice on the terminal, without echo.
fn prompt_password(name: &str) -> anyhow::Result<String> {
    ensure!(
        std::io::stdin().is_terminal(),
        "no terminal to ask for the password; use --pass or OPENRDP_PASSWORD"
    );
    let first = read_hidden(&format!("Password for {name}: "))?;
    let second = read_hidden("Again: ")?;
    ensure!(first == second, "the passwords differ");
    Ok(first)
}

fn read_hidden(prompt: &str) -> anyhow::Result<String> {
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(std::process::Stdio::inherit())
            .status()
    };
    print!("{prompt}");
    std::io::stdout().flush()?;
    stty("-echo")?;
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    stty("echo")?;
    println!();
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

#[cfg(test)]
#[path = "tests/users.rs"]
mod tests;

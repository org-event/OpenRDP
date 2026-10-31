//! Dialogs for an install that was not started from a terminal.
//!
//! A double-clicked app has no console, so `sudo` cannot ask for a password.
//! The account is collected with `display dialog`, and the login-window service
//! is installed with the system administrator prompt.

use std::io::IsTerminal as _;
use std::process::Command;

use anyhow::Context as _;

/// Whether this process can talk to the person who started it in a terminal.
pub fn from_console() -> bool {
    std::io::stdin().is_terminal()
}

pub fn russian() -> bool {
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output();
    if let Ok(output) = output
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with("ru")
    {
        return true;
    }
    std::env::var("LANG").is_ok_and(|lang| lang.starts_with("ru"))
}

/// The command shown in the install window so it can be copied into Terminal.
pub const QUARANTINE_COMMAND: &str = "xattr -d com.apple.quarantine openrdp";

/// AppleScript string literal.
pub fn apple(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Shell single quotes, so a path can sit inside `do shell script`.
pub fn shell_single(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn unlock_reminder() -> &'static str {
    if russian() {
        "Снимите блокировку OpenRDP.\n\nОткройте Терминал и вставьте команду ниже.\nВведите пароль."
    } else {
        "Please unlock OpenRDP.\n\nOpen Terminal and paste the command below.\nEnter your password."
    }
}

pub fn account_prompt() -> &'static str {
    if russian() {
        "Имя для входа по RDP"
    } else {
        "Name for signing in over RDP"
    }
}

pub fn password_prompt() -> &'static str {
    if russian() {
        "Пароль для входа по RDP"
    } else {
        "Password for signing in over RDP"
    }
}

pub fn admin_prompt() -> &'static str {
    if russian() {
        "OpenRDP один раз просит пароль администратора, чтобы после перезагрузки можно было войти в окне входа."
    } else {
        "OpenRDP asks for an administrator password once, so a restart still reaches the login window."
    }
}

pub fn remove_prompt() -> &'static str {
    if russian() {
        "OpenRDP удалит службу окна входа."
    } else {
        "OpenRDP will remove the login-window service."
    }
}

pub fn already_installed() -> &'static str {
    if russian() {
        "OpenRDP уже установлен и запускается при входе."
    } else {
        "OpenRDP is already installed and starts at login."
    }
}

pub fn broker_skipped() -> &'static str {
    if russian() {
        "Служба окна входа не установлена. Сервер запустится после входа в систему."
    } else {
        "The login-window service was not installed. The server still starts after you log in."
    }
}

pub fn grant_permissions() -> &'static str {
    if russian() {
        "OpenRDP установлен. Разрешите запись экрана и универсальный доступ для OpenRDP, затем перезапустите его."
    } else {
        "OpenRDP is installed. Allow Screen Recording and Accessibility for OpenRDP, then restart it."
    }
}

pub fn need_account() -> &'static str {
    if russian() {
        "Нужны имя и пароль."
    } else {
        "A name and a password are required."
    }
}

/// First install window: the quarantine command is selected so it can be copied.
/// `false` when the window is closed.
pub fn remind_unlock() -> anyhow::Result<bool> {
    Ok(ask(unlock_reminder(), false, QUARANTINE_COMMAND)?.is_some())
}

/// `display dialog` answer, or `None` when the dialog was cancelled.
pub fn ask(prompt: &str, hidden: bool, default_answer: &str) -> anyhow::Result<Option<String>> {
    let script = ask_script(prompt, hidden, default_answer);
    let output = osascript(&script)?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(Some(text_returned(&text).unwrap_or_default().to_owned()))
}

pub fn note(message: &str) -> anyhow::Result<()> {
    let script = format!(
        "display dialog {} buttons {{\"OK\"}} default button \"OK\" with title \"OpenRDP\"",
        apple(message)
    );
    let _ = osascript(&script)?;
    Ok(())
}

/// Runs `command` as root. The system shows its own password dialog.
pub fn authorize(command: &str, prompt: &str) -> anyhow::Result<std::process::ExitStatus> {
    let output = osascript(&admin_script(command, prompt))?;
    Ok(output.status)
}

pub fn ask_script(prompt: &str, hidden: bool, default_answer: &str) -> String {
    let hidden = if hidden { " with hidden answer" } else { "" };
    format!(
        "display dialog {} default answer {}{hidden} with title \"OpenRDP\"",
        apple(prompt),
        apple(default_answer),
    )
}

pub fn admin_script(command: &str, prompt: &str) -> String {
    format!(
        "do shell script {} with prompt {} with administrator privileges",
        apple(command),
        apple(prompt),
    )
}

/// The text field from `osascript`'s `button returned:…, text returned:…` line.
pub fn text_returned(output: &str) -> Option<&str> {
    let line = output.lines().next()?;
    let value = line.split_once("text returned:")?.1.trim();
    Some(value)
}

fn osascript(script: &str) -> anyhow::Result<std::process::Output> {
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("show a dialog")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_answer_is_the_text_field() {
        assert_eq!(
            text_returned("button returned:OK, text returned:ivan\n"),
            Some("ivan")
        );
        assert_eq!(
            text_returned("button returned:OK, text returned:\n"),
            Some("")
        );
    }

    #[test]
    fn password_dialog_hides_the_answer() {
        let script = ask_script("Пароль", true, "");
        assert!(script.contains("with hidden answer"));
        assert!(script.contains("\"Пароль\""));
        assert!(!ask_script("Имя", false, "ivan").contains("hidden"));
    }

    #[test]
    fn admin_dialog_is_the_system_prompt() {
        let script = admin_script(
            "'/Apps/OpenRDP.app/Contents/MacOS/openrdp' install-system '/tmp/staged'",
            "просит пароль",
        );
        assert!(script.contains("with administrator privileges"));
        assert!(script.contains("with prompt \"просит пароль\""));
        assert!(script.contains("do shell script \"'/Apps/OpenRDP.app/Contents/MacOS/openrdp' install-system '/tmp/staged'\""));
    }

    #[test]
    fn unlock_window_offers_the_quarantine_command() {
        let script = ask_script(unlock_reminder(), false, QUARANTINE_COMMAND);
        assert!(
            script.contains("Please unlock OpenRDP.")
                || script.contains("Снимите блокировку OpenRDP.")
        );
        assert!(script.contains("xattr -d com.apple.quarantine openrdp"));
        assert!(script.contains("\\n"));
        assert_eq!(apple("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn shell_quotes_survive_an_apostrophe() {
        assert_eq!(
            shell_single("/Users/o'brien/OpenRDP.app"),
            "'/Users/o'\\''brien/OpenRDP.app'"
        );
    }
}

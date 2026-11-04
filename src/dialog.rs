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

pub fn broker_skipped() -> &'static str {
    if russian() {
        "Служба окна входа не установлена. Сервер запустится после входа в систему."
    } else {
        "The login-window service was not installed. The server still starts after you log in."
    }
}

pub fn admin_coming() -> &'static str {
    if russian() {
        "Сейчас macOS спросит пароль администратора этого Mac. Он нужен один раз: чтобы после перезагрузки можно было подключиться ещё на экране входа, и чтобы Mac сам включался после пропадания питания, просыпался по сети и пускал по SSH и через «Общий экран» — так OpenRDP Client разблокирует FileVault. Брандмауэр разрешит OpenRDP входящие подключения."
    } else {
        "macOS will now ask for this Mac's administrator password. It is needed once: so a restart can still be reached at the login window, and so the Mac starts up by itself after a power cut, wakes for network access, and takes SSH and Screen Sharing, which OpenRDP Client uses to unlock FileVault. The firewall will let OpenRDP accept connections."
    }
}

pub fn next_button() -> &'static str {
    if russian() {
        "Дальше"
    } else {
        "Continue"
    }
}

pub fn check_button() -> &'static str {
    if russian() {
        "Проверить"
    } else {
        "Check"
    }
}

fn cancel_button() -> &'static str {
    if russian() { "Отмена" } else { "Cancel" }
}

pub fn need_account() -> &'static str {
    if russian() {
        "Нужны имя и пароль."
    } else {
        "A name and a password are required."
    }
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

/// `true` when the continue button was pressed. Cancel and a closed dialog are
/// `false`.
pub fn proceed(message: &str, button: &str) -> anyhow::Result<bool> {
    let output = osascript(&proceed_script(message, cancel_button(), button))?;
    Ok(output.status.success())
}

pub fn proceed_script(message: &str, cancel: &str, button: &str) -> String {
    format!(
        "display dialog {} buttons {{{}, {}}} default button {} cancel button {} with title \"OpenRDP\"",
        apple(message),
        apple(cancel),
        apple(button),
        apple(button),
        apple(cancel),
    )
}

/// A dialog with up to three buttons, the last one the default; the button
/// pressed, or `None` when it was closed.
pub fn choose_button(message: &str, buttons: &[&str]) -> anyhow::Result<Option<String>> {
    let output = osascript(&buttons_script(message, buttons))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(button_returned(&text).map(str::to_owned))
}

pub fn buttons_script(message: &str, buttons: &[&str]) -> String {
    let list = buttons
        .iter()
        .map(|b| apple(b))
        .collect::<Vec<_>>()
        .join(", ");
    let default = buttons.last().map_or_else(String::new, |b| apple(b));
    format!(
        "display dialog {} buttons {{{list}}} default button {default} with title \"OpenRDP\"",
        apple(message)
    )
}

/// The item picked from `items`, or `None` when cancelled.
pub fn choose_item(prompt: &str, items: &[String], ok: &str) -> anyhow::Result<Option<String>> {
    let output = osascript(&list_script(prompt, items, ok, cancel_button()))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || text.is_empty() || text == "false" {
        return Ok(None);
    }
    Ok(Some(text))
}

pub fn list_script(prompt: &str, items: &[String], ok: &str, cancel: &str) -> String {
    let list = items
        .iter()
        .map(|i| apple(i))
        .collect::<Vec<_>>()
        .join(", ");
    let first = items.first().map_or_else(String::new, |i| apple(i));
    format!(
        "choose from list {{{list}}} with title \"OpenRDP\" with prompt {} default items {{{first}}} OK button name {} cancel button name {}",
        apple(prompt),
        apple(ok),
        apple(cancel),
    )
}

/// The button from `osascript`'s `button returned:…` line.
pub fn button_returned(output: &str) -> Option<&str> {
    let line = output.lines().next()?;
    let value = line.split_once("button returned:")?.1;
    Some(
        value
            .split(", text returned:")
            .next()
            .unwrap_or(value)
            .trim(),
    )
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
    fn buttons_and_lists_read_back() {
        assert_eq!(
            button_returned("button returned:Изменить…\n"),
            Some("Изменить…")
        );
        assert_eq!(
            button_returned("button returned:OK, text returned:ivan\n"),
            Some("OK")
        );
        let script = buttons_script("Аккаунты", &["Добавить…", "Готово"]);
        assert!(script.contains("buttons {\"Добавить…\", \"Готово\"} default button \"Готово\""));
        let list = list_script(
            "Какой?",
            &["ivan".into(), "sergey".into()],
            "Выбрать",
            "Отмена",
        );
        assert!(list.contains("choose from list {\"ivan\", \"sergey\"}"));
        assert!(list.contains("default items {\"ivan\"}"));
    }

    #[test]
    fn apple_escapes_a_newline() {
        assert_eq!(apple("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn proceed_dialog_defaults_to_the_continue_button() {
        let script = proceed_script("Включите переключатель", "Отмена", "Проверить");
        assert!(script.contains("buttons {\"Отмена\", \"Проверить\"}"));
        assert!(script.contains("default button \"Проверить\""));
        assert!(script.contains("cancel button \"Отмена\""));
        assert!(script.contains("with title \"OpenRDP\""));
    }

    #[test]
    fn shell_quotes_survive_an_apostrophe() {
        assert_eq!(
            shell_single("/Users/o'brien/OpenRDP.app"),
            "'/Users/o'\\''brien/OpenRDP.app'"
        );
    }
}

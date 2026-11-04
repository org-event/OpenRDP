//! The graphical install, one window at a time.
//!
//! Screen Recording has a system Allow button. Accessibility does not: macOS
//! only adds OpenRDP to the list, and the switch starts off. Each window says
//! which of those it is, and the server stays stopped until both are actually on.

use std::process::Command;

use crate::dialog;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: Option<&objc2_core_foundation::CFDictionary>)
    -> bool;
}

pub fn run() -> anyhow::Result<()> {
    let ru = dialog::russian();
    if capture_granted() && accessibility_granted(false) {
        let started = !crate::service::agent_running();
        if started {
            crate::service::start_agent()?;
        }
        return manage_accounts(ru, if started { done(ru) } else { already_ready(ru) });
    }

    let was_running = crate::service::agent_running();
    let mut ask = |message: &str, button: &str| dialog::proceed(message, button);
    if !guide_screen(ru, &mut ask)? || !guide_accessibility(ru, &mut ask)? {
        return dialog::note(cancelled(ru));
    }
    if ask(folders_intro(ru), dialog::next_button())? {
        ask_for_folders();
    }
    if was_running {
        crate::service::restart_agent()?;
    } else {
        crate::service::start_agent()?;
    }
    manage_accounts(ru, done(ru))
}

fn guide_screen(
    ru: bool,
    ask: &mut impl FnMut(&str, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if capture_granted() {
        return Ok(true);
    }
    let mut first = true;
    until_granted(
        screen_intro(ru),
        dialog::next_button(),
        screen_still(ru),
        dialog::check_button(),
        || {
            if !first {
                open_privacy("Privacy_ScreenCapture");
            }
            first = false;
            request_capture();
        },
        capture_granted,
        ask,
    )
}

fn guide_accessibility(
    ru: bool,
    ask: &mut impl FnMut(&str, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if accessibility_granted(false) {
        return Ok(true);
    }
    let mut first = true;
    until_granted(
        accessibility_intro(ru),
        dialog::next_button(),
        accessibility_still(ru),
        dialog::check_button(),
        || {
            if first {
                let _ = accessibility_granted(true);
            } else {
                open_privacy("Privacy_Accessibility");
            }
            first = false;
        },
        || accessibility_granted(false),
        ask,
    )
}

/// Asks `intro` once, nudges, and repeats `still` until `granted` or cancel.
pub(crate) fn until_granted(
    intro: &str,
    intro_button: &str,
    still: &str,
    still_button: &str,
    mut nudge: impl FnMut(),
    mut granted: impl FnMut() -> bool,
    ask: &mut impl FnMut(&str, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if !ask(intro, intro_button)? {
        return Ok(false);
    }
    nudge();
    if granted() {
        return Ok(true);
    }
    loop {
        nudge();
        if !ask(still, still_button)? {
            return Ok(false);
        }
        if granted() {
            return Ok(true);
        }
    }
}

/// Files copied in Finder are read when the client pastes them, so macOS
/// would ask for Desktop, Documents and Downloads at that moment, at the Mac,
/// in the middle of a remote session. Listing each folder now asks while
/// somebody is at the Mac; the answer is kept.
fn ask_for_folders() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    for folder in ["Desktop", "Documents", "Downloads"] {
        let _ = std::fs::read_dir(home.join(folder)).map(|mut entries| entries.next());
    }
}

/// Whether Screen Recording is on for this app. macOS answers a running
/// process from what it saw at launch, so a switch turned on during the
/// install reads as off until the app restarts: ask a new process instead.
pub(crate) fn capture_granted() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return capture_granted_here();
    };
    Command::new(exe)
        .arg("--check-capture")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or_else(|_| capture_granted_here(), |status| status.success())
}

/// This process's own view, fixed at its launch.
pub(crate) fn capture_granted_here() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

/// The RDP accounts, with buttons to add, change and remove them. Changes
/// apply from the next connection; the server rereads the file.
fn manage_accounts(ru: bool, status: &str) -> anyhow::Result<()> {
    let path = crate::users::path()?;
    let mut problem: Option<String> = None;
    loop {
        let accounts = crate::users::load(&path)?.unwrap_or_default();
        let names: Vec<String> = accounts.iter().map(|a| a.name.clone()).collect();
        let mut message = format!("{status}\n\n{}", accounts_heading(ru));
        for name in &names {
            message.push_str(&format!("\n•  {name}"));
        }
        if names.is_empty() {
            message.push_str(if ru { "\n(нет)" } else { "\n(none)" });
        }
        if let Some(problem) = problem.take() {
            message.push_str(&format!("\n\n⚠️ {problem}"));
        }
        let (done_b, accounts_b, check_b, more_b) = if ru {
            ("Готово", "Аккаунты…", "Проверить снова", "Ещё…")
        } else {
            ("Done", "Accounts…", "Check again", "More…")
        };
        let Some(pressed) = dialog::choose_button(&message, &[more_b, accounts_b, done_b])? else {
            return Ok(());
        };
        let result = if pressed == accounts_b {
            pick_account(ru, &path, &names)
        } else if pressed == more_b {
            let check = if ru {
                "Проверить сервер"
            } else {
                "Check the server"
            };
            let remove = if ru {
                "Удалить OpenRDP с этого Mac…"
            } else {
                "Remove OpenRDP from this Mac…"
            };
            let ok = if ru { "Выбрать" } else { "Choose" };
            let prompt = if ru {
                "Что сделать?"
            } else {
                "What to do?"
            };
            match dialog::choose_item(prompt, &[check.to_owned(), remove.to_owned()], ok)? {
                Some(item) if item == check => show_checkup(ru, check_b, done_b),
                Some(item) if item == remove => match remove_everything(ru) {
                    Ok(true) => return Ok(()),
                    other => other.map(|_| ()),
                },
                _ => Ok(()),
            }
        } else {
            return Ok(());
        };
        if let Err(err) = result {
            problem = Some(format!("{err:#}"));
        }
    }
}

fn add_account(ru: bool, path: &std::path::Path) -> anyhow::Result<()> {
    let Some(name) = dialog::ask(dialog::account_prompt(), false, "")? else {
        return Ok(());
    };
    let Some(password) = ask_new_password(ru, &name)? else {
        return Ok(());
    };
    let mut accounts = crate::users::load(path)?.unwrap_or_default();
    if !crate::users::set(&mut accounts, name.trim(), &password)? {
        anyhow::bail!(if ru {
            "такой аккаунт уже был: у него теперь новый пароль"
        } else {
            "that account existed: it has the new password now"
        });
    }
    crate::users::save(path, &accounts)
}

/// The server's checks, again as often as asked.
fn show_checkup(ru: bool, again: &str, done: &str) -> anyhow::Result<()> {
    loop {
        let report = crate::checkup::report(ru, &crate::checkup::run(ru));
        if dialog::choose_button(&report, &[again, done])?.as_deref() != Some(again) {
            return Ok(());
        }
    }
}

/// A new account, or one of `names` to change.
fn pick_account(ru: bool, path: &std::path::Path, names: &[String]) -> anyhow::Result<()> {
    let add = if ru {
        "＋ Добавить аккаунт"
    } else {
        "＋ Add an account"
    };
    let mut items = vec![add.to_owned()];
    items.extend(names.iter().cloned());
    let prompt = if ru {
        "Добавить аккаунт или выбрать, какой изменить:"
    } else {
        "Add an account, or pick one to change:"
    };
    let ok = if ru { "Выбрать" } else { "Choose" };
    match dialog::choose_item(prompt, &items, ok)? {
        Some(item) if item == add => add_account(ru, path),
        Some(name) => edit_account(ru, path, &name),
        None => Ok(()),
    }
}

/// Removes the server from this Mac after asking: the services, the app,
/// its data and logs, its permissions and the copy from the disk image.
/// `true` when it is gone.
fn remove_everything(ru: bool) -> anyhow::Result<bool> {
    let question = if ru {
        "Удалить OpenRDP с этого Mac полностью? Уйдут сервер и служба окна входа, приложение, аккаунты RDP, сертификат, журналы, разрешения записи экрана и универсального доступа и копия в «Программах». Файлы перенесутся в Корзину. macOS спросит пароль администратора. Настройки самого Mac (SSH, «Общий экран», включение после сбоя питания) останутся."
    } else {
        "Remove OpenRDP from this Mac completely? The server and its login-window service, the app, the RDP accounts, the certificate, the logs, the Screen Recording and Accessibility permissions and the copy in Applications all go. Files go to the Trash. macOS will ask for an administrator password. The Mac's own settings (SSH, Screen Sharing, start-up after a power failure) stay."
    };
    let button = if ru { "Удалить" } else { "Remove" };
    if !dialog::proceed(question, button)? {
        return Ok(false);
    }
    crate::service::uninstall_everything()?;
    dialog::note(if ru {
        "OpenRDP удалён. Файлы в Корзине."
    } else {
        "OpenRDP is removed. The files are in the Trash."
    })?;
    Ok(true)
}

fn edit_account(ru: bool, path: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let name = name.to_owned();
    let actions: Vec<String> = if ru {
        vec![
            "Сменить пароль".into(),
            "Переименовать".into(),
            "Удалить".into(),
        ]
    } else {
        vec![
            "Change the password".into(),
            "Rename".into(),
            "Remove".into(),
        ]
    };
    let prompt = if ru {
        format!("Аккаунт {name}")
    } else {
        format!("Account {name}")
    };
    let ok = if ru { "Дальше" } else { "Next" };
    let Some(action) = dialog::choose_item(&prompt, &actions, ok)? else {
        return Ok(());
    };
    let mut accounts = crate::users::load(path)?.unwrap_or_default();
    if action == actions[0] {
        let Some(password) = ask_new_password(ru, &name)? else {
            return Ok(());
        };
        crate::users::set(&mut accounts, &name, &password)?;
    } else if action == actions[1] {
        let prompt = if ru { "Новое имя" } else { "New name" };
        let Some(new) = dialog::ask(prompt, false, &name)? else {
            return Ok(());
        };
        crate::users::rename(&mut accounts, &name, new.trim())?;
    } else {
        let question = if ru {
            format!(
                "Удалить аккаунт {name}? Открытый сеанс продолжится, но войти с ним больше нельзя."
            )
        } else {
            format!(
                "Remove the account {name}? An open session keeps running, but it cannot sign in again."
            )
        };
        let button = if ru { "Удалить" } else { "Remove" };
        if !dialog::proceed(&question, button)? {
            return Ok(());
        }
        crate::users::remove(&mut accounts, &name)?;
    }
    crate::users::save(path, &accounts)
}

/// Asks for a password twice; `None` when cancelled.
fn ask_new_password(ru: bool, name: &str) -> anyhow::Result<Option<String>> {
    let first = if ru {
        format!("Пароль для {name}")
    } else {
        format!("Password for {name}")
    };
    let Some(password) = dialog::ask(&first, true, "")? else {
        return Ok(None);
    };
    let again = if ru { "Ещё раз" } else { "Again" };
    let Some(repeat) = dialog::ask(again, true, "")? else {
        return Ok(None);
    };
    anyhow::ensure!(
        password == repeat,
        if ru {
            "пароли не совпали"
        } else {
            "the passwords differ"
        }
    );
    Ok(Some(password))
}

fn accounts_heading(ru: bool) -> &'static str {
    if ru {
        "Аккаунты для входа по RDP (изменения действуют со следующего подключения):"
    } else {
        "Accounts for signing in over RDP (changes apply from the next connection):"
    }
}

pub(crate) fn request_capture() {
    objc2_core_graphics::CGRequestScreenCaptureAccess();
}

pub(crate) fn accessibility_granted(prompt: bool) -> bool {
    let options = objc2_core_foundation::CFDictionary::<
        objc2_core_foundation::CFString,
        objc2_core_foundation::CFBoolean,
    >::from_slices(
        &[&*objc2_core_foundation::CFString::from_static_str(
            "AXTrustedCheckOptionPrompt",
        )],
        &[objc2_core_foundation::CFBoolean::new(prompt)],
    );
    // SAFETY: the dictionary holds the documented option key with a CFBoolean.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

fn open_privacy(anchor: &str) {
    let url =
        format!("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?{anchor}");
    let _ = Command::new("open").arg(url).status();
}

fn screen_intro(ru: bool) -> &'static str {
    if ru {
        "Дальше запись экрана. В окне macOS нажмите «Разрешить». Если откроется список, включите переключатель у OpenRDP. Без этого картинки не будет. Если macOS предложит «Завершить и открыть снова», соглашайтесь: установка продолжится сама."
    } else {
        "Next is Screen Recording. In the macOS window, click Allow. If a list opens instead, turn on the OpenRDP switch. There is no picture without it. If macOS offers to Quit & Reopen, accept: the install carries on by itself."
    }
}

fn screen_still(ru: bool) -> &'static str {
    if ru {
        "Запись экрана всё ещё выключена. В списке включите переключатель OpenRDP и нажмите «Проверить»."
    } else {
        "Screen Recording is still off. Turn on the OpenRDP switch in the list, then click Check."
    }
}

fn accessibility_intro(ru: bool) -> &'static str {
    if ru {
        "Дальше универсальный доступ. Кнопки «Разрешить» нет: включите переключатель OpenRDP. Без этого не работают мышь и клавиатура. Другие пункты включать не нужно."
    } else {
        "Next is Accessibility. There is no Allow button: turn on the OpenRDP switch. The mouse and keyboard do not work without it. Nothing else needs to be turned on."
    }
}

fn accessibility_still(ru: bool) -> &'static str {
    if ru {
        "Универсальный доступ всё ещё выключен. Включите переключатель OpenRDP и нажмите «Проверить»."
    } else {
        "Accessibility is still off. Turn on the OpenRDP switch, then click Check."
    }
}

fn folders_intro(ru: bool) -> &'static str {
    if ru {
        "Дальше папки. macOS спросит доступ к Рабочему столу, Документам и Загрузкам: нажмите «Разрешить» в каждом окне. Это нужно, чтобы копировать файлы через буфер обмена, и иначе macOS спросит об этом посреди удалённой сессии. Можно отказаться: всё остальное работает и без этого."
    } else {
        "Next are folders. macOS asks for the Desktop, Documents and Downloads: click Allow in each window. Copying files through the clipboard needs them, and otherwise macOS would ask in the middle of a remote session. You can say no: everything else works without them."
    }
}

fn done(ru: bool) -> &'static str {
    if ru {
        "Готово. Запись экрана и универсальный доступ включены, сервер запущен."
    } else {
        "Done. Screen Recording and Accessibility are on, and the server is running."
    }
}

fn cancelled(ru: bool) -> &'static str {
    if ru {
        "Пока не готово. Откройте OpenRDP снова — продолжим с разрешения, которого не хватает."
    } else {
        "Not finished yet. Open OpenRDP again and it will continue from the permission that is still missing."
    }
}

fn already_ready(ru: bool) -> &'static str {
    if ru {
        "OpenRDP установлен и работает. Запись экрана и универсальный доступ включены."
    } else {
        "OpenRDP is installed and running. Screen Recording and Accessibility are on."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_intro_does_not_ask_the_system() {
        let mut nudges = 0;
        let finished = until_granted(
            "intro",
            "next",
            "still",
            "check",
            || nudges += 1,
            || true,
            &mut |message, button| {
                assert_eq!((message, button), ("intro", "next"));
                Ok(false)
            },
        )
        .unwrap();
        assert!(!finished);
        assert_eq!(nudges, 0);
    }

    #[test]
    fn the_first_grant_ends_the_step() {
        let nudges = std::cell::Cell::new(0);
        let finished = until_granted(
            "intro",
            "next",
            "still",
            "check",
            || nudges.set(nudges.get() + 1),
            || nudges.get() == 1,
            &mut |_, _| Ok(true),
        )
        .unwrap();
        assert!(finished);
        assert_eq!(nudges.get(), 1);
    }

    #[test]
    fn a_missing_grant_reopens_the_step_until_it_lands() {
        let nudges = std::cell::Cell::new(0);
        let asked = std::cell::RefCell::new(Vec::new());
        let finished = until_granted(
            "intro",
            "next",
            "still",
            "check",
            || nudges.set(nudges.get() + 1),
            || nudges.get() >= 2,
            &mut |message, button| {
                asked
                    .borrow_mut()
                    .push((message.to_owned(), button.to_owned()));
                Ok(true)
            },
        )
        .unwrap();
        assert!(finished);
        assert_eq!(
            asked.borrow().as_slice(),
            [
                ("intro".to_owned(), "next".to_owned()),
                ("still".to_owned(), "check".to_owned())
            ]
        );
        assert_eq!(nudges.get(), 2);
    }

    #[test]
    fn cancelling_the_check_stops_before_another_attempt() {
        let mut nudges = 0;
        let finished = until_granted(
            "intro",
            "next",
            "still",
            "check",
            || nudges += 1,
            || false,
            &mut |message, _| Ok(message == "intro"),
        )
        .unwrap();
        assert!(!finished);
        assert_eq!(nudges, 2);
    }

    #[test]
    fn the_russian_windows_name_the_switch() {
        assert!(screen_intro(true).contains("«Разрешить»"));
        assert!(screen_intro(true).contains("OpenRDP"));
        assert!(accessibility_intro(true).contains("Кнопки «Разрешить» нет"));
        assert!(accessibility_intro(true).contains("мышь"));
        assert!(accessibility_intro(true).contains("Другие пункты включать не нужно"));
        assert!(done(true).contains("сервер запущен"));
        assert!(screen_intro(true).contains("Завершить и открыть снова"));
        assert!(screen_still(true).contains("Проверить"));
        assert!(cancelled(true).contains("снова"));
    }
}

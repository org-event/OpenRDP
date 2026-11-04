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
        if !crate::service::agent_running() {
            crate::service::start_agent()?;
            return dialog::note(done(ru));
        }
        return dialog::note(already_ready(ru));
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
    dialog::note(done(ru))
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

pub(crate) fn capture_granted() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
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
        "Дальше запись экрана. В окне macOS нажмите «Разрешить». Если откроется список, включите переключатель у OpenRDP. Без этого картинки не будет."
    } else {
        "Next is Screen Recording. In the macOS window, click Allow. If a list opens instead, turn on the OpenRDP switch. There is no picture without it."
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
        "Готово. Запись экрана и универсальный доступ включены, сервер запущен. Подключайтесь именем и паролем, которые задали в начале. Больше ничего включать не нужно."
    } else {
        "Done. Screen Recording and Accessibility are on, and the server is running. Connect with the name and password you entered at the start. Nothing else needs to be turned on."
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
        "OpenRDP уже установлен. Запись экрана и универсальный доступ включены, больше ничего не нужно."
    } else {
        "OpenRDP is already installed. Screen Recording and Accessibility are on, and nothing else is needed."
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
        assert!(done(true).contains("Больше ничего включать не нужно"));
        assert!(screen_still(true).contains("Проверить"));
        assert!(cancelled(true).contains("снова"));
    }
}

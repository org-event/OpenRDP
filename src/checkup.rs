//! The installed app's "Check the server": what `scripts/openrdp-doctor`
//! checks from a terminal, as a list in a dialog.

use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

/// How one check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ok,
    Warn,
    Fail,
}

impl Mark {
    fn symbol(self) -> &'static str {
        match self {
            Self::Ok => "✅",
            Self::Warn => "⚠️",
            Self::Fail => "❌",
        }
    }
}

pub struct Check {
    pub mark: Mark,
    pub text: String,
}

/// Every check, in the order a person would fix them.
pub fn run(ru: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |mark: Mark, ru_text: &str, en_text: &str| {
        checks.push(Check {
            mark,
            text: (if ru { ru_text } else { en_text }).to_owned(),
        });
    };
    let pick = |good: bool, bad: Mark| if good { Mark::Ok } else { bad };

    let running = launchctl_running(&format!("gui/{}/com.openrdp.server", uid()));
    add(
        pick(running, Mark::Fail),
        if running {
            "Сервер запущен"
        } else {
            "Сервер не запущен"
        },
        if running {
            "The server is running"
        } else {
            "The server is not running"
        },
    );
    let listening = port_open(3389);
    add(
        pick(listening, Mark::Fail),
        if listening {
            "Порт 3389 принимает подключения"
        } else {
            "Порт 3389 не отвечает"
        },
        if listening {
            "Port 3389 accepts connections"
        } else {
            "Port 3389 does not answer"
        },
    );
    let capture = crate::setup::capture_granted();
    add(
        pick(capture, Mark::Fail),
        if capture {
            "Запись экрана включена"
        } else {
            "Запись экрана выключена: картинки не будет"
        },
        if capture {
            "Screen Recording is on"
        } else {
            "Screen Recording is off: no picture"
        },
    );
    let accessibility = crate::setup::accessibility_granted(false);
    add(
        pick(accessibility, Mark::Fail),
        if accessibility {
            "Универсальный доступ включён"
        } else {
            "Универсальный доступ выключен: не работают мышь и клавиатура"
        },
        if accessibility {
            "Accessibility is on"
        } else {
            "Accessibility is off: no mouse or keyboard"
        },
    );
    let accounts = crate::users::path()
        .ok()
        .and_then(|path| crate::users::load(&path).ok().flatten())
        .map_or(0, |accounts| accounts.len());
    add(
        pick(accounts > 0, Mark::Fail),
        &format!("Аккаунтов RDP: {accounts}"),
        &format!("RDP accounts: {accounts}"),
    );
    let broker = launchctl_running("system/com.openrdp.broker")
        && std::path::Path::new("/Library/LaunchAgents/com.openrdp.loginwindow.plist").exists();
    add(
        pick(broker, Mark::Warn),
        if broker {
            "Служба окна входа работает: подключиться можно и до входа в систему"
        } else {
            "Служба окна входа не установлена: после выхода из системы RDP недоступен"
        },
        if broker {
            "The login-window service is running: connecting works before anyone logs in"
        } else {
            "The login-window service is not installed: after a logout RDP is gone"
        },
    );
    let autorestart = pmset("autorestart").as_deref() == Some("1");
    add(
        pick(autorestart, Mark::Warn),
        if autorestart {
            "Mac включается сам после пропадания питания"
        } else {
            "После пропадания питания Mac останется выключенным"
        },
        if autorestart {
            "The Mac starts up by itself after a power failure"
        } else {
            "The Mac stays off after a power failure"
        },
    );
    let womp = pmset("womp").as_deref() == Some("1");
    add(
        pick(womp, Mark::Warn),
        if womp {
            "Пробуждение по сети включено"
        } else {
            "Пробуждение по сети выключено"
        },
        if womp {
            "Wake for network access is on"
        } else {
            "Wake for network access is off"
        },
    );
    let ssh = port_open(22);
    add(
        pick(ssh, Mark::Warn),
        if ssh {
            "Удалённый вход (SSH) включён: клиент сможет разблокировать FileVault"
        } else {
            "Удалённый вход (SSH) выключен: после пропадания питания FileVault не разблокировать"
        },
        if ssh {
            "Remote Login (SSH) is on: the client can unlock FileVault"
        } else {
            "Remote Login (SSH) is off: FileVault cannot be unlocked after a power cut"
        },
    );
    let vnc = port_open(5900);
    add(
        pick(vnc, Mark::Warn),
        if vnc {
            "«Общий экран» включён"
        } else {
            "«Общий экран» выключен: запасного входа нет"
        },
        if vnc {
            "Screen Sharing is on"
        } else {
            "Screen Sharing is off: no fallback way in"
        },
    );
    match firewall_blocks() {
        Some(true) => add(
            Mark::Fail,
            "Брандмауэр блокирует OpenRDP",
            "The firewall blocks OpenRDP",
        ),
        Some(false) => add(
            Mark::Ok,
            "Брандмауэр пропускает OpenRDP",
            "The firewall lets OpenRDP through",
        ),
        None => add(Mark::Ok, "Брандмауэр выключен", "The firewall is off"),
    }
    let filevault = Command::new("/usr/bin/fdesetup")
        .arg("status")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("On"));
    if filevault {
        add(
            Mark::Ok,
            "FileVault включён: после пропадания питания диск разблокирует OpenRDP Client по SSH",
            "FileVault is on: after a power cut OpenRDP Client unlocks the disk over SSH",
        );
    }
    checks
}

/// The checks as dialog text.
pub fn report(ru: bool, checks: &[Check]) -> String {
    let problems = checks.iter().filter(|c| c.mark != Mark::Ok).count();
    let mut text = format!(
        "OpenRDP {}\n{}\n",
        env!("CARGO_PKG_VERSION"),
        match (ru, problems) {
            (true, 0) => "Всё в порядке.".to_owned(),
            (false, 0) => "Everything is in order.".to_owned(),
            (true, n) => format!("Требует внимания: {n}."),
            (false, n) => format!("Needs attention: {n}."),
        }
    );
    for check in checks {
        text.push_str(&format!("\n{}  {}", check.mark.symbol(), check.text));
    }
    text
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { getuid() }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

fn launchctl_running(target: &str) -> bool {
    Command::new("/bin/launchctl")
        .args(["print", target])
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("state = running")
        })
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// A `pmset -g` setting.
fn pmset(key: &str) -> Option<String> {
    let out = Command::new("/usr/bin/pmset").arg("-g").output().ok()?;
    pmset_value(&String::from_utf8_lossy(&out.stdout), key)
}

fn pmset_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some(key)).then(|| words.next().map(str::to_owned))?
    })
}

/// `None` when the firewall is off, else whether it blocks the server.
fn firewall_blocks() -> Option<bool> {
    let tool = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    let state = Command::new(tool).arg("--getglobalstate").output().ok()?;
    if !String::from_utf8_lossy(&state.stdout).contains("enabled") {
        return None;
    }
    let binary = crate::service::installed_binary().ok()?;
    let out = Command::new(tool)
        .arg("--getappblocked")
        .arg(&binary)
        .output()
        .ok()?;
    // "Incoming connection to … is permitted." or "… is blocked."
    Some(String::from_utf8_lossy(&out.stdout).contains("is blocked"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pmset_values_are_read_by_key() {
        let text = " autorestartatconnect 0\n autorestart          1\n womp                 1\n sleep 0 (sleep prevented by openrdp)\n";
        assert_eq!(pmset_value(text, "autorestart").as_deref(), Some("1"));
        assert_eq!(pmset_value(text, "womp").as_deref(), Some("1"));
        assert_eq!(pmset_value(text, "sleep").as_deref(), Some("0"));
        assert_eq!(pmset_value(text, "powernap"), None);
    }

    #[test]
    fn the_report_counts_what_needs_attention() {
        let checks = [
            Check {
                mark: Mark::Ok,
                text: "a".into(),
            },
            Check {
                mark: Mark::Warn,
                text: "b".into(),
            },
        ];
        let text = report(true, &checks);
        assert!(text.contains("Требует внимания: 1."));
        assert!(text.contains("⚠️  b"));
    }
}

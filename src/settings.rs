//! The installed app's window: the server's state, the RDP accounts, the
//! checks and removal, on one page.
//!
//! A WebKit view in an AppKit window, drawn from `settings.html` inside this
//! binary, so the window always matches the server that self-updates. The
//! page sends `cmd␟arg␟…` strings to the `openrdp` message handler; answers go
//! back as calls to `app.*` in the page.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};

const PAGE: &str = include_str!("settings.html");
const ICON: &[u8] = include_bytes!("../assets/icon/server-256.png");
const SEPARATOR: char = '\u{1f}';

thread_local! {
    static VIEW: RefCell<Option<Retained<WKWebView>>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenRDPSettingsBridge"]
    struct Bridge;

    unsafe impl NSObjectProtocol for Bridge {}

    unsafe impl WKScriptMessageHandler for Bridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: the page posts strings; anything else is ignored.
            let body = unsafe { message.body() };
            if let Ok(text) = body.downcast::<NSString>() {
                handle(&text.to_string());
            }
        }
    }

    unsafe impl NSWindowDelegate for Bridge {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
);

impl Bridge {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Shows the window and runs until it is closed. Must be called on the main
/// thread of a process that has not started an event loop yet.
pub fn open() -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("not on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // A window of its own in the Dock and the app switcher while it is open.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let bridge = Bridge::new(mtm);
    // SAFETY: plain configuration of a new web view on the main thread.
    let view = unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        config.userContentController().addScriptMessageHandler_name(
            ProtocolObject::from_ref(&*bridge),
            &NSString::from_str("openrdp"),
        );
        let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(720.0, 780.0));
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
    };
    let html = PAGE.replace("{{ICON}}", &base64(ICON)).replace(
        "{{LANG}}",
        if crate::dialog::russian() { "ru" } else { "en" },
    );
    // SAFETY: a static page with no base URL; nothing is fetched.
    let _ = unsafe { view.loadHTMLString_baseURL(&NSString::from_str(&html), None) };

    // SAFETY: a new window, kept alive by `window` until the app ends.
    let window = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(720.0, 780.0)),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        );
        window.setReleasedWhenClosed(false);
        window
    };
    window.setTitle(&NSString::from_str("OpenRDP"));
    window.setTitlebarAppearsTransparent(true);
    window.setMinSize(CGSize::new(560.0, 520.0));
    window.setContentView(Some(&view));
    window.setDelegate(Some(ProtocolObject::from_ref(&*bridge)));
    window.center();
    window.makeKeyAndOrderFront(None);
    VIEW.with(|slot| *slot.borrow_mut() = Some(view));
    // Deprecated, but `activate` alone is macOS 14 and does not take focus
    // from the app that started this one.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
    drop(bridge);
    Ok(())
}

/// One message from the page.
fn handle(message: &str) {
    let mut parts = message.split(SEPARATOR);
    let command = parts.next().unwrap_or_default();
    let args: Vec<&str> = parts.collect();
    let arg = |i: usize| args.get(i).copied().unwrap_or_default();
    match command {
        "state" => send_state(None),
        "add" => account_change(|accounts| {
            if crate::users::set(accounts, arg(0).trim(), arg(1))? {
                Ok(())
            } else {
                anyhow::bail!("exists")
            }
        }),
        "password" => {
            account_change(|accounts| crate::users::set(accounts, arg(0), arg(1)).map(drop))
        }
        "rename" => {
            account_change(|accounts| crate::users::rename(accounts, arg(0), arg(1).trim()))
        }
        "remove" => account_change(|accounts| crate::users::remove(accounts, arg(0))),
        "check" => in_background(|| {
            let checks = crate::checkup::run(crate::dialog::russian());
            let items: Vec<String> = checks
                .iter()
                .map(|c| {
                    format!(
                        "{{\"mark\":{},\"text\":{}}}",
                        js_string(match c.mark {
                            crate::checkup::Mark::Ok => "ok",
                            crate::checkup::Mark::Warn => "warn",
                            crate::checkup::Mark::Fail => "fail",
                        }),
                        js_string(&c.text)
                    )
                })
                .collect();
            format!("app.checks([{}])", items.join(","))
        }),
        "uninstall" => in_background(|| match crate::service::uninstall_everything() {
            Ok(()) => "app.removed(null)".to_owned(),
            Err(err) => format!("app.removed({})", js_string(&format!("{err:#}"))),
        }),
        "log" => eprintln!("settings page: {}", arg(0)),
        "update-check" => in_background(|| match crate::update::newer_release() {
            Ok(tag) => format!(
                "app.update({{\"newer\":{}}})",
                tag.as_deref().map_or_else(|| "null".to_owned(), js_string)
            ),
            Err(err) => format!(
                "app.update({{\"error\":{}}})",
                js_string(&format!("{err:#}"))
            ),
        }),
        "update-install" => in_background(|| match crate::update::install_newer() {
            Ok(tag) => {
                // The window belongs to the old build: open the new one, then go.
                let _ = std::process::Command::new("/usr/bin/open")
                    .args(["-n", "-a"])
                    .arg(crate::service::installed_app())
                    .status();
                format!("app.update({{\"installed\":{}}})", js_string(&tag))
            }
            Err(err) => format!(
                "app.update({{\"error\":{}}})",
                js_string(&format!("{err:#}"))
            ),
        }),
        "quit" => {
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }
        _ => {}
    }
}

/// Changes the account file and redraws, or shows why not.
fn account_change(change: impl FnOnce(&mut Vec<crate::users::Account>) -> anyhow::Result<()>) {
    let result = (|| {
        let path = crate::users::path()?;
        let mut accounts = crate::users::load(&path)?.unwrap_or_default();
        change(&mut accounts)?;
        crate::users::save(&path, &accounts)
    })();
    send_state(result.err().map(|err| format!("{err:#}")));
}

fn send_state(problem: Option<String>) {
    let accounts: Vec<String> = crate::users::path()
        .ok()
        .and_then(|path| crate::users::load(&path).ok().flatten())
        .unwrap_or_default()
        .iter()
        .map(|a| js_string(&a.name))
        .collect();
    let running = crate::service::agent_running();
    let script = format!(
        "app.state({{\"version\":{},\"running\":{running},\"accounts\":[{}],\"problem\":{}}})",
        js_string(env!("CARGO_PKG_VERSION")),
        accounts.join(","),
        problem
            .as_deref()
            .map_or_else(|| "null".to_owned(), js_string),
    );
    run_script(&script);
}

/// Runs `work` off the main thread, then hands its script to the page.
fn in_background(work: impl FnOnce() -> String + Send + 'static) {
    std::thread::spawn(move || {
        let script = work();
        dispatch2::DispatchQueue::main().exec_async(move || run_script(&script));
    });
}

fn run_script(script: &str) {
    VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            // SAFETY: on the main thread, where the view lives; no completion handler.
            unsafe { view.evaluateJavaScript_completionHandler(&NSString::from_str(script), None) };
        }
    });
}

/// A JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_reach_the_page_intact() {
        assert_eq!(
            js_string("a\"b\\c\n</script>"),
            "\"a\\\"b\\\\c\\n\\u003c/script>\""
        );
        assert_eq!(js_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}

//! What a client asks of the server through its RDP alternate shell.
//!
//! The server runs no programs, so the field carries options, separated by
//! spaces, commas or semicolons: `/shell:curtain,nokeep` in FreeRDP,
//! `alternate shell:s:curtain` in an .rdp file. OpenRDP Client sets them from
//! its settings. Unknown words are ignored.
//!
//! - `curtain`: darken the Mac's own displays during the session (with
//!   `--curtain client`, the default);
//! - `nokeep`: remove the virtual display as soon as the client leaves,
//!   instead of keeping it `--keep-display` seconds for a reconnect;
//! - `reversescroll`: turn the vertical scroll the other way.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;

/// The options of the client connecting now; the next one replaces them.
#[derive(Debug, Default)]
pub struct ClientOptions {
    pub curtain: AtomicBool,
    pub no_keep: AtomicBool,
    pub reverse_scroll: AtomicBool,
}

impl ClientOptions {
    fn set(&self, alternate_shell: &str) {
        let has = |name: &str| has(alternate_shell, name);
        let (curtain, no_keep, reverse_scroll) =
            (has("curtain"), has("nokeep"), has("reversescroll"));
        info!(curtain, no_keep, reverse_scroll, "client options");
        self.curtain.store(curtain, Ordering::Release);
        self.no_keep.store(no_keep, Ordering::Release);
        self.reverse_scroll.store(reverse_scroll, Ordering::Release);
    }
}

fn has(alternate_shell: &str, name: &str) -> bool {
    alternate_shell
        .split([' ', ',', ';'])
        .any(|option| option.eq_ignore_ascii_case(name))
}

/// Records what each connecting client asks for.
pub struct ClientRequest(pub Arc<ClientOptions>);

impl ironrdp_server::ConnectionHandler for ClientRequest {
    fn on_connection_info(&mut self, info: &ironrdp_server::ConnectionInfo) {
        self.0.set(&info.alternate_shell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_options_from_the_alternate_shell() {
        assert!(has("curtain", "curtain"));
        assert!(has("Curtain", "curtain"));
        assert!(has("sound;curtain", "curtain"));
        assert!(has("x, curtain", "curtain"));
        assert!(!has("", "curtain"));
        assert!(!has("curtains", "curtain"));
        assert!(!has("explorer.exe", "curtain"));

        let options = ClientOptions::default();
        options.set("curtain,nokeep,ReverseScroll");
        assert!(options.curtain.load(Ordering::Acquire));
        assert!(options.no_keep.load(Ordering::Acquire));
        assert!(options.reverse_scroll.load(Ordering::Acquire));
        options.set("");
        assert!(!options.curtain.load(Ordering::Acquire));
        assert!(!options.no_keep.load(Ordering::Acquire));
        assert!(!options.reverse_scroll.load(Ordering::Acquire));
    }
}

//! Curtain: the Mac's own displays go dark while a client is connected, so
//! whoever sits at the Mac does not watch the remote session.
//!
//! Each physical display's gamma is set to black; the frame buffers, and so
//! the capture, are untouched. A display that comes back, or that macOS
//! reconfigures (mirroring resets gamma), gets the curtain again within a
//! second. The ColorSync settings come back when the session ends, and macOS
//! restores them itself if the server dies. The keyboard and mouse at the Mac
//! still work: this hides the screen, it does not lock the Mac.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_graphics::{
    CGDisplayRestoreColorSyncSettings, CGGetOnlineDisplayList, CGSetDisplayTransferByFormula,
};
use tracing::{info, warn};

/// When the Mac's own displays go dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// When the client asks, with `curtain` as its alternate shell.
    Client,
    Always,
    Never,
}

impl std::str::FromStr for Policy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "client" => Ok(Self::Client),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            other => Err(format!("--curtain is client, always or never, not {other}")),
        }
    }
}

/// Whether a client's alternate shell (`/shell:curtain` in FreeRDP,
/// `alternate shell:s:curtain` in an .rdp file) asks for the curtain. The
/// server runs no programs, so the field carries client options, separated by
/// spaces, commas or semicolons.
pub fn asked(alternate_shell: &str) -> bool {
    alternate_shell
        .split([' ', ',', ';'])
        .any(|option| option.eq_ignore_ascii_case("curtain"))
}

/// Records what each connecting client asks for.
pub struct ClientRequest(pub Arc<AtomicBool>);

impl ironrdp_server::ConnectionHandler for ClientRequest {
    fn on_connection_info(&mut self, info: &ironrdp_server::ConnectionInfo) {
        let asked = asked(&info.alternate_shell);
        if asked {
            info!("the client asked for the curtain");
        }
        self.0.store(asked, Ordering::Release);
    }
}

const REDRAW: Duration = Duration::from_secs(1);

pub struct Curtain {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Curtain {
    /// Darkens every online display except the one `spared` names (the
    /// virtual display the client sees), until dropped.
    pub fn draw(spared: impl Fn() -> Option<u32> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("openrdp-curtain".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    while !stop.load(Ordering::Acquire) {
                        let spared = spared();
                        for id in online_displays()
                            .into_iter()
                            .filter(|id| Some(*id) != spared)
                        {
                            darken(id);
                        }
                        std::thread::sleep(REDRAW);
                    }
                }
            })
            .map_err(|err| warn!(%err, "cannot start the curtain"))
            .ok();
        info!("curtain drawn");
        Self { stop, thread }
    }
}

impl Drop for Curtain {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        CGDisplayRestoreColorSyncSettings();
        info!("curtain lifted");
    }
}

fn darken(id: u32) {
    // Minimum and maximum 0 map every value to black on all three channels.
    let error = CGSetDisplayTransferByFormula(id, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0);
    if error.0 != 0 {
        warn!(id, error = error.0, "cannot darken a display");
    }
}

fn online_displays() -> Vec<u32> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: the buffer holds 16 entries, as passed.
    let error = unsafe { CGGetOnlineDisplayList(16, ids.as_mut_ptr(), &mut count) };
    if error.0 != 0 {
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

#[cfg(test)]
mod tests {
    use objc2_core_graphics::{CGGetDisplayTransferByFormula, CGMainDisplayID};

    use super::*;

    #[test]
    fn reads_the_request_from_the_alternate_shell() {
        assert!(asked("curtain"));
        assert!(asked("Curtain"));
        assert!(asked("sound;curtain"));
        assert!(asked("x, curtain"));
        assert!(!asked(""));
        assert!(!asked("curtains"));
        assert!(!asked("explorer.exe"));
    }

    fn maximum(id: u32) -> f32 {
        let mut v = [0f32; 9];
        let [a, b, c, d, e, f, g, h, i] = &mut v;
        // SAFETY: nine valid out-pointers.
        let error = unsafe { CGGetDisplayTransferByFormula(id, a, b, c, d, e, f, g, h, i) };
        assert_eq!(error.0, 0);
        v[1].max(v[4]).max(v[7])
    }

    /// Darkens this Mac's displays for a moment; run by hand at the Mac.
    #[test]
    #[ignore]
    fn darkens_and_restores() {
        let id = CGMainDisplayID();
        let before = maximum(id);
        let curtain = Curtain::draw(|| None);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(maximum(id), 0.0);
        drop(curtain);
        assert_eq!(maximum(id), before);
    }
}

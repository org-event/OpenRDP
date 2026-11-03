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

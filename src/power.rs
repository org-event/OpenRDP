//! Power assertions: the Mac stays awake while the server runs, and its
//! display stays on while a client is connected. A connecting client wakes a
//! display that went to sleep meanwhile.
//!
//! Keeping a woken display on takes more than PreventUserIdleDisplaySleep:
//! that only stops the idle timer, and after hours without a user the timer
//! has long run out. macOS turned the display off ten seconds after the wake,
//! in the middle of the server's display configuration, and WindowServer then
//! refused every reconfiguration until it slept and woke again. So the
//! session also declares user activity, as a remote user is, and renews it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2_core_foundation::CFString;
use tracing::{debug, info, warn};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: &CFString, user_type: u32, id: *mut u32) -> i32;
}

/// `kIOPMUserActiveLocal`: as if someone touched the keyboard.
const USER_ACTIVE_LOCAL: u32 = 0;
/// How long a waking display gets before capture is tried anyway.
const WAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a session renews its user activity; well inside the ten
/// seconds macOS gives a declared wake.
const RENEW: Duration = Duration::from_secs(3);

/// Declares user activity, or renews the declaration `id` names.
fn declare_user_activity(reason: &str, id: &mut u32) -> bool {
    // SAFETY: valid CF string and out-pointer.
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(&CFString::from_str(reason), USER_ACTIVE_LOCAL, id)
    };
    if status != 0 {
        warn!(status, "cannot declare user activity");
    }
    status == 0
}

/// Wakes the displays and waits for the main one: a sleeping display can be
/// neither captured nor mirrored, and no virtual display can be added.
///
/// Declared every time, since a virtual display does not report sleep.
pub fn wake_display() {
    let main = objc2_core_graphics::CGMainDisplayID();
    if objc2_core_graphics::CGDisplayIsAsleep(main) {
        info!("display asleep; waking it for the client");
    }
    let mut id = 0;
    if !declare_user_activity("OpenRDP client connecting", &mut id) {
        return;
    }
    let started = Instant::now();
    if objc2_core_graphics::CGDisplayIsAsleep(main) {
        while objc2_core_graphics::CGDisplayIsAsleep(main) && started.elapsed() < WAKE_TIMEOUT {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Displays report awake a moment before they accept configuration.
        std::thread::sleep(Duration::from_millis(500));
        debug!(ms = started.elapsed().as_millis(), "display awake");
    }
    // SAFETY: the assertion created above; it would also time out on its own.
    unsafe { IOPMAssertionRelease(id) };
}

const ASSERTION_LEVEL_ON: u32 = 255;

/// Held until dropped.
pub struct Awake {
    id: u32,
}

impl Awake {
    /// Keeps the system from idle sleep, so the server stays reachable.
    pub fn system() -> Option<Self> {
        Self::new("PreventUserIdleSystemSleep", "OpenRDP server is running")
    }

    fn display() -> Option<Self> {
        Self::new("PreventUserIdleDisplaySleep", "OpenRDP client is connected")
    }

    fn new(kind: &str, reason: &str) -> Option<Self> {
        let mut id = 0;
        // SAFETY: valid CF strings and out-pointer.
        let status = unsafe {
            IOPMAssertionCreateWithName(
                &CFString::from_str(kind),
                ASSERTION_LEVEL_ON,
                &CFString::from_str(reason),
                &mut id,
            )
        };
        if status != 0 {
            warn!(kind, status, "cannot create a power assertion");
            return None;
        }
        debug!(kind, "power assertion held");
        Some(Self { id })
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        // SAFETY: an assertion this value created.
        unsafe { IOPMAssertionRelease(self.id) };
    }
}

/// Keeps the display on while a client is connected, until dropped: holds off
/// idle sleep and keeps declaring user activity.
pub struct DisplayAwake {
    _assertion: Option<Awake>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DisplayAwake {
    pub fn hold() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("openrdp-display-awake".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    let mut id = 0;
                    while !stop.load(Ordering::Acquire) {
                        if !declare_user_activity("OpenRDP client is connected", &mut id) {
                            return;
                        }
                        // Short steps, so dropping does not wait long.
                        let renewed = Instant::now();
                        while renewed.elapsed() < RENEW && !stop.load(Ordering::Acquire) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                    if id != 0 {
                        // SAFETY: the assertion declared above.
                        unsafe { IOPMAssertionRelease(id) };
                    }
                }
            })
            .map_err(|err| warn!(%err, "cannot keep declaring user activity"))
            .ok();
        Self {
            _assertion: Awake::display(),
            stop,
            thread,
        }
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

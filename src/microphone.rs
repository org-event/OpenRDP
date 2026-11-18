//! The client's microphone, heard on the Mac (MS-RDPEAI, `AUDIO_INPUT`).
//!
//! Apps record from "OpenRDP Microphone", a Core Audio device from the
//! driver in `driver/openrdp_audio.c`. This server plays the client's
//! microphone into its hidden twin, "OpenRDP Microphone Feed", and the driver
//! loops the feed into the microphone. `install-system` puts the driver in
//! `/Library/Audio/Plug-Ins/HAL`.
//!
//! The client is asked to record only once an app on the Mac uses OpenRDP
//! Microphone, so the client's microphone stays off in sessions that never
//! need it. MS-RDPEAI has no "stop" from the server: once open, the client
//! records until the session ends.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Context as _;
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_server::{
    RdpeaiServerBackend, RdpeaiServerFactory, RdpeaiServerMessage, ServerEvent, ServerEventSender,
};
use objc2_core_foundation::CFString;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

const MIC_UID: &str = "OpenRDPMicrophone_UID";
const FEED_UID: &str = "OpenRDPMicrophoneFeed_UID";

const DRIVER_DIR: &str = "/Library/Audio/Plug-Ins/HAL/OpenRDPAudio.driver";
const DRIVER_EXE: &str = "OpenRDPAudio";
const DRIVER_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/OpenRDPAudio"));
/// From the driver's Info.plist: its factory and the AudioServerPlugIn type.
const FACTORY_UUID: &str = "6B1F7A3E-2C4D-4E8A-9F11-0A7D3C5B9E21";
const PLUGIN_TYPE_UUID: &str = "443ABAB8-E7B3-491A-B985-BEB9187030DB";

/// 20 ms of the client's microphone per packet.
const PACKET_MS: u32 = 20;

// ---------------------------------------------------------------------------
// The driver on disk. Root only, from `install-system` / `uninstall-system`.

fn driver_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key><string>en</string>
	<key>CFBundleExecutable</key><string>{DRIVER_EXE}</string>
	<key>CFBundleIdentifier</key><string>com.openrdp.audio</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>CFBundleName</key><string>OpenRDP Microphone</string>
	<key>CFBundlePackageType</key><string>BNDL</string>
	<key>CFBundleShortVersionString</key><string>{version}</string>
	<key>CFBundleVersion</key><string>{version}</string>
	<key>CFPlugInFactories</key>
	<dict><key>{FACTORY_UUID}</key><string>OpenRDPAudio_Create</string></dict>
	<key>CFPlugInTypes</key>
	<dict><key>{PLUGIN_TYPE_UUID}</key><array><string>{FACTORY_UUID}</string></array></dict>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
"#
    )
}

/// Writes the driver and restarts Core Audio when it changed. A failure here
/// costs the microphone only, so it is reported, not returned.
pub(crate) fn install_driver() {
    match write_driver(Path::new(DRIVER_DIR)) {
        Ok(false) => println!("OpenRDP Microphone: already installed"),
        Ok(true) => {
            restart_core_audio();
            println!("OpenRDP Microphone: installed");
        }
        Err(err) => println!("OpenRDP Microphone: could not install it ({err:#})"),
    }
}

pub(crate) fn uninstall_driver() {
    let dir = Path::new(DRIVER_DIR);
    if dir.exists() {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                restart_core_audio();
                println!("Removed OpenRDP Microphone.");
            }
            Err(err) => println!("Could not remove {DRIVER_DIR}: {err}"),
        }
    }
}

/// `true` when anything was written.
fn write_driver(dir: &Path) -> anyhow::Result<bool> {
    let plist = dir.join("Contents/Info.plist");
    let exe = dir.join(format!("Contents/MacOS/{DRIVER_EXE}"));
    let plist_text = driver_plist();
    let same = std::fs::read(&exe).is_ok_and(|bytes| bytes == DRIVER_BINARY)
        && std::fs::read_to_string(&plist).is_ok_and(|text| text == plist_text);
    if same {
        return Ok(false);
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    std::fs::create_dir_all(exe.parent().context("driver path")?)?;
    std::fs::write(&plist, plist_text).with_context(|| format!("write {}", plist.display()))?;
    std::fs::write(&exe, DRIVER_BINARY).with_context(|| format!("write {}", exe.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    // Seals Info.plist into the signature; the linker's ad hoc one covers the code only.
    crate::service::run(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(dir),
    )
    .context("sign the driver")?;
    Ok(true)
}

/// Core Audio loads drivers when it starts. Apps playing sound lose it for a moment.
fn restart_core_audio() {
    let _ = Command::new("/usr/bin/killall")
        .arg("coreaudiod")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Core Audio and Audio Toolbox, the few calls used here.

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct ComponentDescription {
    kind: u32,
    sub_kind: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type RenderProc = unsafe extern "C" fn(
    refcon: *mut c_void,
    flags: *mut u32,
    timestamp: *const c_void,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32;

#[repr(C)]
struct RenderCallback {
    proc_: RenderProc,
    refcon: *mut c_void,
}

type ListenerProc = unsafe extern "C" fn(
    object: u32,
    count: u32,
    addresses: *const PropertyAddress,
    client: *mut c_void,
) -> i32;

type AudioUnit = *mut c_void;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectAddPropertyListener(
        object: u32,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> i32;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: *mut c_void, desc: *const ComponentDescription)
    -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> i32;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> i32;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
    fn AudioUnitInitialize(unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStart(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStop(unit: AudioUnit) -> i32;
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SYSTEM_OBJECT: u32 = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const TRANSLATE_UID_TO_DEVICE: u32 = fourcc(b"uidd");
const RUNNING_SOMEWHERE: u32 = fourcc(b"gone");

const UNIT_SCOPE_GLOBAL: u32 = 0;
const UNIT_SCOPE_INPUT: u32 = 1;
const CURRENT_DEVICE: u32 = 2000;
const STREAM_FORMAT: u32 = 8;
const SET_RENDER_CALLBACK: u32 = 23;
const FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
/// Signed integer, packed, native (little) endian.
const FLAGS_PCM16: u32 = 0x4 | 0x8;

fn check(status: i32, what: &str) -> anyhow::Result<()> {
    anyhow::ensure!(status == 0, "{what}: OSStatus {status}");
    Ok(())
}

fn size_of<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).unwrap_or(u32::MAX)
}

/// The Core Audio device with `uid`, if the driver is loaded.
fn device(uid: &str) -> Option<u32> {
    let uid = CFString::from_str(uid);
    let uid_ref: *const CFString = &*uid;
    let address = PropertyAddress {
        selector: TRANSLATE_UID_TO_DEVICE,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    };
    let mut id: u32 = 0;
    let mut size = size_of::<u32>();
    // SAFETY: the qualifier is a CFStringRef, the output a u32 of the given size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address,
            size_of::<*const CFString>(),
            (&raw const uid_ref).cast(),
            &mut size,
            (&raw mut id).cast(),
        )
    };
    (status == 0 && id != 0).then_some(id)
}

/// Whether macOS lists "OpenRDP Microphone": the driver is installed and loaded.
pub(crate) fn device_present() -> bool {
    device(MIC_UID).is_some()
}

const fn running_address() -> PropertyAddress {
    PropertyAddress {
        selector: RUNNING_SOMEWHERE,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    }
}

/// Whether any app on the Mac is recording from `device`.
fn in_use(device: u32) -> bool {
    let address = running_address();
    let mut running: u32 = 0;
    let mut size = size_of::<u32>();
    // SAFETY: the output is a u32 of the given size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            (&raw mut running).cast(),
        )
    };
    status == 0 && running != 0
}

// ---------------------------------------------------------------------------
// Playing into the feed.

/// The client's samples, waiting for the feed device to take them.
struct Jitter {
    samples: VecDeque<i16>,
    /// Played from once this much is buffered, and again after running dry.
    target: usize,
    /// Above this the oldest samples are dropped down to `target`, so the
    /// delay cannot grow when the client's clock runs a little fast.
    max: usize,
    playing: bool,
}

impl Jitter {
    fn push(&mut self, data: &[u8]) {
        self.samples.extend(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair)),
        );
        if self.samples.len() > self.max {
            let excess = self.samples.len() - self.target;
            self.samples.drain(..excess);
        }
    }

    fn pull(&mut self, out: &mut [i16]) {
        if !self.playing && self.samples.len() >= self.target {
            self.playing = true;
        }
        let mut filled = 0;
        if self.playing {
            // drain(..n), not drain(..): a drained range is removed whole, also
            // the part zip never takes, which threw away all but one buffer.
            filled = self.samples.len().min(out.len());
            for (slot, sample) in out.iter_mut().zip(self.samples.drain(..filled)) {
                *slot = sample;
            }
            if filled < out.len() {
                self.playing = false;
            }
        }
        out[filled..].fill(0);
    }
}

/// An AUHAL output unit playing into the feed device, converting from the
/// client's format.
struct Player {
    unit: AudioUnit,
    jitter: Arc<Mutex<Jitter>>,
    format: AudioFormat,
}

// SAFETY: the unit is only started, stopped and disposed through `&mut self`
// or drop; Audio Toolbox calls are thread safe.
unsafe impl Send for Player {}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("rate", &self.format.n_samples_per_sec)
            .field("channels", &self.format.n_channels)
            .finish_non_exhaustive()
    }
}

unsafe extern "C" fn render(
    refcon: *mut c_void,
    _flags: *mut u32,
    _timestamp: *const c_void,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: refcon is the Arc's Mutex, kept alive by the Player until the
    // unit is disposed; data is the unit's interleaved buffer list.
    let (jitter, buffer) = unsafe { (&*refcon.cast::<Mutex<Jitter>>(), &mut (*data).buffers[0]) };
    let len = buffer.size as usize / 2;
    // SAFETY: the buffer holds `size` bytes of 16-bit samples.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<i16>(), len) };
    match jitter.try_lock() {
        Ok(mut jitter) => jitter.pull(out),
        Err(_) => out.fill(0),
    }
    0
}

impl Player {
    fn start(feed: u32, format: &AudioFormat) -> anyhow::Result<Self> {
        let rate = format.n_samples_per_sec as usize;
        let channels = usize::from(format.n_channels);
        let jitter = Arc::new(Mutex::new(Jitter {
            samples: VecDeque::new(),
            target: rate * channels * 60 / 1000,
            max: rate * channels * 300 / 1000,
            playing: false,
        }));
        let desc = ComponentDescription {
            kind: fourcc(b"auou"),
            sub_kind: fourcc(b"ahal"),
            manufacturer: fourcc(b"appl"),
            flags: 0,
            flags_mask: 0,
        };
        // SAFETY: plain Audio Toolbox calls with valid arguments; the unit is
        // disposed on every error path through Drop.
        unsafe {
            let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
            anyhow::ensure!(!component.is_null(), "no HAL output unit");
            let mut unit: AudioUnit = std::ptr::null_mut();
            check(
                AudioComponentInstanceNew(component, &mut unit),
                "open the output unit",
            )?;
            let player = Self {
                unit,
                jitter,
                format: format.clone(),
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    CURRENT_DEVICE,
                    UNIT_SCOPE_GLOBAL,
                    0,
                    (&raw const feed).cast(),
                    size_of::<u32>(),
                ),
                "select OpenRDP Microphone Feed",
            )?;
            let block = u32::from(format.n_channels) * 2;
            let stream = StreamDescription {
                sample_rate: f64::from(format.n_samples_per_sec),
                format_id: FORMAT_LINEAR_PCM,
                format_flags: FLAGS_PCM16,
                bytes_per_packet: block,
                frames_per_packet: 1,
                bytes_per_frame: block,
                channels_per_frame: u32::from(format.n_channels),
                bits_per_channel: 16,
                reserved: 0,
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    STREAM_FORMAT,
                    UNIT_SCOPE_INPUT,
                    0,
                    (&raw const stream).cast(),
                    size_of::<StreamDescription>(),
                ),
                "set the format",
            )?;
            let callback = RenderCallback {
                proc_: render,
                refcon: Arc::as_ptr(&player.jitter).cast_mut().cast(),
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    SET_RENDER_CALLBACK,
                    UNIT_SCOPE_INPUT,
                    0,
                    (&raw const callback).cast(),
                    size_of::<RenderCallback>(),
                ),
                "set the render callback",
            )?;
            check(AudioUnitInitialize(unit), "initialize the output unit")?;
            check(AudioOutputUnitStart(unit), "start the output unit")?;
            Ok(player)
        }
    }

    fn push(&self, data: &[u8]) {
        if let Ok(mut jitter) = self.jitter.lock() {
            jitter.push(data);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: the unit came from AudioComponentInstanceNew; disposing it
        // waits out a render callback in progress.
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

// ---------------------------------------------------------------------------
// The channel.

fn pcm(rate: u32, channels: u16) -> AudioFormat {
    let block_align = channels * 2;
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(block_align),
        n_block_align: block_align,
        bits_per_sample: 16,
        data: None,
    }
}

/// Every 16-bit PCM rate a client might record at, mono and stereo.
fn offered() -> Vec<AudioFormat> {
    [48_000, 44_100, 32_000, 22_050, 16_000, 11_025, 8_000]
        .into_iter()
        .flat_map(|rate| [pcm(rate, 1), pcm(rate, 2)])
        .collect()
}

/// The client's best format: the highest rate, stereo before mono. FreeRDP's
/// macOS recorder records only in stereo and, asked for mono, closes the
/// channel without a word; Core Audio mixes stereo down to the mono driver.
fn choose(negotiated: &[AudioFormat]) -> Option<(u32, AudioFormat)> {
    negotiated
        .iter()
        .enumerate()
        .filter(|(_, f)| f.format == WaveFormat::PCM && f.bits_per_sample == 16)
        .filter(|(_, f)| (1..=2).contains(&f.n_channels))
        .max_by_key(|(_, f)| (f.n_samples_per_sec, f.n_channels))
        .and_then(|(index, f)| Some((u32::try_from(index).ok()?, f.clone())))
}

#[derive(Debug, Default)]
struct State {
    /// Index into the negotiated list, and that format.
    chosen: Option<(u32, AudioFormat)>,
    /// Open sent: the client records (or refused) until the channel closes.
    opened: bool,
    player: Option<Player>,
}

#[derive(Debug)]
struct Shared {
    events: Option<UnboundedSender<ServerEvent>>,
    state: Mutex<State>,
}

impl Shared {
    /// Asks the client to record once an app records from OpenRDP Microphone.
    fn open_if_used(&self) {
        let Some(mic) = device(MIC_UID) else { return };
        if !in_use(mic) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some((index, format)) = state.chosen.clone() else {
            return;
        };
        if state.opened {
            return;
        }
        let Some(events) = &self.events else { return };
        let frames = format.n_samples_per_sec * PACKET_MS / 1000;
        let sent = events.send(ServerEvent::Rdpeai(RdpeaiServerMessage::Open {
            frames_per_packet: frames,
            initial_format: index,
            capture_format: format.clone(),
        }));
        if sent.is_ok() {
            state.opened = true;
            info!(
                rate = format.n_samples_per_sec,
                channels = format.n_channels,
                "microphone: asked the client to record"
            );
        }
    }
}

/// The session's channel, for the Core Audio listener. One session at a time.
static CURRENT: Mutex<Option<Weak<Shared>>> = Mutex::new(None);
/// The device the listener is on; Core Audio's ids change when it restarts.
static LISTENING: Mutex<Option<u32>> = Mutex::new(None);

unsafe extern "C" fn mic_changed(
    _object: u32,
    _count: u32,
    _addresses: *const PropertyAddress,
    _client: *mut c_void,
) -> i32 {
    let current = CURRENT.lock().ok().and_then(|c| c.as_ref()?.upgrade());
    if let Some(shared) = current {
        shared.open_if_used();
    }
    0
}

fn listen(mic: u32) {
    let Ok(mut listening) = LISTENING.lock() else {
        return;
    };
    if *listening == Some(mic) {
        return;
    }
    let address = running_address();
    // SAFETY: a plain function with no client data; it lives as long as the process.
    let status =
        unsafe { AudioObjectAddPropertyListener(mic, &address, mic_changed, std::ptr::null_mut()) };
    if status == 0 {
        *listening = Some(mic);
    } else {
        warn!(status, "microphone: cannot watch OpenRDP Microphone");
    }
}

#[derive(Default)]
pub struct MicrophoneFactory {
    events: Option<UnboundedSender<ServerEvent>>,
}

impl ServerEventSender for MicrophoneFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.events = Some(sender);
    }
}

impl RdpeaiServerFactory for MicrophoneFactory {
    fn build_backend(&self) -> Box<dyn RdpeaiServerBackend> {
        let shared = Arc::new(Shared {
            events: self.events.clone(),
            state: Mutex::new(State::default()),
        });
        if let Ok(mut current) = CURRENT.lock() {
            *current = Some(Arc::downgrade(&shared));
        }
        Box::new(Microphone {
            formats: offered(),
            shared,
        })
    }
}

struct Microphone {
    formats: Vec<AudioFormat>,
    shared: Arc<Shared>,
}

impl RdpeaiServerBackend for Microphone {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn on_formats_negotiated(&mut self, negotiated: &[AudioFormat]) {
        let chosen = choose(negotiated);
        debug!(?negotiated, ?chosen, "microphone formats");
        let Some(mic) = device(MIC_UID) else {
            warn!(
                "microphone: the OpenRDP Microphone driver is not loaded; restart the broker: sudo launchctl kickstart -k system/com.openrdp.broker"
            );
            return;
        };
        if chosen.is_none() {
            warn!("microphone: the client offers no 16-bit PCM format");
        }
        if let Ok(mut state) = self.shared.state.lock() {
            *state = State {
                chosen,
                ..State::default()
            };
        }
        listen(mic);
        self.shared.open_if_used();
    }

    fn on_open_reply(&mut self, result: i32) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if result < 0 {
            // FreeRDP refuses while macOS still asks for the permission. Ask
            // again the next time an app starts recording.
            warn!(result, "microphone: the client could not record");
            state.opened = false;
            return;
        }
        let Some((_, format)) = state.chosen.clone() else {
            return;
        };
        state.player = start_player(&format);
    }

    fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if state.player.as_ref().is_none_or(|p| p.format != *format) {
            state.player = start_player(format);
        }
        if let Some(player) = &state.player {
            player.push(data);
        }
    }

    fn on_close(&mut self) {
        if let Ok(mut state) = self.shared.state.lock()
            && state.player.take().is_some()
        {
            info!("microphone stopped");
        }
    }
}

fn start_player(format: &AudioFormat) -> Option<Player> {
    let Some(feed) = device(FEED_UID) else {
        warn!("microphone: OpenRDP Microphone Feed is missing");
        return None;
    };
    match Player::start(feed, format) {
        Ok(player) => {
            info!(
                rate = format.n_samples_per_sec,
                channels = format.n_channels,
                "microphone started"
            );
            Some(player)
        }
        Err(err) => {
            warn!(error = %format!("{err:#}"), "microphone: cannot play into the feed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_the_highest_rate_then_stereo() {
        let negotiated = [
            pcm(44_100, 2),
            pcm(48_000, 2),
            pcm(48_000, 1),
            pcm(16_000, 1),
        ];
        let (index, format) = choose(&negotiated).unwrap();
        assert_eq!(index, 1);
        assert_eq!((format.n_samples_per_sec, format.n_channels), (48_000, 2));
    }

    #[test]
    fn jitter_waits_for_the_target_and_caps_the_delay() {
        let mut jitter = Jitter {
            samples: VecDeque::new(),
            target: 4,
            max: 8,
            playing: false,
        };
        let mut out = [7i16; 2];
        jitter.push(&[1, 0, 2, 0]);
        jitter.pull(&mut out);
        assert_eq!(out, [0, 0], "not enough buffered yet");
        jitter.push(&[3, 0, 4, 0]);
        jitter.pull(&mut out[..1]);
        assert_eq!(out[0], 1);
        jitter.pull(&mut out);
        assert_eq!(out, [2, 3], "a pull keeps what it does not take");
        let many: Vec<u8> = (10..20).flat_map(|n: i16| n.to_le_bytes()).collect();
        jitter.push(&many);
        assert_eq!(jitter.samples.len(), 4, "dropped down to the target");
        assert_eq!(jitter.samples.front(), Some(&16));
    }

    #[test]
    fn driver_plist_names_the_factory() {
        let plist = driver_plist();
        assert!(plist.contains("OpenRDPAudio_Create"));
        assert!(plist.contains(PLUGIN_TYPE_UUID));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Plays 4 s of a 440 Hz tone into the feed, stereo, in 20 ms packets, as
    /// a client would. Record "OpenRDP Microphone" meanwhile and listen: the
    /// tone must be clean. `cargo test live_tone -- --ignored`.
    #[test]
    #[ignore = "needs the OpenRDP Microphone driver"]
    fn live_tone() {
        let format = pcm(48_000, 2);
        let player = start_player(&format).expect("player");
        let mut n = 0u64;
        let start = std::time::Instant::now();
        for packet in 0..200u32 {
            let mut data = Vec::with_capacity(960 * 4);
            for _ in 0..960 {
                let v =
                    ((n as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
                data.extend_from_slice(&v.to_le_bytes());
                data.extend_from_slice(&v.to_le_bytes());
                n += 1;
            }
            player.push(&data);
            let due = start + std::time::Duration::from_millis(20 * u64::from(packet + 1));
            std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
        }
    }
}

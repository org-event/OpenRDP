//! The client's camera, seen on the Mac (MS-RDPECAM, video capture redirection).
//!
//! macOS shows a camera to apps only through a Camera Extension, a system
//! extension that needs an Apple Developer ID to install. OpenRDP has none, so
//! it borrows OBS Studio's: "OBS Virtual Camera" (OBS 30 or later) has a sink
//! stream that any process may feed. Install OBS once, click "Start Virtual
//! Camera" there and allow the extension in System Settings; OBS need not run
//! after that. Apps on the Mac then pick "OBS Virtual Camera".
//!
//! The client announces its camera on `RDCamera_Device_Enumerator`; the server
//! opens the device channel it names, activates the device and picks an MJPEG
//! media type. It starts the stream only while an app on the Mac uses OBS
//! Virtual Camera, so the client's camera light stays off otherwise, and stops
//! it a few seconds after the last app lets go. Each JPEG is drawn into a
//! 1920x1080 BGRA picture, the format OBS's camera declares, and queued on its
//! sink.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_dvc::{DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use ironrdp_pdu::ironrdp_core::{AsAny, decode};
use ironrdp_rdpecam::pdu::{DeviceMessage, DevicePdu};
use ironrdp_rdpecam::{
    DeviceDescriptor, EnumerationServer, MediaFormat, MediaType, ProtocolVersion, StartStreamInfo,
    StreamDescription,
};
use ironrdp_server::{DvcServerMessage, ServerEvent};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext, CGImage};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// OBS Virtual Camera's device UID, fixed in OBS's camera extension.
const OBS_DEVICE_UID: &str = "7626645E-4425-469E-9D8B-97E0FA59AC75";
/// The picture OBS's camera declares: 1920x1080, 32-bit BGRA.
const OUT_WIDTH: usize = 1920;
const OUT_HEIGHT: usize = 1080;
/// The largest picture asked of the client; JPEG keeps 720p at a few MB/s.
const MAX_WIDTH: u32 = 1280;
const MAX_HEIGHT: u32 = 720;
const WANTED_FPS: f64 = 30.0;
/// Sample Requests kept in flight, so a round trip does not cost frames.
const CREDITS: usize = 2;
/// How often the server looks whether an app uses the camera.
const POLL: Duration = Duration::from_millis(500);
/// Polls without an app before the client's camera is stopped.
const IDLE_POLLS: u32 = 6;

// ---------------------------------------------------------------------------
// Choosing a media type.

/// The MJPEG type closest to 1280x720 at 30 fps, no larger than that.
fn choose(media_types: &[MediaType]) -> Option<MediaType> {
    media_types
        .iter()
        .copied()
        .filter(|t| {
            t.format == MediaFormat::Mjpg
                && t.width > 0
                && t.height > 0
                && t.width <= MAX_WIDTH
                && t.height <= MAX_HEIGHT
                && t.frame_rate_numerator > 0
                && t.frame_rate_denominator > 0
        })
        .min_by(|a, b| score(a).total_cmp(&score(b)))
}

/// Missing pixels count most, then the distance from 30 fps.
fn score(t: &MediaType) -> f64 {
    let missing = f64::from(MAX_WIDTH * MAX_HEIGHT) - f64::from(t.width) * f64::from(t.height);
    let fps = f64::from(t.frame_rate_numerator) / f64::from(t.frame_rate_denominator);
    missing + (fps - WANTED_FPS).abs()
}

// ---------------------------------------------------------------------------
// The device channel.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Activating,
    Streams,
    MediaTypes,
    /// Media type chosen, stream stopped.
    Ready,
    Starting,
    Streaming,
    Stopping,
    Failed,
}

/// What the device hands to the picture thread.
#[derive(Debug)]
enum Feed {
    Jpeg(Vec<u8>),
    Stopped,
}

/// The device's protocol, without the channel around it.
struct Device {
    phase: Phase,
    stream: u8,
    media_type: Option<MediaType>,
    /// An app on the Mac uses the camera.
    wanted: bool,
    feed: Box<dyn FnMut(Feed) + Send>,
}

impl Device {
    fn new(feed: Box<dyn FnMut(Feed) + Send>) -> Self {
        Self {
            phase: Phase::Activating,
            stream: 0,
            media_type: None,
            wanted: false,
            feed,
        }
    }

    fn start_request(&mut self) -> Vec<DevicePdu> {
        let Some(media_type) = self.media_type else {
            return Vec::new();
        };
        self.phase = Phase::Starting;
        vec![DevicePdu::StartStreamsRequest(vec![StartStreamInfo {
            stream_index: self.stream,
            media_type,
        }])]
    }

    fn set_wanted(&mut self, wanted: bool) -> Vec<DevicePdu> {
        self.wanted = wanted;
        match (self.phase, wanted) {
            (Phase::Ready, true) => self.start_request(),
            (Phase::Streaming, false) => {
                self.phase = Phase::Stopping;
                (self.feed)(Feed::Stopped);
                vec![DevicePdu::StopStreamsRequest]
            }
            // Starting and Stopping settle on `wanted` once the client answers.
            _ => Vec::new(),
        }
    }

    fn fail(&mut self, why: &str) -> Vec<DevicePdu> {
        warn!(why, "camera: giving up on the client's camera");
        if self.phase == Phase::Streaming {
            (self.feed)(Feed::Stopped);
        }
        self.phase = Phase::Failed;
        Vec::new()
    }

    fn handle(&mut self, pdu: DevicePdu) -> Vec<DevicePdu> {
        match (self.phase, pdu) {
            (Phase::Activating, DevicePdu::SuccessResponse) => {
                self.phase = Phase::Streams;
                vec![DevicePdu::StreamListRequest]
            }
            (Phase::Streams, DevicePdu::StreamListResponse(streams)) => {
                let index = streams
                    .iter()
                    .position(|s| s.frame_source_types & StreamDescription::COLOR != 0)
                    .and_then(|i| u8::try_from(i).ok());
                let Some(index) = index else {
                    return self.fail("no color stream");
                };
                self.stream = index;
                self.phase = Phase::MediaTypes;
                vec![DevicePdu::MediaTypeListRequest(index)]
            }
            (Phase::MediaTypes, DevicePdu::MediaTypeListResponse(types)) => {
                let Some(media_type) = choose(&types) else {
                    debug!(?types, "camera media types");
                    return self.fail("the client offers no MJPEG up to 1280x720");
                };
                info!(
                    width = media_type.width,
                    height = media_type.height,
                    fps = media_type.frame_rate_numerator / media_type.frame_rate_denominator,
                    "camera: client camera ready"
                );
                self.media_type = Some(media_type);
                self.phase = Phase::Ready;
                if self.wanted {
                    self.start_request()
                } else {
                    Vec::new()
                }
            }
            (Phase::Starting, DevicePdu::SuccessResponse) => {
                if self.wanted {
                    info!("camera: streaming the client's camera");
                    self.phase = Phase::Streaming;
                    vec![DevicePdu::SampleRequest(self.stream); CREDITS]
                } else {
                    self.phase = Phase::Stopping;
                    vec![DevicePdu::StopStreamsRequest]
                }
            }
            (Phase::Stopping, DevicePdu::SuccessResponse) => {
                info!("camera: stopped the client's camera");
                self.phase = Phase::Ready;
                if self.wanted {
                    self.start_request()
                } else {
                    Vec::new()
                }
            }
            (
                Phase::Streaming,
                DevicePdu::SampleResponse {
                    stream_index,
                    sample,
                },
            ) if stream_index == self.stream => {
                if sample.starts_with(&[0xFF, 0xD8]) {
                    (self.feed)(Feed::Jpeg(sample));
                }
                vec![DevicePdu::SampleRequest(self.stream)]
            }
            (Phase::Streaming, DevicePdu::SampleErrorResponse { error, .. }) => {
                warn!(?error, "camera: the client lost a frame");
                vec![DevicePdu::SampleRequest(self.stream)]
            }
            // Samples still on their way after a Stop Streams Request.
            (
                Phase::Stopping | Phase::Ready,
                DevicePdu::SampleResponse { .. } | DevicePdu::SampleErrorResponse { .. },
            ) => Vec::new(),
            (Phase::Starting, DevicePdu::ErrorResponse(error)) => {
                // macOS may still be asking the client's user for the camera:
                // try again the next time an app opens the camera.
                warn!(?error, "camera: the client could not start its camera");
                self.phase = Phase::Ready;
                self.wanted = false;
                Vec::new()
            }
            (_, DevicePdu::ErrorResponse(error)) => {
                warn!(?error, "camera: the client refused a request");
                self.fail("the client refused a request")
            }
            (phase, pdu) => {
                debug!(?phase, ?pdu, "camera: ignoring an out-of-sequence message");
                Vec::new()
            }
        }
    }
}

/// The server end of one client camera's channel.
pub struct CameraChannel {
    channel_name: String,
    version: ProtocolVersion,
    device: Device,
    /// Cleared when the channel goes, to end the threads.
    alive: Arc<AtomicBool>,
}

impl CameraChannel {
    fn messages(&self, pdus: Vec<DevicePdu>) -> Vec<DvcMessage> {
        pdus.into_iter()
            .map(|pdu| Box::new(DeviceMessage::new(self.version, pdu)) as DvcMessage)
            .collect()
    }
}

impl AsAny for CameraChannel {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl DvcProcessor for CameraChannel {
    fn channel_name(&self) -> &str {
        &self.channel_name
    }

    fn max_message_size(&self) -> Option<usize> {
        // A JPEG of 1280x720 stays far below this.
        Some(16 * 1024 * 1024)
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.device.phase = Phase::Activating;
        Ok(self.messages(vec![DevicePdu::ActivateDeviceRequest]))
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let message: DeviceMessage = match decode(payload) {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "camera: malformed message");
                return Ok(Vec::new());
            }
        };
        let replies = self.device.handle(message.pdu);
        Ok(self.messages(replies))
    }

    fn close(&mut self, _channel_id: u32) {
        self.alive.store(false, Ordering::Release);
        if self.device.phase == Phase::Streaming {
            (self.device.feed)(Feed::Stopped);
        }
        self.device.phase = Phase::Failed;
    }
}

impl DvcServerProcessor for CameraChannel {}

impl Drop for CameraChannel {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The factory: one enumeration channel per connection.

/// Adds the camera's enumeration channel to every connection.
#[derive(Default)]
pub struct CameraFactory {
    events: Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>,
}

impl CameraFactory {
    /// Where the server's event sender goes once the server is built.
    pub fn sender_slot(&self) -> Arc<Mutex<Option<UnboundedSender<ServerEvent>>>> {
        Arc::clone(&self.events)
    }
}

impl ironrdp_server::DynamicChannelFactory for CameraFactory {
    fn attach(&self, channels: ironrdp_dvc::DrdynvcServer) -> ironrdp_dvc::DrdynvcServer {
        let events = Arc::clone(&self.events);
        // The device in use; OBS Virtual Camera shows one picture.
        let current: Arc<Mutex<Option<String>>> = Arc::default();
        let removed_current = Arc::clone(&current);
        let removed_events = Arc::clone(&events);
        let enumeration = EnumerationServer::new(
            Box::new(move |version, device| added(&events, &current, version, &device)),
            Box::new(move |_version, channel_name| {
                let mut current = removed_current.lock().unwrap_or_else(|e| e.into_inner());
                if current.as_deref() != Some(channel_name.as_str()) {
                    return;
                }
                *current = None;
                info!(
                    channel = channel_name,
                    "camera: the client removed its camera"
                );
                send(&removed_events, DvcServerMessage::Close { channel_name });
            }),
        );
        channels.with_dynamic_channel(enumeration)
    }
}

fn send(events: &Mutex<Option<UnboundedSender<ServerEvent>>>, message: DvcServerMessage) {
    let events = events.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(events) = events.as_ref() {
        let _ = events.send(ServerEvent::Dvc(message));
    }
}

fn added(
    events: &Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>,
    current: &Mutex<Option<String>>,
    version: ProtocolVersion,
    device: &DeviceDescriptor,
) {
    let mut current = current.lock().unwrap_or_else(|e| e.into_inner());
    if current.is_some() {
        debug!(
            name = device.display_name,
            "camera: one client camera at a time"
        );
        return;
    }
    let Some(obs) = obs_device() else {
        warn!(
            name = device.display_name,
            "camera: the client offers a camera, but OBS Virtual Camera is missing: install OBS Studio 30 or later, click Start Virtual Camera once and allow its extension in System Settings"
        );
        return;
    };
    info!(
        name = device.display_name,
        channel = device.channel_name,
        "camera: client camera announced"
    );
    *current = Some(device.channel_name.clone());

    let alive = Arc::new(AtomicBool::new(true));
    let (frames, rx) = std::sync::mpsc::sync_channel::<Feed>(2);
    let feed = Box::new(move |feed: Feed| {
        let stopped = matches!(feed, Feed::Stopped);
        match frames.try_send(feed) {
            // The Mac is behind: this frame goes.
            Err(TrySendError::Full(_)) if !stopped => {}
            Err(TrySendError::Full(feed)) => {
                let _ = frames.send(feed);
            }
            _ => {}
        }
    });
    std::thread::spawn(move || pictures(&rx));
    std::thread::spawn({
        let alive = Arc::clone(&alive);
        let events = Arc::clone(events);
        let channel_name = device.channel_name.clone();
        move || watch(obs, &alive, &events, &channel_name)
    });
    send(
        events,
        DvcServerMessage::Open(Box::new(CameraChannel {
            channel_name: device.channel_name.clone(),
            version,
            device: Device::new(feed),
            alive,
        })),
    );
}

/// Tells the device channel whether an app on the Mac uses the camera.
fn watch(
    obs: u32,
    alive: &AtomicBool,
    events: &Mutex<Option<UnboundedSender<ServerEvent>>>,
    channel_name: &str,
) {
    let mut wanted = false;
    let mut idle = 0;
    while alive.load(Ordering::Acquire) {
        let used = in_use(obs);
        idle = if used { 0 } else { idle + 1 };
        let now = if wanted { idle < IDLE_POLLS } else { used };
        if now != wanted {
            wanted = now;
            debug!(wanted, "camera: an app on the Mac uses the camera");
            send(
                events,
                DvcServerMessage::Run {
                    channel_name: channel_name.to_owned(),
                    run: Box::new(move |processor| {
                        let Some(channel) = processor.as_any_mut().downcast_mut::<CameraChannel>()
                        else {
                            return Ok(Vec::new());
                        };
                        let pdus = channel.device.set_wanted(wanted);
                        Ok(channel.messages(pdus))
                    }),
                },
            );
        }
        std::thread::sleep(POLL);
    }
}

/// Draws the client's frames into OBS Virtual Camera until the channel goes.
fn pictures(rx: &Receiver<Feed>) {
    let mut sink: Option<ObsSink> = None;
    while let Ok(feed) = rx.recv() {
        match feed {
            Feed::Stopped => sink = None,
            Feed::Jpeg(jpeg) => {
                if sink.is_none() {
                    match ObsSink::open() {
                        Ok(opened) => sink = Some(opened),
                        Err(err) => {
                            warn!(error = %format!("{err:#}"), "camera: cannot feed OBS Virtual Camera");
                            continue;
                        }
                    }
                }
                if let Some(sink) = &sink {
                    sink.push(&jpeg);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core Media I/O, Core Media, Core Video, Image I/O: the few calls used here.

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const TIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

#[repr(C)]
struct SampleTimingInfo {
    duration: CMTime,
    presentation: CMTime,
    decode: CMTime,
}

type QueueAlteredProc = unsafe extern "C" fn(stream: u32, token: *mut c_void, refcon: *mut c_void);

#[link(name = "CoreMediaIO", kind = "framework")]
unsafe extern "C" {
    fn CMIOObjectGetPropertyDataSize(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> i32;
    fn CMIOObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        used: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn CMIOStreamCopyBufferQueue(
        stream: u32,
        altered: Option<QueueAlteredProc>,
        refcon: *mut c_void,
        queue: *mut *mut c_void,
    ) -> i32;
    fn CMIODeviceStartStream(device: u32, stream: u32) -> i32;
    fn CMIODeviceStopStream(device: u32, stream: u32) -> i32;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSimpleQueueEnqueue(queue: *mut c_void, element: *const c_void) -> i32;
    fn CMSimpleQueueGetCount(queue: *mut c_void) -> i32;
    fn CMSimpleQueueGetCapacity(queue: *mut c_void) -> i32;
    fn CMVideoFormatDescriptionCreate(
        allocator: *const c_void,
        codec: u32,
        width: i32,
        height: i32,
        extensions: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CMSampleBufferCreateForImageBuffer(
        allocator: *const c_void,
        image: *mut c_void,
        data_ready: u8,
        make_ready: *const c_void,
        refcon: *const c_void,
        format: *mut c_void,
        timing: *const SampleTimingInfo,
        out: *mut *mut c_void,
    ) -> i32;
    fn CMClockGetHostTimeClock() -> *mut c_void;
    fn CMClockGetTime(clock: *mut c_void) -> CMTime;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: *const c_void;
    static kCVPixelBufferWidthKey: *const c_void;
    static kCVPixelBufferHeightKey: *const c_void;
    static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    fn CVPixelBufferPoolCreate(
        allocator: *const c_void,
        pool_attributes: *const c_void,
        buffer_attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: *const c_void,
        pool: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(object: *const c_void);
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *const c_void;
    fn CFNumberCreate(allocator: *const c_void, kind: isize, value: *const c_void)
    -> *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFStringGetCString(string: *const c_void, buffer: *mut u8, size: isize, encoding: u32)
    -> u8;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: *const c_void, options: *const c_void) -> *const c_void;
    fn CGImageSourceCreateImageAtIndex(
        source: *const c_void,
        index: usize,
        options: *const c_void,
    ) -> *const c_void;
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SYSTEM_OBJECT: u32 = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const HARDWARE_DEVICES: u32 = fourcc(b"dev#");
const DEVICE_UID: u32 = fourcc(b"uid ");
const DEVICE_STREAMS: u32 = fourcc(b"stm#");
const RUNNING_SOMEWHERE: u32 = fourcc(b"gone");
const STREAM_DIRECTION: u32 = fourcc(b"sdir");
const PIXEL_FORMAT_BGRA: u32 = fourcc(b"BGRA");
const NUMBER_SINT32: isize = 3;
const UTF8: u32 = 0x0800_0100;
/// kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little: BGRA in memory.
const BITMAP_BGRX: u32 = 6 | (2 << 12);

fn address(selector: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    }
}

/// A list of object ids, such as the devices or a device's streams.
fn ids(object: u32, selector: u32) -> Vec<u32> {
    let address = address(selector);
    let mut size = 0u32;
    // SAFETY: plain out-parameter.
    let status =
        unsafe { CMIOObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let mut ids = vec![0u32; size as usize / 4];
    let mut used = 0u32;
    // SAFETY: `ids` holds `size` bytes.
    let status = unsafe {
        CMIOObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            size,
            &mut used,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(used as usize / 4);
    ids
}

fn u32_property(object: u32, selector: u32) -> Option<u32> {
    let address = address(selector);
    let mut value = 0u32;
    let mut used = 0u32;
    // SAFETY: the output is a u32 of the given size.
    let status = unsafe {
        CMIOObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            4,
            &mut used,
            (&raw mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn device_uid(device: u32) -> Option<String> {
    let address = address(DEVICE_UID);
    let mut uid: *const c_void = ptr::null();
    let mut used = 0u32;
    // SAFETY: the output is a CFStringRef, which this function then owns.
    let status = unsafe {
        CMIOObjectGetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            u32::try_from(size_of::<*const c_void>()).unwrap_or(8),
            &mut used,
            (&raw mut uid).cast(),
        )
    };
    if status != 0 || uid.is_null() {
        return None;
    }
    let mut buffer = [0u8; 128];
    // SAFETY: `uid` is a CFString; the buffer's size is passed.
    let ok = unsafe { CFStringGetCString(uid, buffer.as_mut_ptr(), 128, UTF8) };
    // SAFETY: the copy returned above is released once.
    unsafe { CFRelease(uid) };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

/// OBS Virtual Camera, when its extension is installed and allowed.
fn obs_device() -> Option<u32> {
    ids(SYSTEM_OBJECT, HARDWARE_DEVICES)
        .into_iter()
        .find(|&device| device_uid(device).as_deref() == Some(OBS_DEVICE_UID))
}

/// Whether macOS lists OBS Virtual Camera, for the server check.
pub(crate) fn device_present() -> bool {
    obs_device().is_some()
}

/// Whether any process runs the camera. Our own feed may count as well; then
/// the client's camera runs until the session ends once an app has used it.
fn in_use(device: u32) -> bool {
    u32_property(device, RUNNING_SOMEWHERE).is_some_and(|running| running != 0)
}

/// OBS Virtual Camera's sink stream, fed while the client's camera streams.
struct ObsSink {
    device: u32,
    stream: u32,
    queue: NonNull<c_void>,
    format: NonNull<c_void>,
    pool: NonNull<c_void>,
}

// SAFETY: the Core Media objects are used from one thread at a time.
unsafe impl Send for ObsSink {}

fn check(status: i32, what: &str) -> anyhow::Result<()> {
    anyhow::ensure!(status == 0, "{what}: OSStatus {status}");
    Ok(())
}

fn cf_number(value: i32) -> *const c_void {
    // SAFETY: an i32 read as kCFNumberSInt32Type.
    unsafe { CFNumberCreate(ptr::null(), NUMBER_SINT32, (&raw const value).cast()) }
}

impl ObsSink {
    fn open() -> anyhow::Result<Self> {
        let device =
            obs_device().ok_or_else(|| anyhow::anyhow!("OBS Virtual Camera is missing"))?;
        let streams = ids(device, DEVICE_STREAMS);
        // The sink is the output stream (direction 0); OBS lists it second.
        let stream = streams
            .iter()
            .copied()
            .find(|&s| u32_property(s, STREAM_DIRECTION) == Some(0))
            .or_else(|| streams.get(1).copied())
            .ok_or_else(|| anyhow::anyhow!("OBS Virtual Camera has no sink stream"))?;

        let mut queue = ptr::null_mut();
        // SAFETY: no callback; the queue comes back retained.
        check(
            unsafe { CMIOStreamCopyBufferQueue(stream, None, ptr::null_mut(), &mut queue) },
            "CMIOStreamCopyBufferQueue",
        )?;
        let queue = NonNull::new(queue).ok_or_else(|| anyhow::anyhow!("no sink queue"))?;

        let mut format = ptr::null_mut();
        // SAFETY: plain out-parameter.
        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                ptr::null(),
                PIXEL_FORMAT_BGRA,
                OUT_WIDTH as i32,
                OUT_HEIGHT as i32,
                ptr::null(),
                &mut format,
            )
        };
        let format = NonNull::new(format).filter(|_| status == 0);
        let Some(format) = format else {
            // SAFETY: retained above, released once.
            unsafe { CFRelease(queue.as_ptr()) };
            anyhow::bail!("CMVideoFormatDescriptionCreate: OSStatus {status}");
        };

        let pool = match Self::pool() {
            Ok(pool) => pool,
            Err(err) => {
                // SAFETY: retained above, released once.
                unsafe {
                    CFRelease(format.as_ptr());
                    CFRelease(queue.as_ptr());
                }
                return Err(err);
            }
        };
        let sink = Self {
            device,
            stream,
            queue,
            format,
            pool,
        };
        // SAFETY: ids from Core Media I/O.
        check(
            unsafe { CMIODeviceStartStream(device, stream) },
            "CMIODeviceStartStream",
        )?;
        info!("camera: feeding OBS Virtual Camera");
        Ok(sink)
    }

    fn pool() -> anyhow::Result<NonNull<c_void>> {
        // SAFETY: Core Foundation objects built and released here; the keys are
        // Core Video's constants.
        unsafe {
            let empty = CFDictionaryCreate(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                0,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            let format = cf_number(PIXEL_FORMAT_BGRA as i32);
            let width = cf_number(OUT_WIDTH as i32);
            let height = cf_number(OUT_HEIGHT as i32);
            let keys = [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ];
            let values = [format, width, height, empty];
            let attributes = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                4,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            let mut pool = ptr::null_mut();
            let status = CVPixelBufferPoolCreate(ptr::null(), ptr::null(), attributes, &mut pool);
            for object in [attributes, empty, format, width, height] {
                CFRelease(object);
            }
            check(status, "CVPixelBufferPoolCreate")?;
            NonNull::new(pool).ok_or_else(|| anyhow::anyhow!("no pixel buffer pool"))
        }
    }

    /// Decodes one JPEG into a picture and queues it; drops it when the queue is full.
    fn push(&self, jpeg: &[u8]) {
        let queue = self.queue.as_ptr();
        // SAFETY: the queue lives as long as `self`.
        if unsafe { CMSimpleQueueGetCount(queue) >= CMSimpleQueueGetCapacity(queue) } {
            return;
        }
        let Some(image) = decode_jpeg(jpeg) else {
            return;
        };
        let mut buffer = ptr::null_mut();
        // SAFETY: the pool lives as long as `self`.
        let status = unsafe {
            CVPixelBufferPoolCreatePixelBuffer(ptr::null(), self.pool.as_ptr(), &mut buffer)
        };
        if status != 0 || buffer.is_null() {
            return;
        }
        // SAFETY: `buffer` is a locked BGRA pixel buffer of OUT_WIDTH x OUT_HEIGHT;
        // every object made here is released once, the sample buffer by the
        // queue's reader when it was queued.
        unsafe {
            CVPixelBufferLockBaseAddress(buffer, 0);
            draw(
                &image,
                CVPixelBufferGetBaseAddress(buffer),
                CVPixelBufferGetBytesPerRow(buffer),
            );
            CVPixelBufferUnlockBaseAddress(buffer, 0);

            let timing = SampleTimingInfo {
                duration: TIME_INVALID,
                presentation: CMClockGetTime(CMClockGetHostTimeClock()),
                decode: TIME_INVALID,
            };
            let mut sample = ptr::null_mut();
            let status = CMSampleBufferCreateForImageBuffer(
                ptr::null(),
                buffer,
                1,
                ptr::null(),
                ptr::null(),
                self.format.as_ptr(),
                &timing,
                &mut sample,
            );
            CFRelease(buffer);
            if status != 0 || sample.is_null() {
                return;
            }
            if CMSimpleQueueEnqueue(queue, sample) != 0 {
                CFRelease(sample);
            }
        }
    }
}

impl Drop for ObsSink {
    fn drop(&mut self) {
        // SAFETY: started in `open`; each object was retained once.
        unsafe {
            CMIODeviceStopStream(self.device, self.stream);
            CFRelease(self.pool.as_ptr());
            CFRelease(self.format.as_ptr());
            CFRelease(self.queue.as_ptr());
        }
        info!("camera: stopped feeding OBS Virtual Camera");
    }
}

/// A CGImage of the JPEG, or `None` when it does not decode.
fn decode_jpeg(jpeg: &[u8]) -> Option<CFRetained<CGImage>> {
    let length = isize::try_from(jpeg.len()).ok()?;
    // SAFETY: the data is copied into a CFData; the data and the source are
    // released here, the image is owned by the returned value.
    unsafe {
        let data = CFDataCreate(ptr::null(), jpeg.as_ptr(), length);
        if data.is_null() {
            return None;
        }
        let source = CGImageSourceCreateWithData(data, ptr::null());
        CFRelease(data);
        if source.is_null() {
            return None;
        }
        let image = CGImageSourceCreateImageAtIndex(source, 0, ptr::null());
        CFRelease(source);
        NonNull::new(image.cast_mut().cast::<CGImage>()).map(|image| CFRetained::from_raw(image))
    }
}

/// Where a picture of `width` x `height` goes in the output, keeping its shape.
fn fit(width: usize, height: usize) -> CGRect {
    let (out_w, out_h) = (OUT_WIDTH as f64, OUT_HEIGHT as f64);
    if width == 0 || height == 0 {
        return CGRect::new(CGPoint::ZERO, CGSize::new(out_w, out_h));
    }
    let scale = (out_w / width as f64).min(out_h / height as f64);
    let (w, h) = (width as f64 * scale, height as f64 * scale);
    CGRect::new(
        CGPoint::new(((out_w - w) / 2.0).round(), ((out_h - h) / 2.0).round()),
        CGSize::new(w.round(), h.round()),
    )
}

/// Draws `image` into the BGRA memory at `base`, black around it.
///
/// # Safety
/// `base` must hold OUT_HEIGHT rows of `bytes_per_row` bytes.
unsafe fn draw(image: &CGImage, base: *mut c_void, bytes_per_row: usize) {
    let Some(space) = CGColorSpace::new_device_rgb() else {
        return;
    };
    // SAFETY: the caller's memory, of the given geometry.
    let context = unsafe {
        crate::cursor::CGBitmapContextCreate(
            base,
            OUT_WIDTH,
            OUT_HEIGHT,
            8,
            bytes_per_row,
            Some(&space),
            BITMAP_BGRX,
        )
    };
    let Some(context) = context else { return };
    // SAFETY: created above, so owned.
    let context = unsafe { CFRetained::from_raw(context) };
    let rect = fit(CGImage::width(Some(image)), CGImage::height(Some(image)));
    if rect.size.width < OUT_WIDTH as f64 || rect.size.height < OUT_HEIGHT as f64 {
        CGContext::set_rgb_fill_color(Some(&context), 0.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(
            Some(&context),
            CGRect::new(
                CGPoint::ZERO,
                CGSize::new(OUT_WIDTH as f64, OUT_HEIGHT as f64),
            ),
        );
    }
    CGContext::draw_image(Some(&context), rect, Some(image));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mjpg(width: u32, height: u32, fps: u32) -> MediaType {
        MediaType {
            format: MediaFormat::Mjpg,
            width,
            height,
            frame_rate_numerator: fps,
            frame_rate_denominator: 1,
            pixel_aspect_ratio_numerator: 1,
            pixel_aspect_ratio_denominator: 1,
            flags: 0,
        }
    }

    #[test]
    fn picks_mjpeg_closest_to_720p30() {
        let nv12 = MediaType {
            format: MediaFormat::Nv12,
            ..mjpg(1280, 720, 30)
        };
        let types = [
            nv12,
            mjpg(1920, 1080, 30),
            mjpg(640, 480, 30),
            mjpg(1280, 720, 15),
            mjpg(1280, 720, 30),
        ];
        assert_eq!(choose(&types), Some(mjpg(1280, 720, 30)));
        assert_eq!(choose(&types[..3]), Some(mjpg(640, 480, 30)));
        assert_eq!(choose(&[nv12, mjpg(1920, 1080, 30)]), None);
    }

    fn recorder() -> (Device, Arc<Mutex<Vec<String>>>) {
        let fed = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&fed);
        let device = Device::new(Box::new(move |feed| {
            log.lock().unwrap().push(match feed {
                Feed::Jpeg(data) => format!("jpeg {}", data.len()),
                Feed::Stopped => "stopped".to_owned(),
            });
        }));
        (device, fed)
    }

    fn ready(device: &mut Device) {
        assert_eq!(
            device.handle(DevicePdu::SuccessResponse),
            vec![DevicePdu::StreamListRequest]
        );
        assert_eq!(
            device.handle(DevicePdu::StreamListResponse(vec![
                StreamDescription::color(true, true)
            ])),
            vec![DevicePdu::MediaTypeListRequest(0)]
        );
    }

    #[test]
    fn streams_only_while_an_app_uses_the_camera() {
        let (mut device, fed) = recorder();
        ready(&mut device);
        let media_type = mjpg(1280, 720, 30);
        assert!(
            device
                .handle(DevicePdu::MediaTypeListResponse(vec![media_type]))
                .is_empty()
        );
        assert_eq!(device.phase, Phase::Ready);

        let start = DevicePdu::StartStreamsRequest(vec![StartStreamInfo {
            stream_index: 0,
            media_type,
        }]);
        assert_eq!(device.set_wanted(true), vec![start.clone()]);
        assert_eq!(
            device.handle(DevicePdu::SuccessResponse),
            vec![DevicePdu::SampleRequest(0); CREDITS]
        );
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        assert_eq!(
            device.handle(DevicePdu::SampleResponse {
                stream_index: 0,
                sample: jpeg
            }),
            vec![DevicePdu::SampleRequest(0)]
        );

        assert_eq!(
            device.set_wanted(false),
            vec![DevicePdu::StopStreamsRequest]
        );
        // A sample already on its way is dropped without a new request.
        assert!(
            device
                .handle(DevicePdu::SampleResponse {
                    stream_index: 0,
                    sample: vec![0xFF, 0xD8]
                })
                .is_empty()
        );
        // Wanted again before the client confirmed the stop: start once it does.
        assert!(device.set_wanted(true).is_empty());
        assert_eq!(device.handle(DevicePdu::SuccessResponse), vec![start]);
        assert_eq!(*fed.lock().unwrap(), ["jpeg 4", "stopped"]);
    }

    #[test]
    fn starts_at_once_when_an_app_waits_already() {
        let (mut device, _) = recorder();
        assert!(device.set_wanted(true).is_empty());
        ready(&mut device);
        let replies = device.handle(DevicePdu::MediaTypeListResponse(vec![mjpg(640, 480, 30)]));
        assert!(matches!(
            replies.as_slice(),
            [DevicePdu::StartStreamsRequest(_)]
        ));
    }

    #[test]
    fn a_refused_start_waits_for_the_next_app() {
        let (mut device, _) = recorder();
        ready(&mut device);
        assert!(
            device
                .handle(DevicePdu::MediaTypeListResponse(vec![mjpg(640, 480, 30)]))
                .is_empty()
        );
        assert_eq!(device.set_wanted(true).len(), 1);
        let refused = DevicePdu::ErrorResponse(ironrdp_rdpecam::ErrorCode::NotInitialized);
        assert!(device.handle(refused).is_empty());
        assert_eq!(device.phase, Phase::Ready);
        assert_eq!(device.set_wanted(true).len(), 1);
    }

    #[test]
    fn draws_a_jpeg_letterboxed() {
        let jpeg = include_bytes!("tests/camera-16x12.jpg");
        let image = decode_jpeg(jpeg).expect("decodes");
        assert_eq!(
            (CGImage::width(Some(&image)), CGImage::height(Some(&image))),
            (16, 12)
        );
        assert!(decode_jpeg(b"not a jpeg").is_none());

        let stride = OUT_WIDTH * 4;
        let mut pixels = vec![0x55u8; stride * OUT_HEIGHT];
        // SAFETY: the buffer has OUT_HEIGHT rows of `stride` bytes.
        unsafe { draw(&image, pixels.as_mut_ptr().cast(), stride) };
        // 16x12 is 4:3, so 240 black columns on each side.
        assert_eq!(&pixels[..3], &[0, 0, 0]);
        assert_eq!(&pixels[stride - 4..stride - 1], &[0, 0, 0]);
        let middle = (OUT_HEIGHT / 2) * stride + (OUT_WIDTH / 2) * 4;
        assert_ne!(&pixels[middle..middle + 4], &[0x55; 4]);
    }

    #[test]
    fn letterboxes_into_1080p() {
        let r = fit(640, 480);
        assert_eq!(
            (r.origin.x, r.origin.y, r.size.width, r.size.height),
            (240.0, 0.0, 1440.0, 1080.0)
        );
        let r = fit(1280, 720);
        assert_eq!(
            (r.origin.x, r.origin.y, r.size.width, r.size.height),
            (0.0, 0.0, 1920.0, 1080.0)
        );
    }
}

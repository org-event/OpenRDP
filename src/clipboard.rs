//! Clipboard sharing (CLIPRDR): text, images and files both ways.
//!
//! The Mac side is the general pasteboard, polled for changes since macOS has no
//! change notification. A copy on the Mac announces its formats to the client,
//! which fetches the data when it pastes. A copy on the client is fetched right
//! away and written to the pasteboard, so any Mac app can paste it. Files go
//! through `clipboard_files`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::clipboard_files::{self, Download, LocalFile, Step};

const POLL: Duration = Duration::from_millis(300);
/// Our id for the registered "PNG" format; registered ids start at 0xC000.
const PNG_FORMAT: ClipboardFormatId = ClipboardFormatId(0xC0A0);
const PNG_NAME: &str = "PNG";
const FILE_LIST_NAME: &str = "FileGroupDescriptorW";

#[derive(Default)]
pub struct MacClipboardFactory {
    sender: Option<UnboundedSender<ServerEvent>>,
}

impl CliprdrBackendFactory for MacClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(MacClipboard::new(self.sender.clone()))
    }
}

impl ServerEventSender for MacClipboardFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl CliprdrServerFactory for MacClipboardFactory {}

/// What a client copy is fetched as, in request order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fetch {
    Text,
    Png,
    Dib,
}

#[derive(Default)]
struct Incoming {
    queue: VecDeque<(Fetch, ClipboardFormatId)>,
    text: Option<String>,
    png: Option<Vec<u8>>,
}

struct Shared {
    sender: Option<UnboundedSender<ServerEvent>>,
    /// Pasteboard change count of our own last write, not to be announced back.
    own_change: Mutex<isize>,
    stop: AtomicBool,
    /// The Mac files offered to the client, by their index in the list.
    offered: Mutex<Arc<Vec<LocalFile>>>,
    /// Lists the client locked, so its requests use the copy it saw.
    locked: Mutex<HashMap<u32, Arc<Vec<LocalFile>>>>,
}

impl Shared {
    fn send(&self, message: ClipboardMessage) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ServerEvent::Clipboard(message));
        }
    }

    /// Tells the client what the pasteboard holds now.
    fn announce(&self) {
        let paths = autoreleasepool(|_| {
            clipboard_files::pasteboard_files(&NSPasteboard::generalPasteboard())
        });
        if !paths.is_empty() {
            let files = clipboard_files::collect(&paths);
            if !files.is_empty() {
                let descriptors: Vec<FileDescriptor> =
                    files.iter().map(|f| f.descriptor().clone()).collect();
                info!(files = files.len(), "offering the Mac's copied files");
                *self.offered.lock().expect("poisoned") = Arc::new(files);
                self.send(ClipboardMessage::SendInitiateFileCopy(descriptors));
                return;
            }
        }
        let formats = autoreleasepool(|_| local_formats());
        debug!(?formats, "announcing the Mac clipboard");
        self.send(ClipboardMessage::SendInitiateCopy(formats));
    }

    /// Runs pasteboard work off the server's task: reading data the copying app
    /// promised (Universal Clipboard pulls it from another device) can take
    /// many seconds, and the whole connection would freeze meanwhile.
    fn in_background(self: &Arc<Self>, work: impl FnOnce(&Self) + Send + 'static) {
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("openrdp-pasteboard".into())
            .spawn(move || work(&shared));
        if let Err(err) = spawned {
            warn!(%err, "cannot start pasteboard work");
        }
    }
}

pub struct MacClipboard {
    shared: Arc<Shared>,
    watcher: Option<JoinHandle<()>>,
    incoming: Incoming,
    download: Option<Download>,
    /// Stream ids of file requests, unique across downloads.
    stream: u32,
}

impl core::fmt::Debug for MacClipboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MacClipboard")
    }
}

impl ironrdp_pdu::ironrdp_core::AsAny for MacClipboard {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl MacClipboard {
    fn new(sender: Option<UnboundedSender<ServerEvent>>) -> Self {
        Self {
            shared: Arc::new(Shared {
                sender,
                own_change: Mutex::new(-1),
                stop: AtomicBool::new(false),
                offered: Mutex::default(),
                locked: Mutex::default(),
            }),
            watcher: None,
            incoming: Incoming::default(),
            download: None,
            stream: 0,
        }
    }

    fn start_watching(&mut self) {
        if self.watcher.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.watcher = std::thread::Builder::new()
            .name("openrdp-clipboard".into())
            .spawn(move || watch(&shared))
            .map_err(|err| warn!(%err, "cannot watch the clipboard"))
            .ok();
    }

    fn request_next(&mut self) {
        if let Some(&(_, format)) = self.incoming.queue.front() {
            self.shared
                .send(ClipboardMessage::SendInitiatePaste(format));
            return;
        }
        let Incoming { text, png, .. } = std::mem::take(&mut self.incoming);
        if text.is_none() && png.is_none() {
            return;
        }
        let change = autoreleasepool(|_| write_pasteboard(text.as_deref(), png.as_deref()));
        *self.shared.own_change.lock().expect("poisoned") = change;
        info!(
            text = text.as_ref().map(String::len),
            png = png.as_ref().map(Vec::len),
            "client clipboard copied to the Mac"
        );
    }
}

impl MacClipboard {
    fn follow(&mut self, step: Step) {
        match step {
            Step::Request(request) => {
                self.shared
                    .send(ClipboardMessage::SendFileContentsRequest(request));
            }
            Step::Done(paths) => {
                if let Some(download) = self.download.take() {
                    self.stream = download.stream();
                }
                let change = autoreleasepool(|_| clipboard_files::write_pasteboard(&paths));
                *self.shared.own_change.lock().expect("poisoned") = change;
            }
            Step::Failed => {
                if let Some(download) = self.download.take() {
                    self.stream = download.stream();
                }
            }
        }
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

impl CliprdrBackend for MacClipboard {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
    }

    fn on_ready(&mut self) {
        self.shared.in_background(Shared::announce);
        self.start_watching();
    }

    fn on_request_format_list(&mut self) {
        self.shared.in_background(Shared::announce);
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        debug!(?capabilities, "clipboard capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        debug!(?available_formats, "client clipboard changed");
        let find = |wanted: &dyn Fn(&ClipboardFormat) -> bool| {
            available_formats
                .iter()
                .find(|f| wanted(f))
                .map(ClipboardFormat::id)
        };
        // A new copy replaces one still being fetched.
        self.download = None;
        if let Some(id) = find(&|f| {
            f.name()
                .is_some_and(|n| n.value().eq_ignore_ascii_case(FILE_LIST_NAME))
        }) {
            // The file list comes back through `on_remote_file_list`.
            self.incoming = Incoming::default();
            self.shared.send(ClipboardMessage::SendInitiatePaste(id));
            return;
        }
        let mut queue = VecDeque::new();
        if let Some(id) = find(&|f| f.id() == ClipboardFormatId::CF_UNICODETEXT) {
            queue.push_back((Fetch::Text, id));
        }
        let png = find(&|f| {
            f.name()
                .is_some_and(|n| n.value().eq_ignore_ascii_case(PNG_NAME))
        });
        let dib = find(&|f| f.id() == ClipboardFormatId::CF_DIBV5)
            .or_else(|| find(&|f| f.id() == ClipboardFormatId::CF_DIB));
        // PNG first; the bitmap is the fallback if the client cannot render it.
        if let Some(id) = png {
            queue.push_back((Fetch::Png, id));
        }
        if let Some(id) = dib {
            queue.push_back((Fetch::Dib, id));
        }
        // A new copy replaces one still being fetched.
        self.incoming = Incoming {
            queue,
            ..Incoming::default()
        };
        self.request_next();
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.shared.in_background(move |shared| {
            let started = std::time::Instant::now();
            let response = autoreleasepool(|_| local_data(request.format));
            debug!(format = ?request.format, ms = started.elapsed().as_millis(), "clipboard read");
            let response = response.unwrap_or_else(|| {
                debug!(format = ?request.format, "clipboard format not available");
                OwnedFormatDataResponse::new_error()
            });
            shared.send(ClipboardMessage::SendFormatData(response));
        });
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some((fetch, format)) = self.incoming.queue.pop_front() else {
            return;
        };
        if response.is_error() {
            debug!(?format, "client could not render the clipboard format");
        } else {
            match fetch {
                Fetch::Text => match response.to_unicode_string() {
                    Ok(text) => self.incoming.text = Some(text),
                    Err(err) => warn!(%err, "bad clipboard text"),
                },
                Fetch::Png => {
                    self.incoming.png = Some(response.data().to_vec());
                    self.incoming
                        .queue
                        .retain(|&(fetch, _)| fetch != Fetch::Dib);
                }
                Fetch::Dib => {
                    let data = response.data();
                    let png = if format == ClipboardFormatId::CF_DIBV5 {
                        ironrdp_cliprdr_format::bitmap::dibv5_to_png(data)
                    } else {
                        ironrdp_cliprdr_format::bitmap::dib_to_png(data)
                    };
                    match png {
                        Ok(png) => self.incoming.png = Some(png),
                        Err(err) => warn!(%err, "bad clipboard bitmap"),
                    }
                }
            }
        }
        self.request_next();
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let files = match request.data_id {
            Some(id) => self
                .shared
                .locked
                .lock()
                .expect("poisoned")
                .get(&id)
                .cloned(),
            None => Some(Arc::clone(&self.shared.offered.lock().expect("poisoned"))),
        };
        self.shared.in_background(move |shared| {
            let response = match files {
                Some(files) => clipboard_files::answer(&files, &request),
                None => FileContentsResponse::new_error(request.stream_id),
            };
            shared.send(ClipboardMessage::SendFileContentsResponse(response));
        });
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let Some(download) = self.download.as_mut() else {
            return;
        };
        if response.stream_id() != download.stream() {
            return;
        }
        let step = download.receive(&response);
        self.follow(step);
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        let offered = Arc::clone(&self.shared.offered.lock().expect("poisoned"));
        self.shared
            .locked
            .lock()
            .expect("poisoned")
            .insert(data_id.0, offered);
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.shared
            .locked
            .lock()
            .expect("poisoned")
            .remove(&data_id.0);
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        self.download = clipboard_files::download_base()
            .and_then(|base| Download::start(&base, files, clip_data_id, self.stream));
        if let Some(download) = self.download.as_mut() {
            let step = download.advance();
            self.follow(step);
        }
    }
}

/// Announces every pasteboard change that is not our own write.
fn watch(shared: &Shared) {
    let mut seen = NSPasteboard::generalPasteboard().changeCount();
    while !shared.stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL);
        let change = NSPasteboard::generalPasteboard().changeCount();
        if change == seen {
            continue;
        }
        seen = change;
        if change != *shared.own_change.lock().expect("poisoned") {
            shared.announce();
        }
    }
}

/// Formats from the pasteboard's type list alone: reading the data could wait
/// on the app or device that promised it.
fn local_formats() -> Vec<ClipboardFormat> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let mut formats = Vec::new();
    let Some(types) = pasteboard.types() else {
        return formats;
    };
    // SAFETY: AppKit's pasteboard type constants.
    let (string, png, tiff) = unsafe {
        (
            NSPasteboardTypeString,
            NSPasteboardTypePNG,
            NSPasteboardTypeTIFF,
        )
    };
    let has = |wanted: &NSString| types.iter().any(|t| &*t == wanted);
    if has(string) {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
    }
    if has(png) || has(tiff) {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        formats.push(
            ClipboardFormat::new(PNG_FORMAT).with_name(ClipboardFormatName::new_static(PNG_NAME)),
        );
    }
    formats
}

fn local_data(format: ClipboardFormatId) -> Option<OwnedFormatDataResponse> {
    let pasteboard = NSPasteboard::generalPasteboard();
    if format == ClipboardFormatId::CF_UNICODETEXT {
        // SAFETY: AppKit's pasteboard type constant.
        let text = pasteboard.stringForType(unsafe { NSPasteboardTypeString })?;
        return Some(OwnedFormatDataResponse::new_unicode_string(
            &text.to_string(),
        ));
    }
    let png = local_png(&pasteboard)?;
    if format == PNG_FORMAT {
        Some(OwnedFormatDataResponse::new_data(png))
    } else if format == ClipboardFormatId::CF_DIB {
        match ironrdp_cliprdr_format::bitmap::png_to_cf_dib(&png) {
            Ok(dib) => Some(OwnedFormatDataResponse::new_data(dib)),
            Err(err) => {
                warn!(%err, "cannot convert the clipboard image");
                None
            }
        }
    } else {
        None
    }
}

/// The pasteboard image as PNG, converted from TIFF when that is all there is.
fn local_png(pasteboard: &NSPasteboard) -> Option<Vec<u8>> {
    // SAFETY: AppKit's pasteboard type constants.
    if let Some(png) = pasteboard.dataForType(unsafe { NSPasteboardTypePNG }) {
        return Some(png.to_vec());
    }
    let tiff = pasteboard.dataForType(unsafe { NSPasteboardTypeTIFF })?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    // SAFETY: PNG takes no required properties.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

/// Replaces the pasteboard contents; returns the resulting change count.
fn write_pasteboard(text: Option<&str>, png: Option<&[u8]>) -> isize {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    if let Some(text) = text {
        // SAFETY: AppKit's pasteboard type constant.
        pasteboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
    }
    if let Some(png) = png {
        let data = NSData::with_bytes(png);
        // SAFETY: AppKit's pasteboard type constant.
        pasteboard.setData_forType(Some(&data), unsafe { NSPasteboardTypePNG });
    }
    pasteboard.changeCount()
}

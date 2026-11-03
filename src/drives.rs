//! Client drives (MS-RDPEFS) as volumes on the Mac.
//!
//! A drive the client redirects (FreeRDP `/drive:name,path`, Windows App
//! "Folders") is served by a small WebDAV server on 127.0.0.1, under a random
//! path, and mounted with the system's `mount_webdav` in `~/RDP Drives/<name>`,
//! so Finder and every app see it. Each WebDAV call becomes RDPDR requests to
//! the client: open, query, read, write, close. IronRDP numbers those requests
//! itself, so they go one at a time and each reply answers the one waiting.
//! The volume goes away when the client removes the drive or disconnects.

use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
use dav_server::fakels::FakeLs;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::FutureExt as _;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::efs::{
    Boolean, ClientDriveLockControlResponse, ClientDriveNotifyChangeDirectoryResponse,
    ClientDriveQueryDirectoryResponse, ClientDriveQueryInformationResponse,
    ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, CreateDisposition,
    CreateOptions, DesiredAccess, DeviceAnnounceHeader, DeviceCloseResponse, DeviceControlResponse,
    DeviceCreateResponse, DeviceFlushBuffersResponse, DeviceReadResponse, DeviceType,
    DeviceWriteResponse, FileAttributes, FileDispositionInformation, FileInformationClass,
    FileInformationClassLevel, FileRenameInformation, NtStatus,
};
use ironrdp_rdpdr::server::RdpdrServerBackend;
use ironrdp_server::{RdpdrServerFactory, RdpdrServerMessage, ServerEvent, ServerEventSender};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, info, warn};

/// How long one request to the client may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes per read or write request.
const CHUNK: usize = 256 << 10;
/// Seconds between 1601 and 1970, for FILETIME.
const FILETIME_EPOCH: u64 = 11_644_473_600;
/// STATUS_OBJECT_NAME_NOT_FOUND, which IronRDP has no constant for.
const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;

#[derive(Default)]
pub struct DriveFactory {
    events: Option<UnboundedSender<ServerEvent>>,
}

impl ServerEventSender for DriveFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.events = Some(sender);
    }
}

impl RdpdrServerFactory for DriveFactory {
    fn build_backend(&self) -> Box<dyn RdpdrServerBackend> {
        Box::new(DriveBackend {
            link: Arc::new(Link {
                events: self.events.clone(),
                pending: Mutex::new(None),
                turn: tokio::sync::Mutex::new(()),
            }),
            mounts: Arc::default(),
        })
    }
}

/// A reply from the client to one request.
#[derive(Debug)]
enum Reply {
    Create {
        status: NtStatus,
        file_id: u32,
    },
    Read {
        status: NtStatus,
        data: Vec<u8>,
    },
    Write {
        status: NtStatus,
        length: u32,
    },
    Info {
        status: NtStatus,
        buffer: Option<FileInformationClass>,
    },
    Status(NtStatus),
}

impl Reply {
    fn status(&self) -> NtStatus {
        match self {
            Self::Create { status, .. }
            | Self::Read { status, .. }
            | Self::Write { status, .. }
            | Self::Info { status, .. }
            | Self::Status(status) => *status,
        }
    }
}

/// The RDPDR channel as a request/reply pipe, one request at a time.
#[derive(Debug)]
struct Link {
    events: Option<UnboundedSender<ServerEvent>>,
    pending: Mutex<Option<oneshot::Sender<Reply>>>,
    turn: tokio::sync::Mutex<()>,
}

impl Link {
    async fn call(&self, message: RdpdrServerMessage) -> FsResult<Reply> {
        let _turn = self.turn.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.pending.lock().expect("poisoned") = Some(tx);
        let events = self.events.as_ref().ok_or(FsError::GeneralFailure)?;
        events
            .send(ServerEvent::Rdpdr(message))
            .map_err(|_| FsError::GeneralFailure)?;
        match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => {
                self.pending.lock().expect("poisoned").take();
                warn!("the client did not answer a drive request");
                Err(FsError::GeneralFailure)
            }
        }
    }

    fn complete(&self, reply: Reply) {
        match self.pending.lock().expect("poisoned").take() {
            Some(tx) => {
                let _ = tx.send(reply);
            }
            None => debug!(?reply, "drive reply nobody waits for"),
        }
    }
}

fn check(status: NtStatus) -> FsResult<()> {
    if status == NtStatus::SUCCESS {
        return Ok(());
    }
    let value = u32::from(status);
    Err(
        if status == NtStatus::NO_SUCH_FILE
            || status == NtStatus::OBJECT_PATH_NOT_FOUND
            || value == OBJECT_NAME_NOT_FOUND
        {
            FsError::NotFound
        } else if status == NtStatus::OBJECT_NAME_COLLISION {
            FsError::Exists
        } else if status == NtStatus::ACCESS_DENIED
            || status == NtStatus::SHARING_VIOLATION
            || status == NtStatus::MEDIA_WRITE_PROTECTED
            || status == NtStatus::DIRECTORY_NOT_EMPTY
        {
            FsError::Forbidden
        } else if status == NtStatus::DISK_FULL {
            FsError::InsufficientStorage
        } else {
            debug!(status = format!("{value:#010x}"), "drive request failed");
            FsError::GeneralFailure
        },
    )
}

/// A drive path for RDPDR: `\`-separated, from the drive's root.
fn rdp_path(path: &DavPath) -> String {
    let relative = path.as_rel_ospath().to_string_lossy().replace('/', "\\");
    format!("\\{}", relative.trim_end_matches('\\'))
}

fn time(filetime: i64) -> SystemTime {
    let ticks = u64::try_from(filetime).unwrap_or(0);
    let secs = (ticks / 10_000_000).saturating_sub(FILETIME_EPOCH);
    UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos((ticks % 10_000_000) * 100)
}

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    modified: SystemTime,
    created: SystemTime,
    dir: bool,
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.created)
    }

    fn is_dir(&self) -> bool {
        self.dir
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta: Box<dyn DavMetaData> = Box::new(self.meta.clone());
        async move { Ok(meta) }.boxed()
    }
}

/// One client drive as a file system.
#[derive(Debug, Clone)]
struct DriveFs {
    link: Arc<Link>,
    device: u32,
}

impl DriveFs {
    async fn create(
        &self,
        path: String,
        access: DesiredAccess,
        disposition: CreateDisposition,
        options: CreateOptions,
    ) -> FsResult<u32> {
        let reply = self
            .link
            .call(RdpdrServerMessage::Create {
                device_id: self.device,
                path,
                desired_access: access,
                create_disposition: disposition,
                create_options: options,
            })
            .await?;
        match reply {
            Reply::Create { status, file_id } => check(status).map(|()| file_id),
            other => check(other.status()).and(Err(FsError::GeneralFailure)),
        }
    }

    async fn close(&self, file_id: u32) {
        let _ = self
            .link
            .call(RdpdrServerMessage::Close {
                device_id: self.device,
                file_id,
            })
            .await;
    }

    async fn query(
        &self,
        file_id: u32,
        level: FileInformationClassLevel,
    ) -> FsResult<FileInformationClass> {
        let reply = self
            .link
            .call(RdpdrServerMessage::QueryInformation {
                device_id: self.device,
                file_id,
                info_class: level,
            })
            .await?;
        match reply {
            Reply::Info { status, buffer } => {
                check(status)?;
                buffer.ok_or(FsError::GeneralFailure)
            }
            other => check(other.status()).and(Err(FsError::GeneralFailure)),
        }
    }

    async fn meta_of(&self, file_id: u32) -> FsResult<Meta> {
        let FileInformationClass::Basic(basic) = self
            .query(file_id, FileInformationClassLevel::FILE_BASIC_INFORMATION)
            .await?
        else {
            return Err(FsError::GeneralFailure);
        };
        let FileInformationClass::Standard(standard) = self
            .query(
                file_id,
                FileInformationClassLevel::FILE_STANDARD_INFORMATION,
            )
            .await?
        else {
            return Err(FsError::GeneralFailure);
        };
        Ok(Meta {
            len: u64::try_from(standard.end_of_file).unwrap_or(0),
            modified: time(basic.last_write_time),
            created: time(basic.creation_time),
            dir: basic
                .file_attributes
                .contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY),
        })
    }

    async fn set(&self, file_id: u32, info: FileInformationClass) -> FsResult<()> {
        let reply = self
            .link
            .call(RdpdrServerMessage::SetInformation {
                device_id: self.device,
                file_id,
                set_buffer: info,
            })
            .await?;
        check(reply.status())
    }

    /// Opens `path` with `options`, runs `work`, and closes it either way.
    async fn with_open<T>(
        &self,
        path: String,
        access: DesiredAccess,
        options: CreateOptions,
        work: impl AsyncFnOnce(u32) -> FsResult<T>,
    ) -> FsResult<T> {
        let file_id = self
            .create(path, access, CreateDisposition::FILE_OPEN, options)
            .await?;
        let result = work(file_id).await;
        self.close(file_id).await;
        result
    }

    async fn delete(&self, path: &DavPath, options: CreateOptions) -> FsResult<()> {
        self.with_open(
            rdp_path(path),
            DesiredAccess::DELETE | DesiredAccess::FILE_READ_ATTRIBUTES,
            options,
            async |file_id| {
                self.set(
                    file_id,
                    FileInformationClass::Disposition(FileDispositionInformation {
                        delete_pending: 1,
                    }),
                )
                .await
            },
        )
        .await
    }
}

impl DavFileSystem for DriveFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let writing = options.write || options.append || options.create || options.create_new;
            let access = if writing {
                DesiredAccess::GENERIC_READ | DesiredAccess::GENERIC_WRITE
            } else {
                DesiredAccess::GENERIC_READ
            };
            let disposition = if options.create_new {
                CreateDisposition::FILE_CREATE
            } else if options.create && options.truncate {
                CreateDisposition::FILE_OVERWRITE_IF
            } else if options.create {
                CreateDisposition::FILE_OPEN_IF
            } else if options.truncate {
                CreateDisposition::FILE_OVERWRITE
            } else {
                CreateDisposition::FILE_OPEN
            };
            let file_id = self
                .create(
                    rdp_path(path),
                    access,
                    disposition,
                    CreateOptions::FILE_NON_DIRECTORY_FILE,
                )
                .await?;
            let meta = match self.meta_of(file_id).await {
                Ok(meta) => meta,
                Err(err) => {
                    self.close(file_id).await;
                    return Err(err);
                }
            };
            let position = if options.append { meta.len } else { 0 };
            let file: Box<dyn DavFile> = Box::new(DriveFile {
                fs: self.clone(),
                file_id,
                meta,
                position,
            });
            Ok(file)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let dir = rdp_path(path);
            let pattern = format!("{}\\*", dir.trim_end_matches('\\'));
            let entries = self
                .with_open(
                    dir,
                    DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY
                        | DesiredAccess::FILE_READ_ATTRIBUTES,
                    CreateOptions::FILE_DIRECTORY_FILE,
                    async |file_id| {
                        let mut entries = Vec::new();
                        let mut initial = true;
                        loop {
                            let reply = self
                                .link
                                .call(RdpdrServerMessage::QueryDirectory {
                                    device_id: self.device,
                                    file_id,
                                    info_class:
                                        FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION,
                                    path: pattern.clone(),
                                    initial_query: initial,
                                })
                                .await?;
                            initial = false;
                            let Reply::Info { status, buffer } = reply else {
                                return Err(FsError::GeneralFailure);
                            };
                            if status == NtStatus::NO_MORE_FILES {
                                return Ok(entries);
                            }
                            check(status)?;
                            let Some(FileInformationClass::BothDirectory(info)) = buffer else {
                                return Ok(entries);
                            };
                            let name = info.file_name.trim_end_matches('\0').to_owned();
                            if name == "." || name == ".." || name.is_empty() {
                                continue;
                            }
                            entries.push(Entry {
                                name,
                                meta: Meta {
                                    len: u64::try_from(info.end_of_file).unwrap_or(0),
                                    modified: time(info.last_write_time),
                                    created: time(info.creation_time),
                                    dir: info
                                        .file_attributes
                                        .contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY),
                                },
                            });
                        }
                    },
                )
                .await?;
            let stream: FsStream<Box<dyn DavDirEntry>> = Box::pin(futures_util::stream::iter(
                entries
                    .into_iter()
                    .map(|entry| Ok(Box::new(entry) as Box<dyn DavDirEntry>)),
            ));
            Ok(stream)
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let meta = self
                .with_open(
                    rdp_path(path),
                    DesiredAccess::FILE_READ_ATTRIBUTES,
                    CreateOptions::empty(),
                    async |file_id| self.meta_of(file_id).await,
                )
                .await?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let file_id = self
                .create(
                    rdp_path(path),
                    DesiredAccess::GENERIC_READ | DesiredAccess::GENERIC_WRITE,
                    CreateDisposition::FILE_CREATE,
                    CreateOptions::FILE_DIRECTORY_FILE,
                )
                .await?;
            self.close(file_id).await;
            Ok(())
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.delete(path, CreateOptions::FILE_DIRECTORY_FILE)
            .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.delete(path, CreateOptions::FILE_NON_DIRECTORY_FILE)
            .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let target = rdp_path(to);
            self.with_open(
                rdp_path(from),
                DesiredAccess::DELETE | DesiredAccess::FILE_READ_ATTRIBUTES,
                CreateOptions::empty(),
                async |file_id| {
                    self.set(
                        file_id,
                        FileInformationClass::Rename(FileRenameInformation {
                            replace_if_exists: Boolean::True,
                            file_name: target,
                        }),
                    )
                    .await
                },
            )
            .await
        }
        .boxed()
    }
}

/// An open file on a client drive.
#[derive(Debug)]
struct DriveFile {
    fs: DriveFs,
    file_id: u32,
    meta: Meta,
    position: u64,
}

impl DriveFile {
    async fn write_all(&mut self, data: Bytes) -> FsResult<()> {
        for chunk in data.chunks(CHUNK) {
            let reply = self
                .fs
                .link
                .call(RdpdrServerMessage::Write {
                    device_id: self.fs.device,
                    file_id: self.file_id,
                    data: chunk.to_vec(),
                    offset: self.position,
                })
                .await?;
            let Reply::Write { status, length } = reply else {
                return Err(FsError::GeneralFailure);
            };
            check(status)?;
            if length as usize != chunk.len() {
                return Err(FsError::GeneralFailure);
            }
            self.position += u64::from(length);
            self.meta.len = self.meta.len.max(self.position);
        }
        Ok(())
    }
}

impl Drop for DriveFile {
    fn drop(&mut self) {
        let fs = self.fs.clone();
        let file_id = self.file_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { fs.close(file_id).await });
        }
    }
}

impl DavFile for DriveFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta: Box<dyn DavMetaData> = Box::new(self.meta.clone());
        async move { Ok(meta) }.boxed()
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        let data = buf.copy_to_bytes(buf.remaining());
        self.write_all(data).boxed()
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        self.write_all(buf).boxed()
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let reply = self
                .fs
                .link
                .call(RdpdrServerMessage::Read {
                    device_id: self.fs.device,
                    file_id: self.file_id,
                    length: u32::try_from(count.min(CHUNK)).unwrap_or(0),
                    offset: self.position,
                })
                .await?;
            let Reply::Read { status, data } = reply else {
                return Err(FsError::GeneralFailure);
            };
            if status == NtStatus::END_OF_FILE {
                return Ok(Bytes::new());
            }
            check(status)?;
            self.position += data.len() as u64;
            Ok(Bytes::from(data))
        }
        .boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let target = match pos {
                SeekFrom::Start(offset) => Some(offset),
                SeekFrom::End(delta) => self.meta.len.checked_add_signed(delta),
                SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            };
            self.position = target.ok_or(FsError::GeneralFailure)?;
            Ok(self.position)
        }
        .boxed()
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}

/// A drive served and mounted on the Mac.
struct Mount {
    point: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Mount {
    async fn start(fs: DriveFs, name: &str) -> anyhow::Result<Self> {
        let token = {
            let state = std::collections::hash_map::RandomState::new();
            let mut first = state.build_hasher();
            first.write_u64(u64::from(std::process::id()));
            let mut second = state.build_hasher();
            second.write_u128(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
            format!("{:016x}{:016x}", first.finish(), second.finish())
        };
        let handler = DavHandler::builder()
            .filesystem(Box::new(fs))
            .locksystem(FakeLs::new())
            .strip_prefix(format!("/{token}"))
            .build_handler();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(serve(listener, handler));

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let safe: String = name
            .chars()
            .map(|c| {
                if c == '/' || c == ':' || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let point = home.join("RDP Drives").join(if safe.is_empty() {
            "Drive".into()
        } else {
            safe
        });
        std::fs::create_dir_all(&point)?;
        let url = format!("http://127.0.0.1:{port}/{token}/");
        let volume = name.to_owned();
        let target = point.clone();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("/sbin/mount_webdav")
                .args(["-S", "-v", &volume, &url])
                .arg(&target)
                .status()
        })
        .await??;
        if !status.success() {
            server.abort();
            anyhow::bail!("mount_webdav failed: {status}");
        }
        info!(point = %point.display(), "client drive mounted");
        Ok(Self { point, server })
    }

    fn stop(self) {
        let point = self.point;
        let unmount = std::process::Command::new("/sbin/umount")
            .arg(&point)
            .status();
        if !unmount.is_ok_and(|status| status.success()) {
            let _ = std::process::Command::new("/usr/sbin/diskutil")
                .args(["unmount", "force"])
                .arg(&point)
                .status();
        }
        let _ = std::fs::remove_dir(&point);
        self.server.abort();
        info!(point = %point.display(), "client drive unmounted");
    }
}

async fn serve(listener: tokio::net::TcpListener, handler: DavHandler) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler.handle(request).await) }
            });
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await
            {
                debug!(%err, "WebDAV connection ended");
            }
        });
    }
}

/// The per-connection RDPDR backend: drives come and go with the client.
#[derive(Debug)]
struct DriveBackend {
    link: Arc<Link>,
    mounts: Arc<Mutex<HashMap<u32, MountSlot>>>,
}

/// A mount being set up, or set up.
enum MountSlot {
    Starting,
    Ready(Mount),
}

impl std::fmt::Debug for MountSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Starting => "Starting",
            Self::Ready(_) => "Ready",
        })
    }
}

impl DriveBackend {
    fn unmount(&self, device_id: u32) {
        let slot = self.mounts.lock().expect("poisoned").remove(&device_id);
        if let Some(MountSlot::Ready(mount)) = slot {
            std::thread::spawn(move || mount.stop());
        }
    }
}

impl Drop for DriveBackend {
    fn drop(&mut self) {
        let slots: Vec<MountSlot> = self
            .mounts
            .lock()
            .expect("poisoned")
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in slots {
            if let MountSlot::Ready(mount) = slot {
                mount.stop();
            }
        }
    }
}

impl ironrdp_pdu::ironrdp_core::AsAny for DriveBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl RdpdrServerBackend for DriveBackend {
    fn on_device_announce(&mut self, devices: &[DeviceAnnounceHeader]) -> Vec<(u32, bool)> {
        devices
            .iter()
            .map(|device| {
                let id = device.device_id();
                if device.device_type() != DeviceType::Filesystem {
                    return (id, false);
                }
                let name = device
                    .preferred_dos_name()
                    .trim_end_matches('\0')
                    .to_owned();
                info!(id, %name, "client drive announced");
                self.mounts
                    .lock()
                    .expect("poisoned")
                    .insert(id, MountSlot::Starting);
                let fs = DriveFs {
                    link: Arc::clone(&self.link),
                    device: id,
                };
                let mounts = Arc::clone(&self.mounts);
                tokio::spawn(async move {
                    match Mount::start(fs, &name).await {
                        Ok(mount) => {
                            let mut mounts = mounts.lock().expect("poisoned");
                            // The drive may have gone while it was being mounted.
                            if let Some(slot) = mounts.get_mut(&id) {
                                *slot = MountSlot::Ready(mount);
                            } else {
                                drop(mounts);
                                std::thread::spawn(move || mount.stop());
                            }
                        }
                        Err(err) => {
                            warn!(%err, %name, "cannot mount the client drive");
                            mounts.lock().expect("poisoned").remove(&id);
                        }
                    }
                });
                (id, true)
            })
            .collect()
    }

    fn on_device_remove(&mut self, device_ids: &[u32]) {
        for &id in device_ids {
            self.unmount(id);
        }
    }

    fn on_client_name(&mut self, computer_name: &str) {
        debug!(computer_name, "drive redirection client");
    }

    fn on_create_complete(&mut self, response: &DeviceCreateResponse) -> PduResult<()> {
        self.link.complete(Reply::Create {
            status: response.device_io_reply.io_status,
            file_id: response.file_id,
        });
        Ok(())
    }

    fn on_close_complete(&mut self, response: &DeviceCloseResponse) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_response.io_status));
        Ok(())
    }

    fn on_read_complete(&mut self, response: &DeviceReadResponse) -> PduResult<()> {
        self.link.complete(Reply::Read {
            status: response.device_io_reply.io_status,
            data: response.read_data.clone(),
        });
        Ok(())
    }

    fn on_write_complete(&mut self, response: &DeviceWriteResponse) -> PduResult<()> {
        self.link.complete(Reply::Write {
            status: response.device_io_reply.io_status,
            length: response.length,
        });
        Ok(())
    }

    fn on_flush_buffers_complete(
        &mut self,
        response: &DeviceFlushBuffersResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_response.io_status));
        Ok(())
    }

    fn on_device_control_complete(&mut self, response: &DeviceControlResponse) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_reply.io_status));
        Ok(())
    }

    fn on_query_information_complete(
        &mut self,
        response: &ClientDriveQueryInformationResponse,
    ) -> PduResult<()> {
        self.link.complete(Reply::Info {
            status: response.device_io_response.io_status,
            buffer: response.buffer.clone(),
        });
        Ok(())
    }

    fn on_set_information_complete(
        &mut self,
        response: &ClientDriveSetInformationResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_reply.io_status));
        Ok(())
    }

    fn on_query_directory_complete(
        &mut self,
        response: &ClientDriveQueryDirectoryResponse,
    ) -> PduResult<()> {
        self.link.complete(Reply::Info {
            status: response.device_io_reply.io_status,
            buffer: response.buffer.clone(),
        });
        Ok(())
    }

    fn on_notify_change_directory_complete(
        &mut self,
        response: &ClientDriveNotifyChangeDirectoryResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_reply.io_status));
        Ok(())
    }

    fn on_query_volume_information_complete(
        &mut self,
        response: &ClientDriveQueryVolumeInformationResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_reply.io_status));
        Ok(())
    }

    fn on_lock_control_complete(
        &mut self,
        response: &ClientDriveLockControlResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_reply.io_status));
        Ok(())
    }

    fn on_query_security_complete(
        &mut self,
        response: &ClientDriveQuerySecurityResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_response.io_status));
        Ok(())
    }

    fn on_set_security_complete(
        &mut self,
        response: &ClientDriveSetSecurityResponse,
    ) -> PduResult<()> {
        self.link
            .complete(Reply::Status(response.device_io_response.io_status));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_times() {
        let path = DavPath::new("/Docs/a%20b.txt").unwrap();
        assert_eq!(rdp_path(&path), "\\Docs\\a b.txt");
        assert_eq!(rdp_path(&DavPath::new("/").unwrap()), "\\");
        assert_eq!(rdp_path(&DavPath::new("/Docs/").unwrap()), "\\Docs");
        let filetime = i64::try_from(FILETIME_EPOCH * 10_000_000 + 15).unwrap();
        assert_eq!(time(filetime), UNIX_EPOCH + Duration::from_nanos(1500));
    }

    #[test]
    fn statuses_map_to_webdav_errors() {
        assert!(check(NtStatus::SUCCESS).is_ok());
        assert!(matches!(
            check(NtStatus::NO_SUCH_FILE),
            Err(FsError::NotFound)
        ));
        assert!(matches!(
            check(NtStatus::from(OBJECT_NAME_NOT_FOUND)),
            Err(FsError::NotFound)
        ));
        assert!(matches!(
            check(NtStatus::OBJECT_NAME_COLLISION),
            Err(FsError::Exists)
        ));
        assert!(matches!(
            check(NtStatus::ACCESS_DENIED),
            Err(FsError::Forbidden)
        ));
    }
}

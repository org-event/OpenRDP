//! Files through the clipboard (MS-RDPECLIP file streams), both ways.
//!
//! A Finder copy on the Mac is offered as a file list; the client fetches the
//! contents it needs in ranges, from the files as they are on disk. A copy of
//! files on the client is downloaded into `~/Library/Caches/OpenRDP/Clipboard`
//! and put on the pasteboard as those files, so Finder pastes them like any
//! other copy. Only the latest copy is kept there.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString, NSURL};
use tracing::{info, warn};

/// The longest wire name (`path\name`) a file descriptor holds.
const MAX_NAME: usize = 259;
/// Bytes asked for per file contents request.
const CHUNK: u32 = 1 << 20;
/// A client copy larger than this is not downloaded.
const MAX_DOWNLOAD: u64 = 16 << 30;
/// Seconds between 1601 and 1970, for FILETIME.
const FILETIME_EPOCH: u64 = 11_644_473_600;

/// A Mac file or folder offered to the client, at its index in the list.
#[derive(Debug)]
pub struct LocalFile {
    path: PathBuf,
    descriptor: FileDescriptor,
}

impl LocalFile {
    pub fn descriptor(&self) -> &FileDescriptor {
        &self.descriptor
    }
}

/// The files and folders Finder put on the pasteboard, if any.
pub fn pasteboard_files(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            // SAFETY: AppKit's pasteboard type constant.
            let url = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
            let url = NSURL::URLWithString(&url)?;
            url.path().map(|path| PathBuf::from(path.to_string()))
        })
        .collect()
}

/// The descriptors for `roots` and, below folders, everything inside them,
/// each folder before its contents. Names the protocol cannot carry are left out.
pub fn collect(roots: &[PathBuf]) -> Vec<LocalFile> {
    let mut out = Vec::new();
    for root in roots {
        add(&mut out, root, None);
    }
    out
}

fn add(out: &mut Vec<LocalFile>, path: &Path, parent: Option<&str>) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let wire = parent.map_or(name.len(), |p| p.chars().count() + 1 + name.chars().count());
    if MAX_NAME < wire || name.contains('\\') {
        warn!(path = %path.display(), "name too long for the clipboard; left out");
        return;
    }
    let mut descriptor = FileDescriptor::new(name);
    if let Some(parent) = parent {
        descriptor = descriptor.with_relative_path(parent);
    }
    if let Ok(modified) = metadata.modified() {
        descriptor = descriptor.with_last_write_time(filetime(modified));
    }
    if metadata.is_dir() {
        out.push(LocalFile {
            path: path.to_owned(),
            descriptor: descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY),
        });
        let inner = parent.map_or_else(|| name.to_owned(), |p| format!("{p}\\{name}"));
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            add(out, &child, Some(&inner));
        }
    } else if metadata.is_file() {
        out.push(LocalFile {
            path: path.to_owned(),
            descriptor: descriptor
                .with_attributes(ClipboardFileAttributes::ARCHIVE)
                .with_file_size(metadata.len()),
        });
    }
}

fn filetime(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    (since.as_secs() + FILETIME_EPOCH) * 10_000_000 + u64::from(since.subsec_nanos() / 100)
}

/// Answers a client's request for one of `files`: its size, or a byte range.
pub fn answer(files: &[LocalFile], request: &FileContentsRequest) -> FileContentsResponse<'static> {
    let stream = request.stream_id;
    let file = usize::try_from(request.index)
        .ok()
        .and_then(|index| files.get(index));
    let Some(file) = file else {
        return FileContentsResponse::new_error(stream);
    };
    if request.flags.contains(FileContentsFlags::SIZE) {
        return match std::fs::metadata(&file.path) {
            Ok(metadata) => FileContentsResponse::new_size_response(stream, metadata.len()),
            Err(_) => FileContentsResponse::new_error(stream),
        };
    }
    let read = || -> std::io::Result<Vec<u8>> {
        let mut source = File::open(&file.path)?;
        source.seek(SeekFrom::Start(request.position))?;
        let mut data = Vec::new();
        source
            .take(u64::from(request.requested_size))
            .read_to_end(&mut data)?;
        Ok(data)
    };
    match read() {
        Ok(data) => FileContentsResponse::new_data_response(stream, data),
        Err(err) => {
            warn!(%err, path = %file.path.display(), "cannot read a copied file");
            FileContentsResponse::new_error(stream)
        }
    }
}

/// Where a client file goes below the download folder: the relative path and
/// name as plain components, or `None` for anything that could leave it.
fn relative(descriptor: &FileDescriptor) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    let parts = descriptor
        .relative_path
        .iter()
        .flat_map(|p| p.split('\\'))
        .chain(std::iter::once(descriptor.name.as_str()));
    for part in parts {
        if part.is_empty() || part.contains(['/', '\0']) {
            return None;
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => path.push(part),
            _ => return None,
        }
    }
    Some(path)
}

fn is_directory(descriptor: &FileDescriptor) -> bool {
    descriptor
        .attributes
        .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY))
}

/// What the client copied, being downloaded one file at a time.
pub struct Download {
    folder: PathBuf,
    files: Vec<(FileDescriptor, PathBuf)>,
    lock: Option<u32>,
    next: usize,
    current: Option<Current>,
    stream: u32,
    bytes: u64,
}

struct Current {
    index: usize,
    file: File,
    size: Option<u64>,
    position: u64,
}

/// What a download wants next.
pub enum Step {
    Request(FileContentsRequest),
    /// Everything is on disk: these are the copied files and folders themselves.
    Done(Vec<PathBuf>),
    Failed,
}

impl Download {
    /// Starts over in a fresh folder below `base`, emptying it first; `None`
    /// if the copy cannot be taken.
    pub fn start(
        base: &Path,
        files: &[FileDescriptor],
        lock: Option<u32>,
        first_stream: u32,
    ) -> Option<Self> {
        let total: u64 = files.iter().filter_map(|f| f.file_size).sum();
        if MAX_DOWNLOAD < total {
            warn!(total, "client copy too large to download");
            return None;
        }
        // Only the latest copy is kept.
        let _ = std::fs::remove_dir_all(base);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let folder = base.join(stamp.to_string());
        if let Err(err) = std::fs::create_dir_all(&folder) {
            warn!(%err, "cannot create the clipboard download folder");
            return None;
        }
        let mut accepted = Vec::new();
        for descriptor in files {
            match relative(descriptor) {
                Some(path) => accepted.push((descriptor.clone(), path)),
                None => {
                    warn!(name = %descriptor.name, "unsafe file name in the client copy; left out")
                }
            }
        }
        info!(
            files = accepted.len(),
            total, "downloading the client's files"
        );
        Some(Self {
            folder,
            files: accepted,
            lock,
            next: 0,
            current: None,
            stream: first_stream,
            bytes: 0,
        })
    }

    pub fn stream(&self) -> u32 {
        self.stream
    }

    /// Creates folders and opens the next file; asks for its first bytes.
    pub fn advance(&mut self) -> Step {
        while self.next < self.files.len() {
            let index = self.next;
            self.next += 1;
            let (descriptor, path) = &self.files[index];
            let target = self.folder.join(path);
            if is_directory(descriptor) {
                if let Err(err) = std::fs::create_dir_all(&target) {
                    warn!(%err, "cannot create a copied folder");
                    return Step::Failed;
                }
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = match File::create(&target) {
                Ok(file) => file,
                Err(err) => {
                    warn!(%err, "cannot create a copied file");
                    return Step::Failed;
                }
            };
            let size = descriptor.file_size;
            self.current = Some(Current {
                index,
                file,
                size,
                position: 0,
            });
            if size == Some(0) {
                self.current = None;
                continue;
            }
            return Step::Request(self.request());
        }
        let mut roots: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(descriptor, _)| {
                descriptor
                    .relative_path
                    .as_deref()
                    .is_none_or(str::is_empty)
            })
            .map(|(_, path)| self.folder.join(path))
            .collect();
        roots.dedup();
        info!(
            bytes = self.bytes,
            files = self.files.len(),
            "client files copied to the Mac"
        );
        Step::Done(roots)
    }

    fn request(&mut self) -> FileContentsRequest {
        self.stream = self.stream.wrapping_add(1);
        let current = self.current.as_ref().expect("a file is open");
        let index = i32::try_from(current.index).unwrap_or(i32::MAX);
        match current.size {
            None => FileContentsRequest {
                stream_id: self.stream,
                index,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id: self.lock,
            },
            Some(size) => FileContentsRequest {
                stream_id: self.stream,
                index,
                flags: FileContentsFlags::RANGE,
                position: current.position,
                requested_size: u32::try_from(size - current.position)
                    .unwrap_or(CHUNK)
                    .min(CHUNK),
                data_id: self.lock,
            },
        }
    }

    /// Takes the client's answer for the current stream.
    pub fn receive(&mut self, response: &FileContentsResponse<'_>) -> Step {
        if response.is_error() {
            warn!("the client could not send a copied file");
            return Step::Failed;
        }
        let Some(current) = self.current.as_mut() else {
            return Step::Failed;
        };
        if current.size.is_none() {
            match response.data_as_size() {
                Ok(size) if size <= MAX_DOWNLOAD => current.size = Some(size),
                _ => return Step::Failed,
            }
        } else {
            let data = response.data();
            if data.is_empty() {
                warn!("the client sent an empty range");
                return Step::Failed;
            }
            if let Err(err) = current.file.write_all(data) {
                warn!(%err, "cannot write a copied file");
                return Step::Failed;
            }
            current.position += data.len() as u64;
            self.bytes += data.len() as u64;
        }
        if current.size.is_some_and(|size| current.position >= size) {
            self.current = None;
            return self.advance();
        }
        Step::Request(self.request())
    }
}

/// Where client copies are downloaded.
pub fn download_base() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Caches/OpenRDP/Clipboard"))
}

/// Puts `paths` on the pasteboard as files, as Finder does; returns the change count.
pub fn write_pasteboard(paths: &[PathBuf]) -> isize {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .filter_map(|path| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let item = NSPasteboardItem::new();
            let string = url.absoluteString()?;
            // SAFETY: AppKit's pasteboard type constant.
            item.setString_forType(&string, unsafe { NSPasteboardTypeFileURL });
            Some(ProtocolObject::from_retained(item))
        })
        .collect();
    pasteboard.writeObjects(&NSArray::from_retained_slice(&items));
    pasteboard.changeCount()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, path: Option<&str>) -> FileDescriptor {
        let descriptor = FileDescriptor::new(name);
        match path {
            Some(path) => descriptor.with_relative_path(path),
            None => descriptor,
        }
    }

    #[test]
    fn relative_paths_stay_inside() {
        assert_eq!(
            relative(&named("a.txt", None)),
            Some(PathBuf::from("a.txt"))
        );
        assert_eq!(
            relative(&named("a.txt", Some("dir\\sub"))),
            Some(PathBuf::from("dir/sub/a.txt"))
        );
        for (name, path) in [
            ("..", None),
            ("a", Some("..")),
            ("a", Some("dir\\..\\..")),
            ("/etc/passwd", None),
            ("a/b", None),
            ("", None),
            ("a", Some("dir\\\\x")),
        ] {
            assert_eq!(relative(&named(name, path)), None, "{name:?} {path:?}");
        }
    }

    /// Plays the client's side of a copy of a folder with two files, one larger
    /// than a chunk, and checks what lands on disk.
    #[test]
    fn downloads_a_client_copy_in_chunks() {
        let base = std::env::temp_dir().join(format!("openrdp-dl-{}", std::process::id()));
        let big: Vec<u8> = (0..(CHUNK as usize + 1000))
            .map(|i| (i % 251) as u8)
            .collect();
        let contents: [&[u8]; 3] = [b"", b"small", &big];
        let files = [
            named("docs", None).with_attributes(ClipboardFileAttributes::DIRECTORY),
            named("a.txt", Some("docs")).with_file_size(5),
            // No size: the download asks for it first.
            named("big.bin", None),
        ];
        let mut download = Download::start(&base, &files, Some(3), 10).unwrap();
        let mut step = download.advance();
        let mut requests = 0;
        let roots = loop {
            match step {
                Step::Request(request) => {
                    requests += 1;
                    assert_eq!(request.data_id, Some(3));
                    assert_eq!(request.stream_id, download.stream());
                    let file = contents[usize::try_from(request.index).unwrap()];
                    let response = if request.flags.contains(FileContentsFlags::SIZE) {
                        FileContentsResponse::new_size_response(
                            request.stream_id,
                            file.len() as u64,
                        )
                    } else {
                        let start = usize::try_from(request.position).unwrap();
                        let end = (start + request.requested_size as usize).min(file.len());
                        FileContentsResponse::new_data_response(
                            request.stream_id,
                            file[start..end].to_vec(),
                        )
                    };
                    step = download.receive(&response);
                }
                Step::Done(roots) => break roots,
                Step::Failed => panic!("download failed"),
            }
        };
        // a.txt in one range; big.bin: its size, then two ranges.
        assert_eq!(requests, 4);
        assert_eq!(roots.len(), 2);
        assert!(roots[0].ends_with("docs") && roots[1].ends_with("big.bin"));
        assert_eq!(std::fs::read(roots[0].join("a.txt")).unwrap(), b"small");
        assert_eq!(std::fs::read(&roots[1]).unwrap(), big);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_failed_range_stops_the_download() {
        let base = std::env::temp_dir().join(format!("openrdp-dl-fail-{}", std::process::id()));
        let files = [named("a.txt", None).with_file_size(5)];
        let mut download = Download::start(&base, &files, None, 0).unwrap();
        let Step::Request(request) = download.advance() else {
            panic!("expected a request");
        };
        assert!(matches!(
            download.receive(&FileContentsResponse::new_error(request.stream_id)),
            Step::Failed
        ));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn filetime_counts_from_1601() {
        assert_eq!(filetime(UNIX_EPOCH), FILETIME_EPOCH * 10_000_000);
    }

    #[test]
    fn collect_lists_folders_before_their_files_and_answers_ranges() {
        let dir = std::env::temp_dir().join(format!("openrdp-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("top/inner")).unwrap();
        std::fs::write(dir.join("top/inner/b.txt"), b"hello world").unwrap();
        std::fs::write(dir.join("a.txt"), b"abc").unwrap();

        let files = collect(&[dir.join("top"), dir.join("a.txt")]);
        let names: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.descriptor.relative_path.clone(),
                    f.descriptor.name.clone(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (None, "top".to_owned()),
                (Some("top".to_owned()), "inner".to_owned()),
                (Some("top\\inner".to_owned()), "b.txt".to_owned()),
                (None, "a.txt".to_owned()),
            ]
        );
        assert!(is_directory(&files[0].descriptor));
        assert_eq!(files[2].descriptor.file_size, Some(11));

        let range = answer(
            &files,
            &FileContentsRequest {
                stream_id: 7,
                index: 2,
                flags: FileContentsFlags::RANGE,
                position: 6,
                requested_size: 100,
                data_id: None,
            },
        );
        assert_eq!(range.data(), b"world");
        let size = answer(
            &files,
            &FileContentsRequest {
                stream_id: 8,
                index: 3,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id: None,
            },
        );
        assert_eq!(size.data_as_size().unwrap(), 3);
        assert!(
            answer(
                &files,
                &FileContentsRequest {
                    stream_id: 9,
                    index: 9,
                    flags: FileContentsFlags::RANGE,
                    position: 0,
                    requested_size: 1,
                    data_id: None,
                },
            )
            .is_error()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

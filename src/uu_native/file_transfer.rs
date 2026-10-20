//! UU file-transfer RPC on `FILE_DATA_CHANNEL`.
//!
//! This implements the official browse/upload/download shapes used by the
//! desktop file window: directory listing, resumable upload blocks, download
//! blocks, rename/create/delete and existence checks. Paths are translated
//! from the viewer's Windows-style paths to the Linux filesystem; `..` is
//! removed and a leading drive letter maps to `/`.

use super::wire::{self, Field};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use flate2::write::ZlibEncoder;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

const FT_SUCCEEDED: u64 = 1;
const FT_UNEXPECTED: u64 = 2;
const FT_PROTOCOL: u64 = 3;
const FT_PERMISSION: u64 = 4;
const FT_DISK: u64 = 5;
const FT_IO: u64 = 6;
const FT_CANCELED: u64 = 7;
const FT_NOT_FOUND: u64 = 9;
const FT_DIR: u64 = 0;
const FT_DRIVE: u64 = 3;
const FT_FILE: u64 = 4;
const FOS_OVERRIDE: u64 = 1;
const FOS_DUPLICATE: u64 = 2;
const FOS_SKIP: u64 = 3;
const FOS_RESUME: u64 = 4;
type PendingKey = (u32, u64, u64, u64);

const DOWNLOAD_BLOCK: usize = 32 * 1024;
/// Concurrent block requests sent to the viewer. Together with the bounded
/// writer queue this caps memory and keeps a slow channel from growing an
/// unbounded backlog.
const DOWNLOAD_WINDOW: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TaskId {
    pub task: u64,
    pub index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub rel_path: String,
    pub size: u64,
    pub modified: u64,
}

struct Upload {
    target: PathBuf,
    folder: String,
    strategy: u64,
    files: Vec<FileInfo>,
    open: HashMap<u64, (std::fs::File, PathBuf, u64)>,
}

pub struct FileTransfer {
    out: mpsc::Sender<Vec<u8>>,
    save_path: PathBuf,
    next_request: AtomicU64,
    pending: Mutex<HashMap<PendingKey, oneshot::Sender<Vec<u8>>>>,
    uploads: Mutex<HashMap<u64, Upload>>,
    downloading: Arc<tokio::sync::Mutex<()>>,
    first_block: Mutex<Option<u64>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

fn encode_task(id: TaskId) -> Vec<u8> {
    let mut out = Vec::new();
    wire::int(1, id.task, &mut out);
    wire::int(2, id.index, &mut out);
    out
}

fn parse_task(fields: &[Field]) -> TaskId {
    wire::blob(fields, 1)
        .and_then(wire::decode)
        .map(|fields| TaskId {
            task: wire::uint(&fields, 1).unwrap_or(0),
            index: wire::uint(&fields, 2).unwrap_or(0),
        })
        .unwrap_or_default()
}

fn encode_file(file: &FileInfo) -> Vec<u8> {
    let mut out = Vec::new();
    wire::bytes(1, file.rel_path.as_bytes(), &mut out);
    wire::int(2, file.size, &mut out);
    wire::int(3, file.modified, &mut out);
    out
}

fn parse_file(data: &[u8]) -> Option<FileInfo> {
    let fields = wire::decode(data)?;
    Some(FileInfo {
        rel_path: wire::blob(&fields, 1)
            .and_then(|value| std::str::from_utf8(value).ok())
            .unwrap_or_default()
            .to_owned(),
        size: wire::uint(&fields, 2).unwrap_or(0),
        modified: wire::uint(&fields, 3).unwrap_or(0),
    })
}

fn get_uint(fields: &[Field], number: u32) -> u64 {
    wire::uint(fields, number).unwrap_or(0)
}

fn get_text(fields: &[Field], number: u32) -> String {
    wire::blob(fields, number)
        .and_then(|value| std::str::from_utf8(value).ok())
        .unwrap_or_default()
        .to_owned()
}

fn get_all(fields: &[Field], number: u32) -> Vec<Vec<u8>> {
    fields
        .iter()
        .filter(|field| field.number == number)
        .filter_map(|field| match &field.value {
            wire::Value::Bytes(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn parse_files(inline: Vec<Vec<u8>>, list_data: Option<&[u8]>) -> Vec<FileInfo> {
    let mut files: Vec<FileInfo> = inline.iter().filter_map(|data| parse_file(data)).collect();
    if !files.is_empty() {
        return files;
    }
    let Some(data) = list_data.filter(|data| !data.is_empty()) else {
        return files;
    };
    let mut candidates = Vec::new();
    for mut decoder in [
        Box::new(ZlibDecoder::new(data)) as Box<dyn Read>,
        Box::new(GzDecoder::new(data)) as Box<dyn Read>,
        Box::new(DeflateDecoder::new(data)) as Box<dyn Read>,
        Box::new(data) as Box<dyn Read>,
    ] {
        let mut output = Vec::new();
        if decoder.read_to_end(&mut output).is_ok() {
            candidates.push(output);
        }
    }
    for candidate in candidates {
        if let Some(fields) = wire::decode(&candidate) {
            files = get_all(&fields, 1)
                .iter()
                .filter_map(|data| parse_file(data))
                .collect();
            if !files.is_empty() {
                break;
            }
        }
    }
    files
}

fn encode_list(files: &[FileInfo]) -> Vec<u8> {
    let mut list = Vec::new();
    for file in files {
        wire::bytes(1, &encode_file(file), &mut list);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(&list);
    encoder.finish().unwrap_or_default()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn normalize(path: &str) -> PathBuf {
    let path = path.replace('\\', "/");
    let path = match path.split_once(':') {
        Some((drive, rest))
            if drive.len() <= 1
                && drive
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
                && (rest.is_empty() || rest.starts_with('/')) =>
        {
            rest.to_owned()
        }
        _ => path,
    };
    let (mut root, path) = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => (home(), rest.to_owned()),
        _ => (PathBuf::from("/"), path),
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                root.pop();
            }
            value => root.push(value),
        }
    }
    root
}

fn is_this_pc(path: &str) -> bool {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return true;
    }
    let path = path.trim_end_matches('/');
    path.ends_with(':')
        && path.len() <= 2
        && path[..path.len() - 1]
            .chars()
            .all(|character| character.is_ascii_alphabetic())
}

fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn entry(kind: u64, name: &str, size: u64, modified: u64, full: &str) -> Vec<u8> {
    let mut out = Vec::new();
    wire::int(1, kind, &mut out);
    wire::bytes(2, name.as_bytes(), &mut out);
    wire::int(3, size, &mut out);
    wire::int(4, modified, &mut out);
    wire::bytes(5, full.as_bytes(), &mut out);
    out
}

fn this_pc(save_path: &Path) -> Vec<Vec<u8>> {
    let home = home();
    let mut out = Vec::new();
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    };
    let mut seen = Vec::<PathBuf>::new();
    if save_path.is_dir() && save_path != home {
        out.push(entry(
            FT_DIR,
            &name(save_path),
            0,
            modified(save_path),
            &save_path.to_string_lossy(),
        ));
        seen.push(save_path.to_path_buf());
    }
    for key in ["XDG_DOCUMENTS_DIR", "XDG_DESKTOP_DIR", "XDG_DOWNLOAD_DIR"] {
        if let Some(path) = xdg_dir(key) {
            if seen.iter().any(|seen| seen == &path) {
                continue;
            }
            seen.push(path.clone());
            out.push(entry(
                FT_DIR,
                &name(&path),
                0,
                modified(&path),
                &path.to_string_lossy(),
            ));
        }
    }
    if !seen.iter().any(|seen| seen == &home) {
        out.push(entry(
            FT_DIR,
            &name(&home),
            0,
            modified(&home),
            &home.to_string_lossy(),
        ));
    }
    out.push(entry(FT_DRIVE, "/", 0, 0, "/"));
    out
}

fn xdg_dir(key: &str) -> Option<PathBuf> {
    let home = home();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix(&format!("{key}=")))?
        .trim()
        .trim_matches('"');
    let path = PathBuf::from(value.replace("$HOME", &home.to_string_lossy()));
    (path != home && path.is_dir()).then_some(path)
}

fn list_directory(path: &str, save_path: &Path) -> Result<Vec<Vec<u8>>, u64> {
    if is_this_pc(path) {
        return Ok(this_pc(save_path));
    }
    let directory = normalize(path);
    let entries = std::fs::read_dir(&directory).map_err(|error| io_error(&error))?;
    let mut list = Vec::new();
    for item in entries.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let full = directory.join(&name);
        let Ok(metadata) = std::fs::metadata(&full).or_else(|_| item.metadata()) else {
            continue;
        };
        let kind = if metadata.is_dir() { FT_DIR } else { FT_FILE };
        let size = if metadata.is_dir() { 0 } else { metadata.len() };
        list.push((
            metadata.is_dir(),
            name.to_lowercase(),
            entry(kind, &name, size, modified(&full), &full.to_string_lossy()),
        ));
    }
    list.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(list.into_iter().map(|(_, _, entry)| entry).collect())
}

fn io_error(error: &std::io::Error) -> u64 {
    match error.kind() {
        std::io::ErrorKind::NotFound => FT_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => FT_PERMISSION,
        _ if error.raw_os_error() == Some(libc::ENOSPC) => FT_DISK,
        _ => FT_IO,
    }
}

/// `name (1).ext` style duplicate name for FOS_DUPLICATE.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..10_000 {
        let candidate = path.with_file_name(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn collect_download(path: &Path) -> std::io::Result<(String, Vec<(FileInfo, PathBuf)>)> {
    let metadata = std::fs::metadata(path)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut files = Vec::new();
    if !metadata.is_dir() {
        files.push((
            FileInfo {
                rel_path: name,
                size: metadata.len(),
                modified: modified(path),
            },
            path.to_path_buf(),
        ));
        return Ok((String::new(), files));
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for item in std::fs::read_dir(&directory)?.flatten() {
            let full = item.path();
            let Ok(metadata) = std::fs::metadata(&full) else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(full);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = full
                .strip_prefix(path)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push((
                FileInfo {
                    rel_path: relative,
                    size: metadata.len(),
                    modified: modified(&full),
                },
                full,
            ));
        }
    }
    Ok((name, files))
}

impl FileTransfer {
    pub fn new(out: mpsc::Sender<Vec<u8>>, save_path: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&save_path);
        Arc::new(Self {
            out,
            save_path,
            next_request: AtomicU64::new(1 << 32),
            pending: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            downloading: Arc::new(tokio::sync::Mutex::new(())),
            first_block: Mutex::new(None),
            stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Stop pending downloads/uploads and release the outbound queue.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut uploads) = self.uploads.lock() {
            uploads.clear();
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }

    async fn send(&self, which: u32, body: &[u8]) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let message = wire::message(0, which, body);
        // Await capacity: the caller is the FILE channel's message task, so a
        // slow channel applies backpressure instead of growing an unbounded
        // queue. A closed channel stops the transfer session.
        match self.out.send(message).await {
            Ok(()) => true,
            Err(_) => {
                self.shutdown();
                false
            }
        }
    }

    async fn respond(&self, request_id: u64, case: u32, body: &[u8]) {
        let mut header = Vec::new();
        wire::int(1, request_id, &mut header);
        let mut ftp = Vec::new();
        wire::bytes(case, body, &mut ftp);
        let mut rpc = Vec::new();
        wire::bytes(1, &header, &mut rpc);
        wire::bytes(4, &ftp, &mut rpc);
        self.send(22, &rpc).await;
    }

    async fn request(
        &self,
        case: u32,
        body: Vec<u8>,
        key: (u32, u64, u64, u64),
    ) -> Option<Vec<u8>> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key, tx);
        let mut header = Vec::new();
        wire::int(1, id, &mut header);
        let mut ftp = Vec::new();
        wire::bytes(case, &body, &mut ftp);
        let mut rpc = Vec::new();
        wire::bytes(1, &header, &mut rpc);
        wire::bytes(8, &ftp, &mut rpc);
        self.send(21, &rpc).await;
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(value)) => Some(value),
            _ => {
                self.pending.lock().unwrap().remove(&key);
                None
            }
        }
    }

    pub async fn handle(self: &Arc<Self>, data: &[u8]) -> bool {
        let Some(message) = wire::decode(data) else {
            return false;
        };
        let mut handled = false;
        for field in message {
            match (field.number, &field.value) {
                (21, wire::Value::Bytes(rpc)) => {
                    let Some(rpc) = wire::decode(rpc) else {
                        continue;
                    };
                    let Some(ftp) = wire::blob(&rpc, 8) else {
                        continue;
                    };
                    let request_id = wire::blob(&rpc, 1)
                        .and_then(wire::decode)
                        .and_then(|header| wire::uint(&header, 1))
                        .unwrap_or(0);
                    handled = true;
                    if let Some(ftp) = wire::decode(ftp) {
                        self.on_request(request_id, &ftp).await;
                    }
                }
                (22, wire::Value::Bytes(rpc)) => {
                    let Some(rpc) = wire::decode(rpc) else {
                        continue;
                    };
                    let Some(ftp) = wire::blob(&rpc, 4) else {
                        continue;
                    };
                    handled = true;
                    if let Some((case, id, block)) = response_key(ftp) {
                        if let Some(sender) = self
                            .pending
                            .lock()
                            .unwrap()
                            .remove(&(case, id.task, id.index, block))
                        {
                            let _ = sender.send(ftp.to_vec());
                        }
                    }
                }
                _ => {}
            }
        }
        handled
    }

    async fn on_request(self: &Arc<Self>, request_id: u64, ftp: &[Field]) {
        let Some(case) = ftp
            .iter()
            .find(|field| matches!(field.value, wire::Value::Bytes(_)))
        else {
            return;
        };
        let wire::Value::Bytes(body) = &case.value else {
            return;
        };
        let Some(body) = wire::decode(body) else {
            return;
        };
        match case.number {
            1 => self.receive(request_id, &body).await,
            2 => self.send_request(request_id, &body).await,
            3 => self.file_ask(request_id, &body).await,
            4 => self.file_block(request_id, &body).await,
            5..=8 => self.file_operation(request_id, case.number, &body).await,
            9 => self.complete(request_id, &body).await,
            10 => self.read_dir(request_id, &body).await,
            11 => self.file_exist(request_id, &body).await,
            12 => {
                self.respond(request_id, 5, &{
                    let mut out = Vec::new();
                    wire::int(1, FT_SUCCEEDED, &mut out);
                    out
                })
                .await
            }
            _ => {}
        }
    }

    async fn read_dir(&self, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let path = get_text(fields, 2);
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        wire::bytes(2, path.as_bytes(), &mut body);
        match list_directory(&path, &self.save_path) {
            Ok(entries) => {
                let mut listing = Vec::new();
                for entry in &entries {
                    wire::bytes(1, entry, &mut listing);
                }
                let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                let _ = encoder.write_all(&listing);
                let compressed = encoder.finish().unwrap_or_default();
                if listing.len() + compressed.len() < 200_000 {
                    for entry in &entries {
                        wire::bytes(3, entry, &mut body);
                    }
                }
                wire::int(4, FT_SUCCEEDED, &mut body);
                wire::bytes(5, &compressed, &mut body);
            }
            Err(error) => wire::int(4, error, &mut body),
        }
        self.respond(request_id, 7, &body).await;
    }

    async fn receive(&self, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let raw = get_text(fields, 2);
        let target = if raw.trim().is_empty() || is_this_pc(&raw) {
            self.save_path.clone()
        } else {
            normalize(&raw)
        };
        let folder = get_text(fields, 5);
        let strategy = get_uint(fields, 4);
        let files = parse_files(get_all(fields, 3), wire::blob(fields, 6));
        let error = if target.is_dir() {
            FT_SUCCEEDED
        } else {
            FT_NOT_FOUND
        };
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        for file in &files {
            wire::bytes(2, &encode_file(file), &mut body);
        }
        wire::int(3, error, &mut body);
        if error == FT_SUCCEEDED {
            if !folder.is_empty() {
                let mut directory = target.clone();
                for part in folder.replace('\\', "/").split('/') {
                    match part {
                        "" | "." | ".." => {}
                        value => directory.push(value),
                    }
                }
                let _ = std::fs::create_dir_all(directory);
            }
            self.uploads.lock().unwrap().insert(
                id.task,
                Upload {
                    target,
                    folder,
                    strategy,
                    files,
                    open: HashMap::new(),
                },
            );
        }
        self.respond(request_id, 1, &body).await;
    }

    fn upload_path(upload: &Upload, index: u64) -> Option<PathBuf> {
        let file = upload.files.get(index.checked_sub(1)? as usize)?;
        let relative = file.rel_path.replace('\\', "/");
        let relative =
            if !upload.folder.is_empty() && !relative.starts_with(&format!("{}/", upload.folder)) {
                format!("{}/{}", upload.folder, relative)
            } else {
                relative
            };
        let mut path = upload.target.clone();
        for part in relative.split('/') {
            match part {
                "" | "." | ".." => {}
                value => path.push(value),
            }
        }
        Some(path)
    }

    async fn file_ask(&self, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let size = get_uint(fields, 3);
        let (skip, error, resume) = {
            let mut uploads = self.uploads.lock().unwrap();
            match uploads.get_mut(&id.task) {
                None => (false, FT_PROTOCOL, 0),
                Some(upload) => match Self::upload_path(upload, id.index) {
                    None => (false, FT_PROTOCOL, 0),
                    Some(mut path) => {
                        let existing = std::fs::metadata(&path)
                            .ok()
                            .filter(|metadata| metadata.is_file())
                            .map(|metadata| metadata.len());
                        if existing.is_some() && upload.strategy == FOS_SKIP {
                            (true, FT_SUCCEEDED, 0)
                        } else {
                            if existing.is_some() && upload.strategy == FOS_DUPLICATE {
                                path = unique_path(&path);
                            }
                            // FOS_OVERRIDE truncates; FOS_RESUME continues at the
                            // existing length when it is not larger than the source.
                            let resume = match upload.strategy {
                                FOS_RESUME => {
                                    existing.filter(|existing| *existing <= size).unwrap_or(0)
                                }
                                FOS_OVERRIDE | FOS_DUPLICATE | FOS_SKIP => 0,
                                _ => 0,
                            };
                            let opened = path
                                .parent()
                                .map(std::fs::create_dir_all)
                                .unwrap_or(Ok(()))
                                .and_then(|_| {
                                    std::fs::OpenOptions::new()
                                        .create(true)
                                        .write(true)
                                        .truncate(resume == 0)
                                        .open(&path)
                                })
                                .and_then(|mut file| {
                                    file.seek(SeekFrom::Start(resume))?;
                                    Ok(file)
                                });
                            match opened {
                                Ok(file) => {
                                    upload.open.insert(id.index, (file, path, resume));
                                    (false, FT_SUCCEEDED, resume)
                                }
                                Err(error) => (false, io_error(&error), 0),
                            }
                        }
                    }
                },
            }
        };
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        if skip {
            wire::int(2, 1, &mut body);
        }
        wire::int(3, error, &mut body);
        wire::int(4, resume, &mut body);
        self.respond(request_id, 3, &body).await;
    }

    async fn file_block(&self, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let block = get_uint(fields, 2);
        let data = wire::blob(fields, 3).unwrap_or_default();
        if let Ok(mut first) = self.first_block.lock() {
            if first.is_none() {
                *first = Some(block);
            }
        }
        let error = match self
            .uploads
            .lock()
            .unwrap()
            .get_mut(&id.task)
            .and_then(|upload| upload.open.get_mut(&id.index))
        {
            None => FT_PROTOCOL,
            Some((file, _, written)) => match file.write_all(data) {
                Ok(()) => {
                    *written += data.len() as u64;
                    FT_SUCCEEDED
                }
                Err(error) => io_error(&error),
            },
        };
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        wire::int(2, block, &mut body);
        wire::int(3, error, &mut body);
        wire::int(4, data.len() as u64, &mut body);
        self.respond(request_id, 4, &body).await;
    }

    async fn complete(&self, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let viewer_error = get_uint(fields, 2);
        let code = {
            let mut uploads = self.uploads.lock().unwrap();
            let mut code = FT_SUCCEEDED;
            if let Some(upload) = uploads.get_mut(&id.task) {
                if let Some((file, path, written)) = upload.open.remove(&id.index) {
                    let expected = upload
                        .files
                        .get(id.index.saturating_sub(1) as usize)
                        .map(|file| file.size)
                        .unwrap_or(written);
                    drop(file);
                    if viewer_error != 0 && viewer_error != FT_SUCCEEDED {
                        let _ = std::fs::remove_file(path);
                        code = viewer_error;
                    } else if written != expected {
                        code = FT_IO;
                    }
                }
                if upload.open.is_empty() && id.index as usize >= upload.files.len() {
                    uploads.remove(&id.task);
                }
            }
            code
        };
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        wire::int(2, code, &mut body);
        self.respond(request_id, 6, &body).await;
    }

    async fn file_operation(&self, request_id: u64, case: u32, fields: &[Field]) {
        let raw = get_text(fields, if case == 8 { 2 } else { 1 });
        let path = normalize(&raw);
        let result = match case {
            5 => {
                let new_name = get_text(fields, 2);
                if new_name.is_empty() || new_name.contains('/') || new_name.contains('\\') {
                    Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
                } else {
                    std::fs::rename(&path, path.with_file_name(new_name))
                }
            }
            6 => std::fs::create_dir_all(&path),
            7 => std::fs::remove_file(&path),
            _ => match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(error) => Err(error),
            },
        };
        let code = result.as_ref().map_or_else(io_error, |_| FT_SUCCEEDED);
        if case == 8 {
            let mut body = Vec::new();
            wire::bytes(1, &encode_task(parse_task(fields)), &mut body);
            wire::int(2, code, &mut body);
            self.respond(request_id, 6, &body).await;
            return;
        }
        let mut body = Vec::new();
        wire::int(1, code, &mut body);
        if let Err(error) = result {
            wire::bytes(2, error.to_string().as_bytes(), &mut body);
        }
        wire::bytes(3, raw.as_bytes(), &mut body);
        self.respond(request_id, 5, &body).await;
    }

    async fn file_exist(&self, request_id: u64, fields: &[Field]) {
        let raw = get_text(fields, 1);
        let directory = normalize(&raw);
        let mut body = Vec::new();
        wire::bytes(1, raw.as_bytes(), &mut body);
        for name in get_all(fields, 2) {
            let Ok(name) = std::str::from_utf8(&name) else {
                continue;
            };
            let exists = directory.join(name).exists();
            let mut result = Vec::new();
            wire::bytes(1, name.as_bytes(), &mut result);
            if exists {
                wire::int(2, 1, &mut result);
            }
            wire::bytes(2, &result, &mut body);
        }
        self.respond(request_id, 8, &body).await;
    }

    async fn send_request(self: &Arc<Self>, request_id: u64, fields: &[Field]) {
        let id = parse_task(fields);
        let path = normalize(&get_text(fields, 2));
        let collected = collect_download(&path);
        let mut body = Vec::new();
        wire::bytes(1, &encode_task(id), &mut body);
        match collected {
            Ok((folder, files)) => {
                wire::int(2, FT_SUCCEEDED, &mut body);
                let infos: Vec<FileInfo> = files.iter().map(|(file, _)| file.clone()).collect();
                for file in &infos {
                    wire::bytes(3, &encode_file(file), &mut body);
                }
                wire::bytes(4, folder.as_bytes(), &mut body);
                wire::bytes(5, &encode_list(&infos), &mut body);
                self.respond(request_id, 2, &body).await;
                let transfer = self.clone();
                tokio::spawn(async move {
                    let _guard = transfer.downloading.lock().await;
                    let ok = transfer.download_files(id.task, files).await;
                    let mut done = Vec::new();
                    wire::bytes(
                        1,
                        &encode_task(TaskId {
                            task: id.task,
                            index: u64::MAX,
                        }),
                        &mut done,
                    );
                    wire::int(2, if ok { FT_SUCCEEDED } else { FT_UNEXPECTED }, &mut done);
                    let mut header = Vec::new();
                    wire::int(
                        1,
                        transfer.next_request.fetch_add(1, Ordering::Relaxed),
                        &mut header,
                    );
                    let mut ftp = Vec::new();
                    wire::bytes(9, &done, &mut ftp);
                    let mut rpc = Vec::new();
                    wire::bytes(1, &header, &mut rpc);
                    wire::bytes(8, &ftp, &mut rpc);
                    transfer.send(21, &rpc).await;
                });
            }
            Err(code) => {
                wire::int(2, io_error(&code), &mut body);
                self.respond(request_id, 2, &body).await;
            }
        }
    }

    async fn download_files(self: &Arc<Self>, task: u64, files: Vec<(FileInfo, PathBuf)>) -> bool {
        let first_block = self.first_block.lock().unwrap().unwrap_or(0);
        for (index, (info, path)) in files.iter().enumerate() {
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            let id = TaskId {
                task,
                index: index as u64 + 1,
            };
            let mut ask = Vec::new();
            wire::bytes(1, &encode_task(id), &mut ask);
            wire::int(2, info.modified, &mut ask);
            wire::int(3, info.size, &mut ask);
            let Some(confirm) = self.request(3, ask, (3, task, id.index, 0)).await else {
                return false;
            };
            let Some(confirm) = wire::decode(&confirm) else {
                return false;
            };
            let skip = get_uint(&confirm, 2) != 0;
            let error = get_uint(&confirm, 3);
            let resume = get_uint(&confirm, 4);
            if error == FT_CANCELED {
                return false;
            }
            if skip || (error != 0 && error != FT_SUCCEEDED) {
                continue;
            }
            let result = self
                .send_file(id, path, resume.min(info.size), first_block)
                .await;
            let code = result.err().unwrap_or(FT_SUCCEEDED);
            let mut done = Vec::new();
            wire::bytes(1, &encode_task(id), &mut done);
            wire::int(2, code, &mut done);
            let _ = self.request(9, done, (6, task, id.index, 0)).await;
            if code == FT_CANCELED {
                return false;
            }
        }
        true
    }

    async fn send_file(
        self: &Arc<Self>,
        id: TaskId,
        path: &Path,
        resume: u64,
        first_block: u64,
    ) -> Result<(), u64> {
        let mut file = std::fs::File::open(path).map_err(|error| io_error(&error))?;
        file.seek(SeekFrom::Start(resume))
            .map_err(|error| io_error(&error))?;
        let mut block_id = first_block + resume / DOWNLOAD_BLOCK as u64;
        let mut inflight = tokio::task::JoinSet::new();
        loop {
            if self.stopped.load(Ordering::Acquire) {
                inflight.abort_all();
                return Err(FT_CANCELED);
            }
            let mut buffer = vec![0u8; DOWNLOAD_BLOCK];
            let length = file.read(&mut buffer).map_err(|error| io_error(&error))?;
            if length == 0 {
                break;
            }
            buffer.truncate(length);
            while inflight.len() >= DOWNLOAD_WINDOW {
                match inflight.join_next().await {
                    Some(Ok(FT_SUCCEEDED)) => {}
                    Some(Ok(code)) => return Err(code),
                    Some(Err(_)) => return Err(FT_IO),
                    None => break,
                }
            }
            let mut body = Vec::new();
            wire::bytes(1, &encode_task(id), &mut body);
            wire::int(2, block_id, &mut body);
            wire::bytes(3, &buffer, &mut body);
            let key = (4, id.task, id.index, block_id);
            let transfer = self.clone();
            inflight.spawn(async move {
                match transfer.request(4, body, key).await {
                    Some(response) => wire::decode(&response)
                        .map(|fields| {
                            let code = get_uint(&fields, 3);
                            if code == 0 {
                                FT_SUCCEEDED
                            } else {
                                code
                            }
                        })
                        .unwrap_or(FT_IO),
                    None => FT_IO,
                }
            });
            block_id += 1;
        }
        while let Some(result) = inflight.join_next().await {
            match result {
                Ok(FT_SUCCEEDED) => {}
                Ok(code) => return Err(code),
                Err(_) => return Err(FT_IO),
            }
        }
        Ok(())
    }
}

fn response_key(ftp: &[u8]) -> Option<(u32, TaskId, u64)> {
    let fields = wire::decode(ftp)?;
    let case = fields
        .iter()
        .find(|field| matches!(field.value, wire::Value::Bytes(_)))?;
    let wire::Value::Bytes(inner) = &case.value else {
        return None;
    };
    let inner = wire::decode(inner)?;
    Some((
        case.number,
        parse_task(&inner),
        if case.number == 4 {
            get_uint(&inner, 2)
        } else {
            0
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_windows_paths_without_escaping_root() {
        assert_eq!(
            normalize("C:\\..\\home\\a\\.\\b"),
            PathBuf::from("/home/a/b")
        );
        assert_eq!(normalize("/tmp//x/../y"), PathBuf::from("/tmp/y"));
        assert_eq!(normalize(":/"), PathBuf::from("/"));
        assert!(is_this_pc(":/") && is_this_pc("C:\\") && is_this_pc(""));
    }
}

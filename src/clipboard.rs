//! Clipboard sharing: text, images and files copied on one device can be
//! pasted on the others.
//!
//! A watcher thread notices when the local clipboard changes and the new
//! content is sent to every connected device, once per device. Content
//! received from another device is put on the local clipboard; received files
//! are stored in a temporary folder first, and the clipboard refers to them.
//!
//! Clipboards travel over TLS on the Lan Mouse port (TCP), separately from
//! the DTLS input events, which are limited to small datagrams. A device
//! connects to the clients it controls; both sides send over that connection.
//! Like input, only authorized devices may connect.
//!
//! Every message is `[kind: u8][length: u32][payload]`. Images are PNG.
//! Files are a JSON manifest message followed by the contents of each file in
//! data messages. Messages of kinds a device doesn't know are skipped.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, RwLock, mpsc as std_mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use local_channel::mpsc::{Sender, channel};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::WebPkiSupportedAlgorithms,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc as tokio_mpsc,
    task::{JoinHandle, spawn_local},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;

use crate::{client::ClientManager, crypto};

/// how often the local clipboard is checked for changes
const CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// how often the connections to controlled clients are checked
const CONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// largest message accepted, e.g. a big text or image
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// size of the data messages file contents are sent in
const FILE_CHUNK: usize = 1024 * 1024;
/// whether this platform can tell cheaply that the clipboard changed;
/// images and files are only shared where it can
const DETECTS_CHANGES: bool = cfg!(any(windows, target_os = "macos"));

const KIND_TEXT: u8 = 1;
const KIND_IMAGE: u8 = 2;
const KIND_FILES: u8 = 3;
const KIND_FILE_DATA: u8 = 4;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("TLS setup failed: `{0}`")]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// clipboard content, as taken from or put on the local clipboard
#[derive(Debug, PartialEq)]
enum Content {
    Text(String),
    /// PNG encoded image
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
}

/// content to send to other devices
#[derive(Clone)]
enum Outgoing {
    Text(Rc<str>),
    Image(Rc<[u8]>),
    Files(Rc<FileTransfer>),
}

/// copied files and folders to send
#[derive(Debug, Default)]
struct FileTransfer {
    manifest: Vec<FileEntry>,
    /// where the contents of each manifest entry come from (None for folders)
    sources: Vec<Option<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FileEntry {
    /// path relative to the folder the files were copied from, `/` separated
    path: String,
    /// size in bytes, 0 for folders
    size: u64,
    dir: bool,
}

pub(crate) struct ClipboardSync {
    enabled: Rc<Cell<bool>>,
    tasks: Vec<JoinHandle<()>>,
    /// stops the watcher thread when dropped
    _set_tx: std_mpsc::Sender<Content>,
}

impl ClipboardSync {
    /// `max_file_bytes`: copied files are only shared up to this total size
    pub(crate) fn new(
        enabled: bool,
        port: u16,
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        client_manager: ClientManager,
        max_file_bytes: u64,
    ) -> Result<Self, ClipboardError> {
        let (acceptor, connector) = tls_config(cert, authorized_keys)?;
        let enabled = Rc::new(Cell::new(enabled));

        // files received by an earlier run are no longer on the clipboard
        let receive_root = std::env::temp_dir().join("lan-mouse-clipboard");
        let _ = std::fs::remove_dir_all(&receive_root);

        // the watcher thread owns the clipboard
        let (changed_tx, mut changed_rx) = tokio_mpsc::unbounded_channel();
        let (set_tx, set_rx) = std_mpsc::channel();
        thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || watch_clipboard(changed_tx, set_rx))?;

        let shared = Rc::new(Shared {
            enabled: enabled.clone(),
            set_tx: set_tx.clone(),
            writers: Default::default(),
            next_id: Default::default(),
            max_file_bytes,
            receive_root,
        });

        let listen_task = spawn_local(listen(port, acceptor, shared.clone()));
        let connect_task = spawn_local(connect_to_clients(
            connector,
            client_manager,
            shared.clone(),
        ));
        // send local changes to all connected devices
        let share_task = spawn_local(async move {
            while let Some(content) = changed_rx.recv().await {
                if !shared.enabled.get() {
                    continue;
                }
                let outgoing = match content {
                    Content::Text(text) => Outgoing::Text(text.into()),
                    Content::Image(png) => Outgoing::Image(png.into()),
                    Content::Files(paths) => {
                        let limit = shared.max_file_bytes;
                        let collect = move || FileTransfer::collect(&paths, limit);
                        match tokio::task::spawn_blocking(collect).await {
                            Ok(Ok(transfer)) => Outgoing::Files(Rc::new(transfer)),
                            Ok(Err(e)) => {
                                log::info!("not sharing the copied files: {e}");
                                continue;
                            }
                            Err(_) => continue,
                        }
                    }
                };
                shared.send(outgoing);
            }
        });

        Ok(Self {
            enabled,
            tasks: vec![listen_task, connect_task, share_task],
            _set_tx: set_tx,
        })
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.set(enabled);
    }

    pub(crate) fn terminate(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// a connection's way to send clipboards to its device
struct Writer {
    addr: SocketAddr,
    /// certificate fingerprint of the device
    device: String,
    tx: Sender<Outgoing>,
}

/// state shared by the connection tasks
struct Shared {
    enabled: Rc<Cell<bool>>,
    /// puts content on the local clipboard
    set_tx: std_mpsc::Sender<Content>,
    /// by connection id
    writers: RefCell<HashMap<u64, Writer>>,
    next_id: Cell<u64>,
    max_file_bytes: u64,
    /// received files are stored in a new folder in here
    receive_root: PathBuf,
}

impl Shared {
    /// send to every device once, even if there are several connections to it
    /// (e.g. both devices control each other)
    fn send(&self, outgoing: Outgoing) {
        let mut sent = HashSet::new();
        for writer in self.writers.borrow().values() {
            if sent.insert(writer.device.clone()) {
                log::debug!("sending clipboard to {}", writer.addr);
                let _ = writer.tx.send(outgoing.clone());
            }
        }
    }

    /// put content received from another device on the local clipboard
    fn receive(&self, content: Content, addr: SocketAddr) {
        if self.enabled.get() {
            log::debug!("received clipboard from {addr}");
            let _ = self.set_tx.send(content);
        }
    }
}

/// accept connections from devices controlling this one
async fn listen(port: u16, acceptor: TlsAcceptor, shared: Rc<Shared>) {
    let listener = match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
        Ok(listener) => listener,
        Err(e) => return log::warn!("clipboard sharing: could not listen on TCP port {port}: {e}"),
    };
    log::info!("sharing the clipboard on TCP port {port}");
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                log::warn!("clipboard sharing: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let shared = shared.clone();
        spawn_local(async move {
            match acceptor.accept(stream).await {
                Ok(stream) => {
                    let device = device_of(stream.get_ref().1.peer_certificates());
                    serve(stream, addr, device, shared).await
                }
                Err(e) => log::warn!("clipboard sharing: rejected {addr}: {e}"),
            }
        });
    }
}

/// keep a connection to every client this device is connected to for input
async fn connect_to_clients(
    connector: TlsConnector,
    client_manager: ClientManager,
    shared: Rc<Shared>,
) {
    let connected: Rc<RefCell<HashSet<SocketAddr>>> = Default::default();
    let mut interval = tokio::time::interval(CONNECT_INTERVAL);
    loop {
        interval.tick().await;
        for handle in client_manager.active_clients() {
            // the address input currently goes to, i.e. a working connection
            let Some(addr) = client_manager.active_addr(handle) else {
                continue;
            };
            if !connected.borrow_mut().insert(addr) {
                continue;
            }
            let connector = connector.clone();
            let shared = shared.clone();
            let connected = connected.clone();
            spawn_local(async move {
                match connect(&connector, addr).await {
                    Ok(stream) => {
                        let device = device_of(stream.get_ref().1.peer_certificates());
                        serve(stream, addr, device, shared).await
                    }
                    Err(e) => log::debug!("clipboard sharing: could not connect to {addr}: {e}"),
                }
                connected.borrow_mut().remove(&addr);
            });
        }
    }
}

async fn connect(
    connector: &TlsConnector,
    addr: SocketAddr,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let stream = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("lan-mouse").expect("valid name");
    connector.connect(name, stream).await
}

/// fingerprint of the device at the other end of a connection
fn device_of(certificates: Option<&[CertificateDer<'_>]>) -> String {
    certificates
        .and_then(|certificates| certificates.first())
        .map(|certificate| crypto::generate_fingerprint(certificate))
        .unwrap_or_default()
}

/// exchange clipboards over an established connection until it closes
async fn serve<S>(stream: S, addr: SocketAddr, device: String, shared: Rc<Shared>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    log::info!("sharing the clipboard with {addr}");
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = channel::<Outgoing>();
    let id = shared.next_id.get();
    shared.next_id.set(id + 1);
    shared
        .writers
        .borrow_mut()
        .insert(id, Writer { addr, device, tx });

    let receive = async {
        loop {
            let (kind, data) = match read_message(&mut reader).await {
                Ok(message) => message,
                Err(e) => break e,
            };
            match kind {
                KIND_TEXT => match String::from_utf8(data) {
                    Ok(text) => shared.receive(Content::Text(text), addr),
                    Err(e) => break invalid(e),
                },
                KIND_IMAGE => shared.receive(Content::Image(data), addr),
                KIND_FILES => {
                    let accept = shared.enabled.get();
                    let received = receive_files(
                        &mut reader,
                        &data,
                        &shared.receive_root,
                        accept,
                        shared.max_file_bytes,
                    )
                    .await;
                    match received {
                        Ok(Some(paths)) => {
                            log::info!("received {} copied item(s) from {addr}", paths.len());
                            shared.receive(Content::Files(paths), addr);
                        }
                        Ok(None) => {}
                        Err(e) => break e,
                    }
                }
                // file data outside a transfer, or a kind from a newer version
                _ => {}
            }
        }
    };
    let send = async {
        while let Some(outgoing) = rx.recv().await {
            if let Err(e) = write_outgoing(&mut writer, &outgoing).await {
                return e;
            }
        }
        io::Error::from(io::ErrorKind::BrokenPipe)
    };
    let e = tokio::select! {
        e = receive => e,
        e = send => e,
    };
    shared.writers.borrow_mut().remove(&id);
    log::info!("stopped sharing the clipboard with {addr}: {e}");
}

fn invalid(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    writer.write_u8(kind).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let kind = reader.read_u8().await?;
    let len = reader.read_u32().await? as usize;
    if len > MAX_MESSAGE {
        return Err(invalid("clipboard message too large"));
    }
    let mut data = vec![0; len];
    reader.read_exact(&mut data).await?;
    Ok((kind, data))
}

async fn write_outgoing<W: AsyncWrite + Unpin>(
    writer: &mut W,
    outgoing: &Outgoing,
) -> io::Result<()> {
    match outgoing {
        Outgoing::Text(text) => write_message(writer, KIND_TEXT, text.as_bytes()).await?,
        Outgoing::Image(png) => write_message(writer, KIND_IMAGE, png).await?,
        Outgoing::Files(transfer) => send_files(writer, transfer).await?,
    }
    writer.flush().await
}

async fn send_files<W: AsyncWrite + Unpin>(
    writer: &mut W,
    transfer: &FileTransfer,
) -> io::Result<()> {
    let manifest = serde_json::to_vec(&transfer.manifest)?;
    write_message(writer, KIND_FILES, &manifest).await?;
    let mut buf = vec![0; FILE_CHUNK];
    for (entry, source) in transfer.manifest.iter().zip(&transfer.sources) {
        let Some(source) = source else {
            continue;
        };
        let mut file = tokio::fs::File::open(source).await?;
        let mut remaining = entry.size;
        while remaining > 0 {
            let want = remaining.min(FILE_CHUNK as u64) as usize;
            let n = file.read(&mut buf[..want]).await?;
            if n == 0 {
                // the receiver can't tell, so the connection has to go
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{} got smaller while sending it", source.display()),
                ));
            }
            write_message(writer, KIND_FILE_DATA, &buf[..n]).await?;
            remaining -= n as u64;
        }
    }
    Ok(())
}

/// Receive the files announced by `manifest` into a new folder in
/// `receive_root` and return the copied items (the top-level files and
/// folders). Without `accept`, or when they are too large, the files are
/// read but dropped and `None` is returned.
async fn receive_files<R: AsyncRead + Unpin>(
    reader: &mut R,
    manifest: &[u8],
    receive_root: &Path,
    accept: bool,
    max_file_bytes: u64,
) -> io::Result<Option<Vec<PathBuf>>> {
    let manifest: Vec<FileEntry> = serde_json::from_slice(manifest)?;
    let paths = manifest
        .iter()
        .map(|entry| safe_relative_path(&entry.path).ok_or_else(|| invalid("unsafe file name")))
        .collect::<io::Result<Vec<_>>>()?;
    let total: u64 = manifest.iter().map(|entry| entry.size).sum();
    let folder = if accept && total <= max_file_bytes {
        Some(new_receive_folder(receive_root)?)
    } else {
        log::info!(
            "ignoring {} received file(s) ({total} bytes)",
            manifest.len()
        );
        None
    };

    for (entry, path) in manifest.iter().zip(&paths) {
        let target = folder.as_ref().map(|folder| folder.join(path));
        if entry.dir {
            if let Some(target) = &target {
                tokio::fs::create_dir_all(target).await?;
            }
            continue;
        }
        let mut file = match &target {
            Some(target) => {
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                Some(tokio::fs::File::create(target).await?)
            }
            None => None,
        };
        let mut remaining = entry.size;
        while remaining > 0 {
            let (kind, data) = read_message(reader).await?;
            if kind != KIND_FILE_DATA || data.len() as u64 > remaining {
                return Err(invalid("unexpected message in a file transfer"));
            }
            if let Some(file) = &mut file {
                file.write_all(&data).await?;
            }
            remaining -= data.len() as u64;
        }
        if let Some(file) = &mut file {
            file.flush().await?;
        }
    }

    let Some(folder) = folder else {
        return Ok(None);
    };
    remove_other_folders(receive_root, &folder);
    let mut items = vec![];
    for path in &paths {
        if let Some(first) = path.components().next() {
            let item = folder.join(first);
            if !items.contains(&item) {
                items.push(item);
            }
        }
    }
    Ok(Some(items))
}

/// a relative path that stays inside the folder it is joined to
fn safe_relative_path(path: &str) -> Option<PathBuf> {
    let mut safe = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '\0']) {
            return None;
        }
        safe.push(part);
    }
    (!safe.as_os_str().is_empty()).then_some(safe)
}

fn new_receive_folder(receive_root: &Path) -> io::Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let folder = receive_root.join(now.to_string());
    std::fs::create_dir_all(&folder)?;
    Ok(folder)
}

/// earlier received files are no longer on the clipboard
fn remove_other_folders(receive_root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(receive_root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl FileTransfer {
    /// the copied files and folders (recursively), up to `limit` bytes in total
    fn collect(paths: &[PathBuf], limit: u64) -> Result<Self, String> {
        if limit == 0 {
            return Err("file sharing is turned off".into());
        }
        let mut transfer = Self::default();
        let mut total = 0;
        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("unsupported file name: {}", path.display()))?;
            transfer.add(path, name.to_owned(), &mut total, limit)?;
        }
        Ok(transfer)
    }

    fn add(
        &mut self,
        path: &Path,
        relative: String,
        total: &mut u64,
        limit: u64,
    ) -> Result<(), String> {
        let error = |e: io::Error| format!("{}: {e}", path.display());
        let metadata = std::fs::symlink_metadata(path).map_err(error)?;
        if metadata.is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            self.manifest.push(FileEntry {
                path: relative.clone(),
                size: 0,
                dir: true,
            });
            self.sources.push(None);
            let mut children = std::fs::read_dir(path)
                .map_err(error)?
                .flatten()
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                let name = child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| format!("unsupported file name: {}", child.display()))?;
                self.add(&child, format!("{relative}/{name}"), total, limit)?;
            }
        } else {
            *total += metadata.len();
            if *total > limit {
                return Err(format!("larger than {} MB", limit / (1024 * 1024)));
            }
            self.manifest.push(FileEntry {
                path: relative,
                size: metadata.len(),
                dir: false,
            });
            self.sources.push(Some(path.to_owned()));
        }
        Ok(())
    }
}

/// Watches the clipboard and reports new content; puts content received from
/// other devices on the clipboard. Runs until `set_rx` is disconnected.
fn watch_clipboard(
    changed_tx: tokio_mpsc::UnboundedSender<Content>,
    set_rx: std_mpsc::Receiver<Content>,
) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => return log::warn!("clipboard sharing unavailable: {e}"),
    };
    // starts from the current state: what's on the clipboard now isn't shared
    let mut change = ChangeDetector::new();
    let mut last_text = clipboard.get_text().ok();
    loop {
        match set_rx.recv_timeout(CHECK_INTERVAL) {
            Ok(content) => {
                if let Err(e) = put(&mut clipboard, content, &mut last_text) {
                    log::warn!("could not set the clipboard: {e}");
                }
                // a remote clipboard showing up locally is not a new copy
                change.reset();
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if !change.changed() {
            continue;
        }
        if let Some(content) = take(&mut clipboard, &mut last_text) {
            if changed_tx.send(content).is_err() {
                return;
            }
        }
    }
}

fn put(
    clipboard: &mut arboard::Clipboard,
    content: Content,
    last_text: &mut Option<String>,
) -> Result<(), String> {
    match content {
        Content::Text(text) => {
            clipboard
                .set_text(text.clone())
                .map_err(|e| e.to_string())?;
            *last_text = Some(text);
        }
        Content::Image(png) => {
            let image = decode_png(&png)?;
            clipboard.set_image(image).map_err(|e| e.to_string())?;
        }
        Content::Files(paths) => {
            clipboard
                .set()
                .file_list(&paths)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// what was copied: files, text or an image, in that order, since copying
/// a file also copies its name as text and copying text in some programs
/// also copies a picture of it
fn take(clipboard: &mut arboard::Clipboard, last_text: &mut Option<String>) -> Option<Content> {
    if DETECTS_CHANGES {
        if let Ok(files) = clipboard.get().file_list() {
            if !files.is_empty() {
                return Some(Content::Files(files));
            }
        }
    }
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            // without change detection, unchanged text is checked every time
            if !DETECTS_CHANGES && last_text.as_deref() == Some(text.as_str()) {
                return None;
            }
            *last_text = Some(text.clone());
            return (text.len() <= MAX_MESSAGE).then_some(Content::Text(text));
        }
    }
    if DETECTS_CHANGES {
        if let Ok(image) = clipboard.get_image() {
            return encode_png(&image)
                .ok()
                .filter(|png| png.len() <= MAX_MESSAGE)
                .map(Content::Image);
        }
    }
    None
}

fn encode_png(image: &arboard::ImageData) -> Result<Vec<u8>, String> {
    use image::{
        ExtendedColorType, ImageEncoder,
        codecs::png::{CompressionType, FilterType, PngEncoder},
    };
    let mut png = Vec::new();
    // fast compression: it only travels over the local network
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            &image.bytes,
            image.width as u32,
            image.height as u32,
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
    Ok(png)
}

fn decode_png(png: &[u8]) -> Result<arboard::ImageData<'static>, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    Ok(arboard::ImageData {
        width: image.width() as usize,
        height: image.height() as usize,
        bytes: image.into_raw().into(),
    })
}

/// Tells whether the clipboard changed since the last check without opening
/// it (which would block other programs from it twice a second), using the
/// clipboard's change counter on Windows and macOS.
struct ChangeDetector {
    last: Option<i64>,
    #[cfg(target_os = "macos")]
    pasteboard: Option<objc2::rc::Retained<objc2_app_kit::NSPasteboard>>,
}

impl ChangeDetector {
    fn new() -> Self {
        let mut detector = Self {
            last: None,
            #[cfg(target_os = "macos")]
            pasteboard: {
                use objc2::{ClassType, msg_send};
                unsafe { msg_send![objc2_app_kit::NSPasteboard::class(), generalPasteboard] }
            },
        };
        detector.reset();
        detector
    }

    #[cfg(windows)]
    fn counter(&self) -> Option<i64> {
        let sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        Some(sequence as i64)
    }

    #[cfg(target_os = "macos")]
    fn counter(&self) -> Option<i64> {
        use objc2::msg_send;
        let pasteboard = self.pasteboard.as_ref()?;
        let count: isize = unsafe { msg_send![&**pasteboard, changeCount] };
        Some(count as i64)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn counter(&self) -> Option<i64> {
        None
    }

    fn changed(&mut self) -> bool {
        match self.counter() {
            Some(counter) => {
                let changed = self.last != Some(counter);
                self.last = Some(counter);
                changed
            }
            // can't tell, the caller compares the content instead
            None => true,
        }
    }

    /// forget changes up to now, e.g. made by ourselves
    fn reset(&mut self) {
        self.last = self.counter();
    }
}

/// TLS for clipboard connections, using the same certificate as input.
/// Incoming connections must come from an authorized device.
fn tls_config(
    cert: &Certificate,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
) -> Result<(TlsAcceptor, TlsConnector), ClipboardError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = cert.certificate.clone();
    let key = || -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into()
    };
    let schemes = provider.signature_verification_algorithms;

    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(AuthorizedDevices {
            authorized_keys,
            schemes,
        }))
        .with_single_cert(certs.clone(), key())?;
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyDevice { schemes }))
        .with_client_auth_cert(certs, key())?;
    Ok((
        TlsAcceptor::from(Arc::new(server)),
        TlsConnector::from(Arc::new(client)),
    ))
}

/// accepts devices whose certificate fingerprint is authorized
#[derive(Debug)]
struct AuthorizedDevices {
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    schemes: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AuthorizedDevices {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let fingerprint = crypto::generate_fingerprint(end_entity);
        if self
            .authorized_keys
            .read()
            .expect("lock")
            .contains_key(&fingerprint)
        {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "device {fingerprint} is not authorized"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.schemes)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.schemes)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.supported_schemes()
    }
}

/// Accepts any device, like the input connection does: the device we
/// connect to is the one the user configured, and it has to authorize us.
#[derive(Debug)]
struct AnyDevice {
    schemes: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AnyDevice {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.schemes)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.schemes)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a fresh, empty folder for a test
    fn test_folder(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("lan-mouse-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[tokio::test]
    async fn message_round_trip() {
        let mut buf = Vec::new();
        write_message(&mut buf, KIND_TEXT, "héllo 🌍".as_bytes())
            .await
            .unwrap();
        let mut reader = buf.as_slice();
        let (kind, data) = read_message(&mut reader).await.unwrap();
        assert_eq!(kind, KIND_TEXT);
        assert_eq!(String::from_utf8(data).unwrap(), "héllo 🌍");
    }

    #[tokio::test]
    async fn rejects_oversized_messages() {
        let mut buf = vec![KIND_TEXT];
        buf.extend_from_slice(&(MAX_MESSAGE as u32 + 1).to_be_bytes());
        let mut reader = buf.as_slice();
        assert!(read_message(&mut reader).await.is_err());
    }

    #[test]
    fn only_safe_relative_paths() {
        assert_eq!(safe_relative_path("a.txt"), Some(PathBuf::from("a.txt")));
        assert_eq!(
            safe_relative_path("Folder/sub/b.png"),
            Some(PathBuf::from("Folder").join("sub").join("b.png"))
        );
        for unsafe_path in [
            "",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:/x",
            "a\\..\\x",
            "a//b",
            "./a",
        ] {
            assert_eq!(safe_relative_path(unsafe_path), None, "{unsafe_path}");
        }
    }

    #[test]
    fn png_round_trip() {
        let image = arboard::ImageData {
            width: 2,
            height: 1,
            bytes: vec![255, 0, 0, 255, 0, 0, 255, 128].into(),
        };
        let decoded = decode_png(&encode_png(&image).unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.bytes, image.bytes);
    }

    /// files and folders arrive with their contents, in a new folder
    #[tokio::test]
    async fn file_transfer_round_trip() {
        let source = test_folder("source");
        std::fs::write(source.join("note.txt"), "hello").unwrap();
        std::fs::create_dir_all(source.join("photos").join("2026")).unwrap();
        let big = (0..3 * FILE_CHUNK + 7)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(source.join("photos").join("2026").join("big.bin"), &big).unwrap();

        let copied = vec![source.join("note.txt"), source.join("photos")];
        let transfer = FileTransfer::collect(&copied, u64::MAX).unwrap();
        let mut stream = Vec::new();
        write_outgoing(&mut stream, &Outgoing::Files(Rc::new(transfer)))
            .await
            .unwrap();

        let receive_root = test_folder("received");
        let mut reader = stream.as_slice();
        let (kind, manifest) = read_message(&mut reader).await.unwrap();
        assert_eq!(kind, KIND_FILES);
        let items = receive_files(&mut reader, &manifest, &receive_root, true, u64::MAX)
            .await
            .unwrap()
            .unwrap();
        assert!(reader.is_empty(), "the whole transfer was read");

        let names = items
            .iter()
            .map(|item| item.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["note.txt", "photos"]);
        assert_eq!(std::fs::read_to_string(&items[0]).unwrap(), "hello");
        assert_eq!(
            std::fs::read(items[1].join("2026").join("big.bin")).unwrap(),
            big
        );
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(receive_root);
    }

    /// files over the limit are not sent, and not stored when received
    #[tokio::test]
    async fn file_size_limit() {
        let source = test_folder("limit");
        std::fs::write(source.join("a.bin"), vec![0u8; 100]).unwrap();
        let copied = vec![source.join("a.bin")];
        assert!(FileTransfer::collect(&copied, 99).is_err());
        assert!(FileTransfer::collect(&copied, 0).is_err());

        let transfer = FileTransfer::collect(&copied, 100).unwrap();
        let mut stream = Vec::new();
        write_outgoing(&mut stream, &Outgoing::Files(Rc::new(transfer)))
            .await
            .unwrap();
        let receive_root = test_folder("limit-received");
        let mut reader = stream.as_slice();
        let (_, manifest) = read_message(&mut reader).await.unwrap();
        let received = receive_files(&mut reader, &manifest, &receive_root, true, 99)
            .await
            .unwrap();
        assert_eq!(received, None);
        assert!(reader.is_empty(), "the dropped transfer was still read");
        assert_eq!(std::fs::read_dir(&receive_root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(receive_root);
    }
}

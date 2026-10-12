//! Clipboard sharing: text copied on one device can be pasted on the others.
//!
//! A watcher thread notices when the local clipboard changes and the new text
//! is sent to every device this one is connected to. Text received from
//! another device is put on the local clipboard.
//!
//! Clipboards travel over TLS on the Lan Mouse port (TCP), separately from
//! the DTLS input events, which are limited to small datagrams. A device
//! connects to the clients it controls; both sides send over that connection.
//! Like input, only authorized devices may connect.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock, mpsc as std_mpsc},
    thread,
    time::Duration,
};

use local_channel::mpsc::{Sender, channel};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::WebPkiSupportedAlgorithms,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
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
/// clipboard text larger than this is not shared
const MAX_TEXT: usize = 16 * 1024 * 1024;
/// message kind of a text clipboard
const KIND_TEXT: u8 = 1;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("TLS setup failed: `{0}`")]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) struct ClipboardSync {
    enabled: Rc<Cell<bool>>,
    tasks: Vec<JoinHandle<()>>,
    /// stops the watcher thread when dropped
    _set_tx: std_mpsc::Sender<String>,
}

impl ClipboardSync {
    pub(crate) fn new(
        enabled: bool,
        port: u16,
        cert: &Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        client_manager: ClientManager,
    ) -> Result<Self, ClipboardError> {
        let (acceptor, connector) = tls_config(cert, authorized_keys)?;
        let enabled = Rc::new(Cell::new(enabled));

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
        });

        let listen_task = spawn_local(listen(port, acceptor, shared.clone()));
        let connect_task = spawn_local(connect_to_clients(
            connector,
            client_manager,
            shared.clone(),
        ));
        // send local changes to all connected devices
        let share_task = spawn_local(async move {
            while let Some(text) = changed_rx.recv().await {
                if !shared.enabled.get() {
                    continue;
                }
                let text: Rc<str> = text.into();
                for (addr, writer) in shared.writers.borrow().values() {
                    log::debug!("sending clipboard ({} bytes) to {addr}", text.len());
                    let _ = writer.send(text.clone());
                }
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

/// by connection id: peer address and a channel to send clipboard text over
type Writers = RefCell<HashMap<u64, (SocketAddr, Sender<Rc<str>>)>>;

/// state shared by the connection tasks
struct Shared {
    enabled: Rc<Cell<bool>>,
    /// puts text on the local clipboard
    set_tx: std_mpsc::Sender<String>,
    writers: Writers,
    next_id: Cell<u64>,
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
                Ok(stream) => serve(stream, addr, shared).await,
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
                    Ok(stream) => serve(stream, addr, shared).await,
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

/// exchange clipboards over an established connection until it closes
async fn serve<S>(stream: S, addr: SocketAddr, shared: Rc<Shared>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    log::info!("sharing the clipboard with {addr}");
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = channel::<Rc<str>>();
    let id = shared.next_id.get();
    shared.next_id.set(id + 1);
    shared.writers.borrow_mut().insert(id, (addr, tx));

    let receive = async {
        loop {
            match read_text(&mut reader).await {
                Ok(Some(text)) => {
                    if shared.enabled.get() {
                        log::debug!("received clipboard ({} bytes) from {addr}", text.len());
                        let _ = shared.set_tx.send(text);
                    }
                }
                Ok(None) => {}
                Err(e) => break e,
            }
        }
    };
    let send = async {
        while let Some(text) = rx.recv().await {
            if let Err(e) = write_text(&mut writer, &text).await {
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

async fn write_text<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> io::Result<()> {
    writer.write_u8(KIND_TEXT).await?;
    writer.write_u32(text.len() as u32).await?;
    writer.write_all(text.as_bytes()).await?;
    writer.flush().await
}

/// next clipboard text; `None` for message kinds this version doesn't know
async fn read_text<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let kind = reader.read_u8().await?;
    let len = reader.read_u32().await? as usize;
    if len > MAX_TEXT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard too large",
        ));
    }
    let mut data = vec![0; len];
    reader.read_exact(&mut data).await?;
    if kind != KIND_TEXT {
        return Ok(None);
    }
    String::from_utf8(data)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Watches the clipboard and reports new text; puts text received from
/// other devices on the clipboard. Runs until `set_rx` is disconnected.
fn watch_clipboard(
    changed_tx: tokio_mpsc::UnboundedSender<String>,
    set_rx: std_mpsc::Receiver<String>,
) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => return log::warn!("clipboard sharing unavailable: {e}"),
    };
    let mut change = ChangeDetector::default();
    // text that is on the clipboard because another device sent it
    let mut from_remote: Option<String> = None;
    // don't share whatever is on the clipboard at startup
    let mut last = clipboard.get_text().ok();
    loop {
        match set_rx.recv_timeout(CHECK_INTERVAL) {
            Ok(text) => {
                if last.as_deref() == Some(text.as_str()) {
                    continue;
                }
                match clipboard.set_text(text.clone()) {
                    Ok(()) => {
                        last = Some(text.clone());
                        from_remote = Some(text);
                        change.reset();
                    }
                    Err(e) => log::warn!("could not set clipboard: {e}"),
                }
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if !change.changed() {
            continue;
        }
        let Ok(text) = clipboard.get_text() else {
            continue;
        };
        if last.as_deref() == Some(text.as_str()) {
            continue;
        }
        last = Some(text.clone());
        // a remote clipboard showing up locally is not a new copy
        if from_remote.as_deref() == Some(text.as_str()) || text.len() > MAX_TEXT {
            continue;
        }
        if changed_tx.send(text).is_err() {
            return;
        }
    }
}

/// Tells whether the clipboard may have changed since the last check.
/// On Windows this avoids opening the clipboard (and blocking other programs
/// from it) twice a second; elsewhere the text is simply compared.
#[derive(Default)]
struct ChangeDetector {
    #[cfg(windows)]
    sequence: u32,
}

impl ChangeDetector {
    #[cfg(windows)]
    fn changed(&mut self) -> bool {
        let sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        let changed = sequence != self.sequence;
        self.sequence = sequence;
        changed
    }

    #[cfg(not(windows))]
    fn changed(&mut self) -> bool {
        true
    }

    /// forget changes made by ourselves
    fn reset(&mut self) {
        let _ = self.changed();
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

    #[tokio::test]
    async fn text_round_trip() {
        let mut buf = Vec::new();
        write_text(&mut buf, "héllo 🌍").await.unwrap();
        let mut reader = buf.as_slice();
        assert_eq!(
            read_text(&mut reader).await.unwrap().as_deref(),
            Some("héllo 🌍")
        );
    }

    #[tokio::test]
    async fn skips_unknown_kinds() {
        let mut buf = vec![7u8];
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(b"png");
        let mut reader = buf.as_slice();
        assert_eq!(read_text(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized() {
        let mut buf = vec![KIND_TEXT];
        buf.extend_from_slice(&(MAX_TEXT as u32 + 1).to_be_bytes());
        let mut reader = buf.as_slice();
        assert!(read_text(&mut reader).await.is_err());
    }
}

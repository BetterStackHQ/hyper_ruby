// Syslog transport listeners: a stream listener (Unix socket or TCP) that frames
// messages with RFC 6587 octet counting and a newline fallback, and a datagram
// listener where one datagram is one message. Complete messages are handed to
// the same Ruby worker threads the HTTP path uses.

mod framing;
mod proxy;

use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use crossbeam_channel::TrySendError;
use log::{debug, info, warn};
use magnus::{Error as MagnusError, RHash, Symbol, TryConvert};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::{TcpListener, UdpSocket, UnixListener};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use framing::{Framer, RejectReason};
use proxy::{ProxyError, ProxyHeader};

/// Largest PROXY v2 header (including TLVs) we will read.
const MAX_PROXY_HEADER_BYTES: usize = 1024;
/// How long to wait for space on the Ruby worker queue before trying again.
const WORKER_QUEUE_RETRY: Duration = Duration::from_millis(10);
/// Backoff bounds used when a handler refuses a message.
const MIN_REFUSAL_DELAY: Duration = Duration::from_millis(5);
const MAX_REFUSAL_DELAY: Duration = Duration::from_millis(500);
/// How long to pause after a failed accept or receive.
const SOCKET_ERROR_DELAY: Duration = Duration::from_millis(10);
/// Read buffer growth increment for stream connections.
const STREAM_READ_CHUNK: usize = 8 * 1024;

/// Message identities are unique for the life of the process, so a handler can
/// recognise the retries of a message it has already seen.
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Stream,
    Datagram,
}

impl Transport {
    pub(crate) fn symbol(self) -> Symbol {
        match self {
            Transport::Stream => Symbol::new("stream"),
            Transport::Datagram => Symbol::new("datagram"),
        }
    }
}

/// What a Ruby handler made of one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandlerResult {
    Accepted,
    Refused,
    Failed,
}

#[derive(Clone)]
pub(crate) struct SyslogConfig {
    pub stream_path: Option<String>,
    pub stream_bind: Option<String>,
    pub udp_bind: Option<String>,
    pub proxy_protocol: bool,
    pub proxy_header_timeout: u64,
    pub max_frame_bytes: usize,
    pub max_pending: usize,
    pub max_pending_per_connection: usize,
    pub max_connections: u64,
    pub idle_timeout: u64,
    pub udp_max_datagram_bytes: usize,
    pub udp_so_rcvbuf: Option<usize>,
    pub work_ratio: u32,
}

impl SyslogConfig {
    pub(crate) fn new() -> Self {
        Self {
            stream_path: None,
            stream_bind: None,
            udp_bind: None,
            proxy_protocol: false,
            proxy_header_timeout: 5000,
            max_frame_bytes: 102400,
            max_pending: 1000,
            max_pending_per_connection: 64,
            max_connections: 10000,
            idle_timeout: 0,
            udp_max_datagram_bytes: 65536,
            udp_so_rcvbuf: None,
            work_ratio: 4,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.stream_path.is_some() || self.stream_bind.is_some() || self.udp_bind.is_some()
    }

    /// Read the syslog keys out of the server configuration hash.
    pub(crate) fn apply(&mut self, config: &RHash) -> Result<(), MagnusError> {
        if let Some(value) = config.get(Symbol::new("syslog_stream_path")) {
            self.stream_path = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_stream_bind")) {
            self.stream_bind = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_bind")) {
            self.udp_bind = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_proxy_protocol")) {
            self.proxy_protocol = bool::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_proxy_header_timeout")) {
            self.proxy_header_timeout = u64::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_frame_bytes")) {
            self.max_frame_bytes = usize::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_pending")) {
            self.max_pending = usize::try_convert(value)?.clamp(1, Semaphore::MAX_PERMITS);
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_pending_per_connection")) {
            self.max_pending_per_connection = usize::try_convert(value)?.max(1);
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_connections")) {
            self.max_connections = u64::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_idle_timeout_ms")) {
            self.idle_timeout = u64::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_max_datagram_bytes")) {
            self.udp_max_datagram_bytes = usize::try_convert(value)?.max(1);
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_so_rcvbuf")) {
            self.udp_so_rcvbuf = Some(usize::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_work_ratio")) {
            self.work_ratio = u32::try_convert(value)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct SyslogCounters {
    messages_delivered: AtomicU64,
    deliveries_refused: AtomicU64,
    handler_errors: AtomicU64,
    rejected_oversize: AtomicU64,
    rejected_invalid_utf8: AtomicU64,
    rejected_invalid_length: AtomicU64,
    udp_truncated: AtomicU64,
    udp_dropped: AtomicU64,
    connections_opened: AtomicU64,
    connections_closed: AtomicU64,
    connections_refused: AtomicU64,
    proxy_header_errors: AtomicU64,
    proxy_read_errors: AtomicU64,
    abandoned_at_shutdown: AtomicU64,
    /// Messages framed but not yet through a handler.
    pending: AtomicU64,
}

impl SyslogCounters {
    fn record_rejection(&self, reason: RejectReason) {
        let counter = match reason {
            RejectReason::Oversize => &self.rejected_oversize,
            RejectReason::InvalidUtf8 => &self.rejected_invalid_utf8,
            RejectReason::InvalidLength => &self.rejected_invalid_length,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn abandon_pending(&self) {
        self.abandoned_at_shutdown.fetch_add(1, Ordering::Relaxed);
        self.release_pending();
    }

    // Saturating, because a drain timeout clears the gauge while its messages
    // may still be finishing.
    fn release_pending(&self) {
        let _ = self.pending.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
            Some(pending.saturating_sub(1))
        });
    }

    pub(crate) fn to_hash(&self) -> Result<RHash, MagnusError> {
        let rejected = RHash::new();
        rejected.aset(
            Symbol::new(RejectReason::Oversize.as_str()),
            self.rejected_oversize.load(Ordering::Relaxed),
        )?;
        rejected.aset(
            Symbol::new(RejectReason::InvalidUtf8.as_str()),
            self.rejected_invalid_utf8.load(Ordering::Relaxed),
        )?;
        rejected.aset(
            Symbol::new(RejectReason::InvalidLength.as_str()),
            self.rejected_invalid_length.load(Ordering::Relaxed),
        )?;

        let stats = RHash::new();
        let counters: [(&str, &AtomicU64); 12] = [
            ("messages_delivered", &self.messages_delivered),
            ("deliveries_refused", &self.deliveries_refused),
            ("handler_errors", &self.handler_errors),
            ("udp_truncated", &self.udp_truncated),
            ("udp_dropped", &self.udp_dropped),
            ("connections_opened", &self.connections_opened),
            ("connections_closed", &self.connections_closed),
            ("connections_refused", &self.connections_refused),
            ("proxy_header_errors", &self.proxy_header_errors),
            ("proxy_read_errors", &self.proxy_read_errors),
            ("abandoned_at_shutdown", &self.abandoned_at_shutdown),
            ("pending", &self.pending),
        ];
        for (name, counter) in counters {
            stats.aset(Symbol::new(name), counter.load(Ordering::Relaxed))?;
        }
        stats.aset(Symbol::new("frames_rejected"), rejected)?;
        Ok(stats)
    }
}

/// A complete message on its way to a Ruby worker thread.
pub(crate) struct SyslogDelivery {
    pub message: Bytes,
    /// Stream frames are validated UTF-8; datagrams are handed over as binary.
    pub utf8: bool,
    /// Absent when the transport names no peer, such as a Unix socket
    /// connection without a PROXY header.
    pub peer: Option<Arc<str>>,
    pub transport: Transport,
    pub received_at_ns: u64,
    pub message_id: u64,
    pub attempt: u32,
    pub result_tx: oneshot::Sender<HandlerResult>,
}

enum DeliveryOutcome {
    Accepted,
    Refused,
    /// No worker can answer any more; the listener should give up.
    Unavailable,
}

/// Everything a listener needs to hand a message to the Ruby workers.
#[derive(Clone)]
struct Dispatcher {
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
    counters: Arc<SyslogCounters>,
    credits: Arc<Semaphore>,
    connections: Arc<AtomicU64>,
    tracker: TaskTracker,
}

impl Dispatcher {
    async fn deliver(&self, pending: &PendingMessage, attempt: u32) -> DeliveryOutcome {
        let (result_tx, result_rx) = oneshot::channel();
        let mut delivery = SyslogDelivery {
            message: pending.message.clone(),
            utf8: pending.utf8,
            peer: pending.peer.clone(),
            transport: pending.transport,
            received_at_ns: pending.received_at_ns,
            message_id: pending.message_id,
            attempt,
            result_tx,
        };

        loop {
            match self.work_tx.try_send(delivery) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    delivery = returned;
                    sleep(WORKER_QUEUE_RETRY).await;
                }
                Err(TrySendError::Disconnected(_)) => return DeliveryOutcome::Unavailable,
            }
        }

        match result_rx.await {
            Ok(HandlerResult::Accepted) => {
                self.counters
                    .messages_delivered
                    .fetch_add(1, Ordering::Relaxed);
                DeliveryOutcome::Accepted
            }
            Ok(HandlerResult::Refused) => {
                self.counters
                    .deliveries_refused
                    .fetch_add(1, Ordering::Relaxed);
                DeliveryOutcome::Refused
            }
            Ok(HandlerResult::Failed) => {
                self.counters.handler_errors.fetch_add(1, Ordering::Relaxed);
                DeliveryOutcome::Refused
            }
            Err(_) => DeliveryOutcome::Unavailable,
        }
    }
}

/// Handle on the running listeners, used to drain them at shutdown.
pub(crate) struct SyslogRuntime {
    tracker: TaskTracker,
    token: CancellationToken,
    counters: Arc<SyslogCounters>,
    listening: Arc<AtomicBool>,
    socket_path: Option<String>,
    // (dev, ino) of the Unix socket file we bound, so we only unlink our own.
    socket_ident: Option<(u64, u64)>,
}

impl SyslogRuntime {
    pub(crate) fn listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }

    /// Stop accepting connections and reading from the open ones.
    pub(crate) fn stop_accepting(&self) {
        self.listening.store(false, Ordering::Relaxed);
        self.token.cancel();
    }

    /// Wait for the listeners to finish delivering messages they have framed.
    pub(crate) async fn drain(&self, limit: Duration) {
        self.stop_accepting();
        if timeout(limit, self.tracker.wait()).await.is_err() {
            let abandoned = self.counters.pending.swap(0, Ordering::Relaxed);
            self.counters
                .abandoned_at_shutdown
                .fetch_add(abandoned, Ordering::Relaxed);
            warn!(
                "Timed out draining syslog listeners, abandoning {} undelivered messages",
                abandoned
            );
        }
    }

    pub(crate) fn remove_socket_file(&self) {
        remove_socket_file(self.socket_path.as_deref(), self.socket_ident);
    }
}

/// Unlink a bound Unix socket file, unless a replacement server has taken the
/// path over in the meantime.
fn remove_socket_file(path: Option<&str>, ident: Option<(u64, u64)>) {
    let (Some(path), Some((dev, ino))) = (path, ident) else {
        return;
    };
    match std::fs::symlink_metadata(path) {
        Ok(meta) if (meta.dev(), meta.ino()) == (dev, ino) => {
            std::fs::remove_file(path)
                .unwrap_or_else(|e| warn!("Failed to remove syslog socket file: {:?}", e));
        }
        Ok(_) => info!(
            "Syslog socket file {} was replaced by another server; leaving it in place",
            path
        ),
        Err(_) => debug!("Syslog socket file {} already removed", path),
    }
}

/// Bind the configured listeners and start serving. Must be called from within
/// the Tokio runtime.
pub(crate) fn start(
    config: &SyslogConfig,
    counters: Arc<SyslogCounters>,
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
) -> io::Result<SyslogRuntime> {
    let mut socket = (None, None);
    let started = start_listeners(config, counters, work_tx, &mut socket);
    if started.is_err() {
        // Leave no socket file behind for listeners that never came up.
        remove_socket_file(socket.0.as_deref(), socket.1);
    }
    started
}

fn start_listeners(
    config: &SyslogConfig,
    counters: Arc<SyslogCounters>,
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
    socket: &mut (Option<String>, Option<(u64, u64)>),
) -> io::Result<SyslogRuntime> {
    let tracker = TaskTracker::new();
    let token = CancellationToken::new();
    let dispatcher = Dispatcher {
        work_tx,
        counters: counters.clone(),
        credits: Arc::new(Semaphore::new(config.max_pending)),
        connections: Arc::new(AtomicU64::new(0)),
        tracker: tracker.clone(),
    };

    if let Some(path) = &config.stream_path {
        let (listener, ident) = bind_unix_listener(path)?;
        *socket = (Some(path.clone()), ident);
        spawn_stream_listener(
            StreamListener::Unix(listener),
            config.clone(),
            dispatcher.clone(),
            &token,
            &tracker,
        );
    }

    if let Some(address) = &config.stream_bind {
        let address: SocketAddr = address
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        let listener = TcpListener::from_std(bind_std_tcp(address)?)?;
        spawn_stream_listener(
            StreamListener::Tcp(listener),
            config.clone(),
            dispatcher.clone(),
            &token,
            &tracker,
        );
    }

    if let Some(address) = &config.udp_bind {
        let address: SocketAddr = address
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        let datagrams = UdpSocket::from_std(bind_std_udp(address, config)?)?;
        tracker.spawn(serve_datagrams(
            datagrams,
            config.clone(),
            dispatcher.clone(),
            token.child_token(),
        ));
    }

    tracker.close();

    Ok(SyslogRuntime {
        tracker,
        token,
        counters,
        listening: Arc::new(AtomicBool::new(true)),
        socket_path: socket.0.clone(),
        socket_ident: socket.1,
    })
}

fn bind_unix_listener(path: &str) -> io::Result<(UnixListener, Option<(u64, u64)>)> {
    // Bind a unique temporary path and rename over the target, so the path
    // always points at a live socket even when a replacement server takes over
    // a path an older, still draining server bound.
    static SOCKET_TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp_path = format!(
        "{}.{}.{}.tmp",
        path,
        std::process::id(),
        SOCKET_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );

    let listener = UnixListener::bind(&tmp_path)?;
    let ident = std::fs::symlink_metadata(&tmp_path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()));

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok((listener, ident))
}

fn bind_std_tcp(address: SocketAddr) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

fn bind_std_udp(address: SocketAddr, config: &SyslogConfig) -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(address), Type::DGRAM, Some(Protocol::UDP))?;
    // Several processes can bind the same port; the kernel hands each datagram
    // to one of them.
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    if let Some(size) = config.udp_so_rcvbuf {
        socket.set_recv_buffer_size(size)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    Ok(socket.into())
}

enum StreamListener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl StreamListener {
    async fn accept(&self) -> io::Result<(Box<dyn AsyncReadStream>, Option<Arc<str>>)> {
        match self {
            StreamListener::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok((Box::new(stream), None))
            }
            StreamListener::Tcp(listener) => {
                let (stream, address) = listener.accept().await?;
                Ok((Box::new(stream), Some(Arc::from(address.ip().to_string()))))
            }
        }
    }
}

trait AsyncReadStream: AsyncRead + Unpin + Send {}
impl<T: AsyncRead + Unpin + Send> AsyncReadStream for T {}

fn spawn_stream_listener(
    listener: StreamListener,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    token: &CancellationToken,
    tracker: &TaskTracker,
) {
    let token = token.clone();
    let connections = tracker.clone();

    tracker.spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            // Transient accept failures (descriptor limits) must
                            // not turn into a hot loop.
                            warn!("Failed to accept syslog connection: {:?}", e);
                            sleep(SOCKET_ERROR_DELAY).await;
                            continue;
                        }
                    };

                    if dispatcher.connections.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                        dispatcher.connections.fetch_sub(1, Ordering::AcqRel);
                        dispatcher.counters.connections_refused.fetch_add(1, Ordering::Relaxed);
                        debug!("Refusing syslog connection: connection limit reached");
                        continue;
                    }

                    dispatcher.counters.connections_opened.fetch_add(1, Ordering::Relaxed);
                    // A child token, so a connection accepted in the same round
                    // as the shutdown still sees it.
                    connections.spawn(serve_stream(
                        stream,
                        peer,
                        config.clone(),
                        dispatcher.clone(),
                        token.child_token(),
                    ));
                },
                _ = token.cancelled() => {
                    debug!("Syslog stream listener shutting down");
                    break;
                }
            }
        }
    });
}

/// A framed message waiting for a Ruby worker. The permit is held until
/// delivery finishes, which is what bounds undelivered messages overall.
struct PendingMessage {
    message: Bytes,
    utf8: bool,
    peer: Option<Arc<str>>,
    transport: Transport,
    received_at_ns: u64,
    message_id: u64,
    _permit: OwnedSemaphorePermit,
}

async fn serve_stream(
    mut stream: Box<dyn AsyncReadStream>,
    peer: Option<Arc<str>>,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    token: CancellationToken,
) {
    let mut buffer = BytesMut::with_capacity(STREAM_READ_CHUNK);

    let peer = if config.proxy_protocol {
        match read_proxy_header(&mut stream, &mut buffer, &config).await {
            Ok(Some(source)) => Some(Arc::from(source.to_string())),
            Ok(None) => peer,
            Err(failure) => {
                failure.record(&dispatcher.counters);
                debug!(
                    "Rejecting syslog connection: PROXY header {}",
                    failure.as_str()
                );
                close_stream(&dispatcher);
                return;
            }
        }
    } else {
        peer
    };

    // Reading and delivery are separate so that a slow handler stops the reads
    // through the bounded channel rather than blocking the reactor.
    let (pending_tx, pending_rx) =
        mpsc::channel::<PendingMessage>(config.max_pending_per_connection);
    let delivery = tokio::spawn(deliver_stream_messages(
        pending_rx,
        dispatcher.clone(),
        token.clone(),
    ));

    let mut framer = Framer::new(config.max_frame_bytes);
    let idle_timeout = Duration::from_millis(config.idle_timeout);
    let mut eof = false;

    'read: loop {
        // Emit everything the buffer already holds before asking for more.
        loop {
            let decoded = if eof {
                framer.decode_eof(&mut buffer)
            } else {
                framer.decode(&mut buffer)
            };

            match decoded {
                Ok(Some(message)) => {
                    let received_at_ns = now_nanos();
                    let Ok(permit) = dispatcher.credits.clone().acquire_owned().await else {
                        break 'read;
                    };
                    let pending = PendingMessage {
                        message,
                        utf8: true,
                        peer: peer.clone(),
                        transport: Transport::Stream,
                        received_at_ns,
                        message_id: NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed),
                        _permit: permit,
                    };
                    dispatcher.counters.pending.fetch_add(1, Ordering::Relaxed);
                    if pending_tx.send(pending).await.is_err() {
                        dispatcher.counters.abandon_pending();
                        break 'read;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    dispatcher.counters.record_rejection(error.reason);
                    if error.fatal {
                        debug!("Closing syslog connection: {} frame", error.reason.as_str());
                        break 'read;
                    }
                }
            }
        }

        if eof {
            break;
        }

        tokio::select! {
            read = stream.read_buf(&mut buffer) => {
                match read {
                    Ok(0) => eof = true,
                    Ok(_) => (),
                    Err(e) => {
                        debug!("Syslog connection read failed: {:?}", e);
                        break;
                    }
                }
            },
            _ = sleep(idle_timeout), if config.idle_timeout > 0 => {
                debug!("Closing idle syslog connection");
                break;
            },
            _ = token.cancelled() => {
                debug!("Syslog connection stopping reads for shutdown");
                break;
            }
        }
    }

    // Dropping the sender lets the delivery task finish what it already has.
    drop(pending_tx);
    let _ = delivery.await;
    close_stream(&dispatcher);
}

fn close_stream(dispatcher: &Dispatcher) {
    dispatcher.connections.fetch_sub(1, Ordering::AcqRel);
    dispatcher
        .counters
        .connections_closed
        .fetch_add(1, Ordering::Relaxed);
}

async fn deliver_stream_messages(
    mut pending_rx: mpsc::Receiver<PendingMessage>,
    dispatcher: Dispatcher,
    token: CancellationToken,
) {
    let mut workers_gone = false;

    while let Some(pending) = pending_rx.recv().await {
        if workers_gone {
            dispatcher.counters.abandon_pending();
            continue;
        }

        let mut attempt = 1;
        let mut delay = MIN_REFUSAL_DELAY;

        loop {
            match dispatcher.deliver(&pending, attempt).await {
                DeliveryOutcome::Accepted => {
                    dispatcher.counters.release_pending();
                    break;
                }
                DeliveryOutcome::Refused => {
                    // Hold the message and try again; reads stall behind the
                    // bounded pending channel while we wait. Shutdown ends the
                    // retries rather than holding the drain open.
                    if token.is_cancelled() {
                        dispatcher.counters.abandon_pending();
                        break;
                    }
                    sleep(delay).await;
                    delay = (delay * 2).min(MAX_REFUSAL_DELAY);
                    attempt += 1;
                }
                DeliveryOutcome::Unavailable => {
                    dispatcher.counters.abandon_pending();
                    workers_gone = true;
                    break;
                }
            }
        }
    }
}

/// Read and consume a PROXY v2 header, returning the source address it carries.
async fn read_proxy_header(
    stream: &mut Box<dyn AsyncReadStream>,
    buffer: &mut BytesMut,
    config: &SyslogConfig,
) -> Result<Option<std::net::IpAddr>, ProxyFailure> {
    let deadline = Duration::from_millis(config.proxy_header_timeout);
    timeout(deadline, async {
        loop {
            match proxy::parse(buffer, MAX_PROXY_HEADER_BYTES) {
                Ok(ProxyHeader::Complete { source, length }) => {
                    let _ = buffer.split_to(length);
                    return Ok(source);
                }
                Ok(ProxyHeader::Incomplete) => match stream.read_buf(buffer).await {
                    Ok(0) => return Err(ProxyFailure::Read("eof")),
                    Ok(_) => continue,
                    Err(_) => return Err(ProxyFailure::Read("io")),
                },
                Err(error) => return Err(ProxyFailure::Header(error)),
            }
        }
    })
    .await
    .unwrap_or(Err(ProxyFailure::Read("timeout")))
}

/// Why a connection gave up before its PROXY header was complete.
enum ProxyFailure {
    Header(ProxyError),
    Read(&'static str),
}

impl ProxyFailure {
    fn as_str(&self) -> &'static str {
        match self {
            ProxyFailure::Header(error) => error.as_str(),
            ProxyFailure::Read(reason) => reason,
        }
    }

    fn record(&self, counters: &SyslogCounters) {
        match self {
            ProxyFailure::Header(_) => counters.proxy_header_errors.fetch_add(1, Ordering::Relaxed),
            ProxyFailure::Read(_) => counters.proxy_read_errors.fetch_add(1, Ordering::Relaxed),
        };
    }
}

async fn serve_datagrams(
    socket: UdpSocket,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    token: CancellationToken,
) {
    let mut buffer = vec![0u8; config.udp_max_datagram_bytes];

    loop {
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let (length, peer) = match received {
                    Ok(received) => received,
                    Err(e) => {
                        warn!("Syslog datagram receive failed: {:?}", e);
                        sleep(SOCKET_ERROR_DELAY).await;
                        continue;
                    }
                };
                let received_at_ns = now_nanos();

                // A datagram that fills the buffer was almost certainly cut
                // short by it, and a truncated prefix must not pass as a message.
                if length == buffer.len() {
                    dispatcher.counters.udp_truncated.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let Ok(permit) = dispatcher.credits.clone().try_acquire_owned() else {
                    dispatcher.counters.udp_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                };

                let pending = PendingMessage {
                    message: Bytes::copy_from_slice(&buffer[..length]),
                    utf8: false,
                    peer: Some(Arc::from(peer.ip().to_string())),
                    transport: Transport::Datagram,
                    received_at_ns,
                    message_id: NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed),
                    _permit: permit,
                };
                dispatcher.counters.pending.fetch_add(1, Ordering::Relaxed);

                let dispatcher = dispatcher.clone();
                dispatcher.tracker.clone().spawn(async move {
                    // A datagram sender cannot be asked to slow down, so a
                    // refusal drops the message.
                    match dispatcher.deliver(&pending, 1).await {
                        DeliveryOutcome::Accepted => (),
                        DeliveryOutcome::Refused | DeliveryOutcome::Unavailable => {
                            dispatcher.counters.udp_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    dispatcher.counters.release_pending();
                });
            },
            _ = token.cancelled() => {
                debug!("Syslog datagram listener shutting down");
                break;
            }
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

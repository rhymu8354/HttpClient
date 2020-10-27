mod error;

use async_std::net::TcpStream;
use async_tls::TlsConnector;
pub use error::Error;
use futures::{
    AsyncRead,
    AsyncReadExt,
    AsyncWrite,
    AsyncWriteExt,
    FutureExt,
    select,
    StreamExt
};
use futures::channel::{
    mpsc,
    oneshot
};
use futures::executor::block_on;
use futures::future::ready;
use rhymuweb::{
    Response,
    ResponseParseStatus,
    Request,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map;
use std::pin::Pin;
use std::rc::Rc;
use std::thread;

pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

struct ParkedConnection {
    // stream: Box<dyn Stream>,
    request_in: oneshot::Sender<oneshot::Sender<Option<Box<dyn Stream>>>>,

    monitor: Option<Pin<Box<dyn futures::Future<Output=()>>>>,
}

async fn monitor_connection(
    mut connection: Box<dyn Stream>,
    receiver: oneshot::Receiver<oneshot::Sender<Option<Box<dyn Stream>>>>,
) {
    let mut buffer = [0; 1];
    select!(
        _ = connection.read(&mut buffer).fuse() => {
        },
        sender = receiver.fuse() => {
            if let Ok(sender) = sender {
                sender.send(Some(connection)).unwrap_or(());
            }
        },
    )
}

struct ParkedConnectionPool {
    connections: HashMap<usize, ParkedConnection>,
    next: usize,
}

impl ParkedConnectionPool {
    fn add(
        &mut self,
        stream: Box<dyn Stream>,
    ) {
        let key = self.next;
        self.next += 1;
        let (sender, receiver) = oneshot::channel();
        let parked_connection = ParkedConnection{
            request_in: sender,
            monitor: Some(monitor_connection(stream, receiver).boxed()),
        };
        self.connections.insert(key, parked_connection);
    }

    fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    fn new() -> Self {
        Self {
            connections: HashMap::new(),
            next: 0,
        }
    }

    async fn remove(&mut self) -> Option<Box<dyn Stream>> {
        let key = match self.connections.keys().next() {
            Some(key) => *key,
            None => return None,
        };
        if let Some(parked_connection) = self.connections.remove(&key) {
            // NOTE: At this point we probably want to return
            // `parked_connection` so that we can stop borrowing the connection
            // pool, before sending our sender and receiving back the stream.
            // Otherwise, `watch_pool` will attempt to borrow the connection
            // pool mutably while we're still borrowing it here, causing a
            // panic.
            let (sender, receiver) = oneshot::channel();
            if parked_connection.request_in.send(sender).is_err() {
                None
            } else {
                receiver
                    .await
                    .unwrap_or(None)
            }
        } else {
            None
        }
    }
}

struct ParkedConnectionPools {
    pools: HashMap<ConnectionKey, ParkedConnectionPool>,
    kick: Option<oneshot::Sender<()>>,
}

async fn take_parked_connection(
    connection_pools: &Rc<RefCell<HashMap<ConnectionKey, ParkedConnectionPool>>>,
    connection_key: ConnectionKey,
) -> Option<Box<dyn Stream>> {
    match connection_pools.borrow_mut().entry(connection_key) {
        hash_map::Entry::Occupied(mut entry) => {
            // TODO: The await here might cause a panic, because
            // `connection_pools` is still being borrowed mutably while
            // `remove` waits for the monitor to send back the stream, and in
            // the mean time `watch_pool` will attempt to borrow
            // `connection_pools` mutably in order to gather more monitors.
            let stream = entry.get_mut().remove().await;
            if entry.get().is_empty() {
                entry.remove();
            }
            stream
        },
        hash_map::Entry::Vacant(_) => None,
    }
}

async fn process_work_queue(
    connection_pools: Rc<RefCell<HashMap<ConnectionKey, ParkedConnectionPool>>>,
    receiver: mpsc::UnboundedReceiver<WorkerMessage>
) {
    receiver
        .take_while(|message| ready(
            !matches!(message, WorkerMessage::Stop)
        ))
        .for_each(|message| async {
            match message {
                WorkerMessage::GiveConnection{
                    connection_key,
                    connection,
                } => {
                    println!("Placing connection to {:?} in pool", connection_key);
                    connection_pools.borrow_mut().entry(connection_key)
                        .or_insert_with(ParkedConnectionPool::new)
                        .add(connection)
                },
                WorkerMessage::TakeConnection{
                    connection_key,
                    return_channel,
                } => {
                    println!("Asked to recycle connection to {:?}", connection_key);
                    return_channel.send(
                        take_parked_connection(&connection_pools, connection_key).await
                    ).unwrap_or(());
                },
                WorkerMessage::Stop => unreachable!()
            }
        }).await
}

async fn watch_pool(
    connection_pools: Rc<RefCell<HashMap<ConnectionKey, ParkedConnectionPool>>>,
) {
    let mut monitors = Vec::new();
    loop {
        monitors.extend(
            connection_pools.borrow_mut().iter_mut()
                .map(|(_, connection_pool)| {
                    connection_pool.connections.iter_mut()
                        .filter_map(|(_, connection)| {
                            connection.monitor.take()
                        })
                })
                .flatten()
        );
        // NOTE: We need to add another future which completes when
        // any monitor is added to `connection_pools`.  This serves
        // two purposes: (1) to prevent `select_all` from being called
        // with no futures, and (2) to wake up this task to collect
        // more monitors when they are added.
        let (_, _, new_monitors) = futures::future::select_all(
            monitors.into_iter()
        ).await;
        monitors = new_monitors;
    }
}

fn worker(
    receiver: mpsc::UnboundedReceiver<WorkerMessage>
) {
    block_on(async {
        println!("Worker started");
        let connection_pools = Rc::new(RefCell::new(HashMap::new()));
        select!(
            () = process_work_queue(connection_pools.clone(), receiver).fuse() => (),
            () = watch_pool(connection_pools.clone()).fuse() => (),
        );
        println!("Worker stopping");
    });
}

async fn transact(
    raw_request: Vec<u8>,
    mut stream: Box<dyn Stream>
) -> Result<(Response, Box<dyn Stream>), Error> {
    // Send the request to the server.
    stream.write_all(&raw_request).await
        .map_err(Error::UnableToSend)?;

    // Receive the response from the server.
    let mut response = Response::new();
    let mut receive_buffer = Vec::new();
    loop {
        let left_over = receive_buffer.len();
        receive_buffer.resize(
            left_over + 65536,
            0
        );
        let received = stream.read(&mut receive_buffer[left_over..]).await
            .map_err(Error::UnableToReceive)
            .and_then(|received| match received {
                0 => Err(Error::Disconnected),
                received => Ok(received),
            })?;
        receive_buffer.truncate(left_over + received);
        let response_status = response.parse(&mut receive_buffer)
            .map_err(Error::BadResponse)?;
        receive_buffer.drain(0..response_status.consumed);
        if response_status.status == ResponseParseStatus::Complete {
            return Ok((response, stream));
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionKey {
    host: String,
    port: u16,
    use_tls: bool,
}

enum WorkerMessage {
    Stop,
    GiveConnection{
        connection_key: ConnectionKey,
        connection: Box<dyn Stream>,
    },
    TakeConnection{
        connection_key: ConnectionKey,
        return_channel: oneshot::Sender<Option<Box<dyn Stream>>>,
    },
}

pub struct HttpClient {
    work_in: mpsc::UnboundedSender<WorkerMessage>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HttpClient {

    fn recycle_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
        stream: Box<dyn Stream>
    ) where Host: AsRef<str> {
        let host = host.as_ref();
        let connection_key = ConnectionKey{
            host: String::from(host),
            port,
            use_tls
        };
        self.work_in.unbounded_send(WorkerMessage::GiveConnection{
            connection_key,
            connection: stream
        }).unwrap_or(());
    }

    async fn request_recycled_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool
    ) -> Option<Box<dyn Stream>>
        where Host: AsRef<str>
    {
        let host = host.as_ref();
        let connection_key = ConnectionKey{
            host: String::from(host),
            port,
            use_tls
        };
        let (sender, receiver) = oneshot::channel();
        println!("Trying to recycle connection to {:?}", connection_key);
        self.work_in.unbounded_send(WorkerMessage::TakeConnection{
            connection_key,
            return_channel: sender
        }).unwrap();
        receiver.await.unwrap_or(None)
    }

    pub async fn connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
    ) -> Result<Box<dyn Stream>, Error>
        where Host: AsRef<str>
    {
        // Check if a connection is already made, and if so, reuse it.
        let host = host.as_ref();
        if let Some(stream) = self.request_recycled_connection(host, port, use_tls).await {
            return Ok(stream);
        }

        // Connect to the server.
        let address = &format!("{}:{}", host, port);
        println!("Connecting to '{}'...", address);
        let stream = TcpStream::connect(address).await
            .map_err(Error::UnableToConnect)?;
        println!(
            "Connected (address: {}).",
            stream.peer_addr()
                .map_err(Error::UnableToGetPeerAddress)?
        );

        // Wrap with TLS connector if necessary.
        if use_tls {
            println!("Using TLS.");
            let tls_connector = TlsConnector::default();
            let tls_stream = tls_connector.connect(host, stream).await
                .map_err(Error::TlsHandshake)?;
            Ok(Box::new(tls_stream))
        } else {
            println!("Not using TLS.");
            Ok(Box::new(stream))
        }
    }

    pub async fn fetch<Req>(
        &self,
        request: Req,
        reuse: bool,
    ) -> Result<Response, Error>
        where Req: Into<Request>
    {
        let mut request: Request = request.into();
        if let Some(authority) = request.target.take_authority() {
            // Remove scheme from the target.
            let scheme = request.target.take_scheme();

            // Determine the server hostname and include it in the request
            // headers.
            let host = std::str::from_utf8(authority.host())
                .map_err(|_| Error::HostNotValidText(authority.host().to_vec()))?;
            request.headers.set_header("Host", host);

            // Store the body size in the request headers.
            if !request.body.is_empty() {
                request.headers.set_header(
                    "Content-Length",
                    request.body.len().to_string()
                );
            }

            // Set other headers specific to the user agent.
            // request.headers.set_header("Accept-Encoding", "gzip, deflate");
            if !reuse {
                request.headers.set_header("Connection", "Close");
            }

            // Determine the socket address of the server given
            // the hostname and port number.
            let port = authority.port()
                .or_else(
                    || match scheme.as_deref() {
                        Some("http") | Some("ws") => Some(80),
                        Some("https") | Some("wss") => Some(443),
                        _ => None,
                    }
                )
                .ok_or_else(
                    || Error::UnableToDetermineServerPort(request.target.clone())
                )?;

            // Generate the raw request byte stream.
            println!("Request:");
            println!("{}", "=".repeat(78));
            println!("{} {}", request.method, request.target);
            for header in &request.headers {
                println!("{}: {}", header.name, header.value);
            }
            println!();
            match String::from_utf8(request.body.clone()) {
                Err(_) => println!("(Body cannot be decoded as UTF-8)"),
                Ok(body) => println!("{}", body),
            };
            println!("{}", "=".repeat(78));
            let raw_request = request.generate()
                .map_err(Error::BadRequest)?;

            // Connect to the server.
            let use_tls = matches!(
                scheme.as_deref(),
                Some("https") | Some("wss")
            );
            let stream = self.connection(
                host,
                port,
                use_tls
            ).await?;
            let (response, stream) = transact(raw_request, stream).await?;
            if reuse {
                self.recycle_connection(host, port, use_tls, stream);
            }
            Ok(response)
        } else {
            Err(Error::NoTargetAuthority(request.target))
        }
    }

    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded::<WorkerMessage>();
        Self {
            work_in: sender,
            worker: Some(thread::spawn(move || worker(receiver))),
        }
    }

}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        println!("Sending stop message");
        self.work_in.unbounded_send(WorkerMessage::Stop).unwrap();
        println!("Joining worker thread");
        self.worker.take().unwrap().join().unwrap();
        println!("Worker thread joined");
    }
}

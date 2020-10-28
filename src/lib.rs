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

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionKey {
    host: String,
    port: u16,
    use_tls: bool,
}

type ConnectionSender = oneshot::Sender<Option<Box<dyn Connection>>>;
type ConnectionRequester = oneshot::Sender<ConnectionSender>;
type ConnectionRequestee = oneshot::Receiver<ConnectionSender>;

async fn monitor_connection(
    mut connection: Box<dyn Connection>,
    receiver: ConnectionRequestee,
) -> bool {
    println!("Now monitoring parked connection");
    let mut buffer = [0; 1];
    select!(
        _ = connection.read(&mut buffer).fuse() => {
            println!("Connection broken");
        },
        sender = receiver.fuse() => {
            println!("Received request to recycle connection");
            if let Ok(sender) = sender {
                println!("Sending connection to requester");
                // A failure here means the connection requester gave up
                // waiting for the connection.  It shouldn't happen since the
                // requestee should respond quickly, but it's still possible if
                // the requester is dumb and requested a connection only to
                // immediately give up.  If that happens, silently discard the
                // connection, as if we gave it to them only for them to drop
                // it.
                sender.send(Some(connection)).unwrap_or(());
            } else {
                println!("No path back to requester!");
            }
        },
    );
    false
}

struct ParkedConnectionPool {
    requesters: Vec<ConnectionRequester>,
    monitors: Vec<Pin<Box<dyn futures::Future<Output=bool>>>>,
}

impl ParkedConnectionPool {
    fn add(
        &mut self,
        connection: Box<dyn Connection>,
    ) {
        let (sender, receiver) = oneshot::channel();
        self.requesters.push(sender);
        self.monitors.push(monitor_connection(connection, receiver).boxed());
    }

    fn new() -> Self {
        Self {
            requesters: Vec::new(),
            monitors: Vec::new(),
        }
    }

    fn remove(&mut self) -> Option<ConnectionRequester> {
        self.requesters.pop()
    }
}

struct ParkedConnectionPools {
    pools: HashMap<ConnectionKey, ParkedConnectionPool>,
    kick: Option<oneshot::Sender<()>>,
}

impl ParkedConnectionPools {
    fn new() -> Self {
        Self {
            pools: HashMap::new(),
            kick: None,
        }
    }
}

fn give_parked_connection(
    connection_pools: &Rc<RefCell<ParkedConnectionPools>>,
    connection_key: ConnectionKey,
    connection: Box<dyn Connection>,
) {
    connection_pools.borrow_mut().pools.entry(connection_key)
        .or_insert_with(ParkedConnectionPool::new)
        .add(connection);
    if let Some(kick) = connection_pools.borrow_mut().kick.take() {
        println!("Kicking monitors");
        // It shouldn't be possible for the kick to fail, since the kick
        // receiver isn't dropped until either it receives the kick or
        // the worker is stopped, and this function is only called
        // by the worker.  So if it does fail, we want to know about it
        // since it would mean we have a bug.
        kick.send(()).unwrap();
    } else {
        println!("Cannot kick monitors");
    }
}

async fn take_parked_connection(
    connection_pools: &Rc<RefCell<ParkedConnectionPools>>,
    connection_key: ConnectionKey,
) -> Option<Box<dyn Connection>> {
    // Remove connection requester from the pools.
    let connection_requester = match connection_pools.borrow_mut().pools.entry(connection_key) {
        hash_map::Entry::Occupied(mut entry) => {
            println!("Found a connection requester");
            let connection_requester = entry.get_mut().remove();
            connection_requester
        },
        hash_map::Entry::Vacant(_) => {
            println!("No connection requesters found");
            None
        },
    };

    // Now that the connection requester is removed from the pools,
    // we can communicate with its monitor safely to try to recycle
    // the connection it's holding.  We share the pools with the monitor,
    // so it's not safe to hold a mutable reference when yielding.
    if let Some(connection_requester) = connection_requester {
        let (sender, receiver) = oneshot::channel();
        println!("Requesting connection from monitor");
        // It's possible for this to fail if the connection was broken and we
        // managed to take the connection requester from the pools before the
        // worker got around to it.
        if connection_requester.send(sender).is_err() {
            println!("Connection was broken");
            None
        } else {
            println!("Waiting for connection");
            // It's possible for this to fail if the connection is broken
            // while we're waiting for the monitor to receive our request
            // for the connection.
            let connection = receiver
                .await // <-- Make sure we're not holding the pools here!
                .unwrap_or(None);
            if connection.is_some() {
                println!("Got a connection");
            } else {
                println!("Connection was broken");
            }
            connection
        }
    } else {
        None
    }
}

async fn handle_messages(
    connection_pools: Rc<RefCell<ParkedConnectionPools>>,
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
                    give_parked_connection(&connection_pools, connection_key, connection);
                },
                WorkerMessage::TakeConnection{
                    connection_key,
                    return_channel,
                } => {
                    println!("Asked to recycle connection to {:?}", connection_key);
                    // It's possible for this to fail if the user gave up on a
                    // transaction before the worker was able to recycle a
                    // connection for use in sending the request.  If this
                    // happens, the connection is simply dropped.
                    return_channel.send(
                        take_parked_connection(&connection_pools, connection_key).await
                    ).unwrap_or(());
                },
                WorkerMessage::Stop => unreachable!()
            }
        }).await
}

async fn monitor_connections(
    connection_pools: Rc<RefCell<ParkedConnectionPools>>,
) {
    let mut monitors = Vec::new();
    let mut needs_kick = true;
    loop {
        monitors.extend(
            connection_pools.borrow_mut().pools.iter_mut()
                .map(|(_, connection_pool)| {
                    connection_pool.requesters.retain(|requester|
                        if requester.is_canceled() {
                            println!("Stream requester canceled");
                            false
                        } else {
                            println!("Stream requester not canceled");
                            true
                        }
                    );
                    connection_pool.monitors.drain(..)
                })
                .flatten()
        );
        connection_pools.borrow_mut().pools.retain(|_, pool|
            if pool.requesters.is_empty() {
                println!("Channel pool empty");
                false
            } else {
                println!("Channel pool not empty");
                true
            }
        );
        if needs_kick {
            println!("Setting up to kick");
            let (sender, receiver) = oneshot::channel();
            connection_pools.borrow_mut().kick = Some(sender);
            let kick_future = async {
                // It shouldn't be possible for this to fail, since once the
                // kick is set up, it isn't dropped until the kick is sent.
                // The enclosing method holds a reference to `connection_pools`
                // which holds the kick.  So if it does fail, we want to know
                // about it since it would mean we have a bug.
                receiver.await.unwrap();
                true
            }.boxed();
            monitors.push(kick_future);
        } else {
            println!("Already set up to kick");
        }
        println!("Waiting on monitors ({})", monitors.len());
        let (kicked, _, monitors_left) = futures::future::select_all(
            monitors.into_iter()
        ).await;
        needs_kick = if kicked {
            println!("Monitors added");
            true
        } else {
            println!("Monitor completed");
            false
        };
        monitors = monitors_left;
    }
}

async fn worker(
    receiver: mpsc::UnboundedReceiver<WorkerMessage>
) {
    println!("Worker started");
    let connection_pools = Rc::new(RefCell::new(ParkedConnectionPools::new()));
    select!(
        () = handle_messages(connection_pools.clone(), receiver).fuse() => (),
        () = monitor_connections(connection_pools).fuse() => (),
    );
    println!("Worker stopping");
}

async fn transact<RawRequest>(
    raw_request: RawRequest,
    mut connection: Box<dyn Connection>
) -> Result<(Response, Box<dyn Connection>), Error>
    where RawRequest: AsRef<[u8]>
{
    // Send the request to the server.
    connection.write_all(raw_request.as_ref()).await
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
        let received = connection.read(&mut receive_buffer[left_over..]).await
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
            return Ok((response, connection));
        }
    }
}

enum WorkerMessage {
    Stop,
    GiveConnection{
        connection_key: ConnectionKey,
        connection: Box<dyn Connection>,
    },
    TakeConnection{
        connection_key: ConnectionKey,
        return_channel: ConnectionSender,
    },
}

pub struct ConnectResults {
    pub connection: Box<dyn Connection>,
    pub is_reused: bool,
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
        connection: Box<dyn Connection>
    ) where Host: AsRef<str> {
        let host = host.as_ref();
        let connection_key = ConnectionKey{
            host: String::from(host),
            port,
            use_tls
        };
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and isn't dropped until the client
        // itself is dropped.  So if it does fail, we want to know about it
        // since it would mean we have a bug.
        self.work_in.unbounded_send(WorkerMessage::GiveConnection{
            connection_key,
            connection
        }).unwrap();
    }

    async fn request_recycled_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool
    ) -> Option<Box<dyn Connection>>
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
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and isn't dropped until the client
        // itself is dropped.  So if it does fail, we want to know about it
        // since it would mean we have a bug.
        self.work_in.unbounded_send(WorkerMessage::TakeConnection{
            connection_key,
            return_channel: sender
        }).unwrap();
        // This can fail if the connection is broken before the monitor
        // receives our request for the connection.
        let connection = receiver.await.unwrap_or(None);
        if connection.is_some() {
            println!("Got a recycled connection");
        } else {
            println!("No recycled connection available");
        }
        connection
    }

    pub async fn connect<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
    ) -> Result<ConnectResults, Error>
        where Host: AsRef<str>
    {
        // Check if a connection is already made, and if so, reuse it.
        let host = host.as_ref();
        if let Some(connection) = self.request_recycled_connection(host, port, use_tls).await {
            return Ok(ConnectResults{
                connection,
                is_reused: true,
            });
        }

        // Connect to the server.
        let address = &format!("{}:{}", host, port);
        println!("Connecting to '{}'...", address);
        let connection = TcpStream::connect(address).await
            .map_err(Error::UnableToConnect)?;
        println!(
            "Connected (address: {}).",
            connection.peer_addr()
                .map_err(Error::UnableToGetPeerAddress)?
        );

        // Wrap with TLS connector if necessary.
        Ok(ConnectResults{
            connection: if use_tls {
                println!("Using TLS.");
                let tls_connector = TlsConnector::default();
                let tls_connection = tls_connector.connect(host, connection).await
                    .map_err(Error::TlsHandshake)?;
                Box::new(tls_connection)
            } else {
                println!("Not using TLS.");
                Box::new(connection)
            },
            is_reused: false,
        })
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

            // Attempt to connect to the server, sending the request, and
            // receiving back a response.  This repeats if the connection is
            // dropped and it was reused from a previous connection, to avoid
            // the problem where we lose the race between sending a new request
            // and the server timing out the connection and closing it.
            loop {
                // Obtain a connection to the server.  The connection
                // might be reused from a previous request.
                let use_tls = matches!(
                    scheme.as_deref(),
                    Some("https") | Some("wss")
                );
                let connection_results = self.connect(
                    host,
                    port,
                    use_tls
                ).await?;

                // Attempt to send the request to the server and receive back a
                // response.  If we get disconnected and the connection was
                // reused, we will try again.
                let (response, connection) = match transact(
                    &raw_request,
                    connection_results.connection
                ).await {
                    Err(Error::Disconnected) if connection_results.is_reused => {
                        println!("Reused connection broken; trying again");
                        continue;
                    },
                    Err(error) => Err(error),
                    Ok(results) => Ok(results),
                }?;

                // Park the connection for possible reuse.
                if reuse {
                    self.recycle_connection(host, port, use_tls, connection);
                }

                // Return the response received.
                return Ok(response);
            }
        } else {
            Err(Error::NoTargetAuthority(request.target))
        }
    }

    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded::<WorkerMessage>();
        Self {
            work_in: sender,
            worker: Some(thread::spawn(|| block_on(worker(receiver)))),
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
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and we haven't joined or dropped the
        // worker yet (we will a few lines later).  So if it does fail, we want
        // to know about it since it would mean we have a bug.
        self.work_in.unbounded_send(WorkerMessage::Stop).unwrap();
        println!("Joining worker thread");
        // This shouldn't fail unless the worker panics.  If it does, there's
        // no reason why we shouldn't panic as well.
        self.worker.take().unwrap().join().unwrap();
        println!("Worker thread joined");
    }
}

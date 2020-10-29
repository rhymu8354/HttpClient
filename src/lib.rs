#![warn(clippy::pedantic)]
// TODO: Remove this once ready to publish.
#![allow(clippy::missing_errors_doc)]
// TODO: Uncomment this once ready to publish.
//#![warn(missing_docs)]

mod error;

use async_std::net::TcpStream;
use async_tls::TlsConnector;
use async_trait::async_trait;
pub use error::Error;
use futures::{
    AsyncRead,
    AsyncReadExt,
    AsyncWrite,
    AsyncWriteExt,
};
use pantry::{Pantry, Perishable};
use rhymuweb::{
    Response,
    ResponseParseStatus,
    Request,
};

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

#[async_trait]
impl Perishable for Box<dyn Connection> {
    async fn perished(&mut self) {
        let mut buffer = [0; 1];
        self.read(&mut buffer).await.unwrap_or(0);
        println!("Connection broken");
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionKey {
    host: String,
    port: u16,
    use_tls: bool,
}

/// This is used with [`fetch`] to tell the client how the underlying
/// TCP connection will be used, especially after the currently requested
/// resource has been successfully fetched.
///
/// [`fetch`]: struct.HttpClient.html#method.fetch
pub enum ConnectionUse {
    /// Close the connection after retrieving the resource.  When this is used,
    /// the client adds the header "Connection: close" as specified in [`IETF
    /// RFC 7230 section
    /// 6.1`](https://tools.ietf.org/html/rfc7230#section-6.1).
    SingleResource,

    /// Keep the connection open in case the user wants to fetch another
    /// resource from the same server.
    MultipleResources,

    /// Attempt to upgrade the connection for use in a higher-level protocol.
    Upgrade{
        /// This is the value to set for the "Upgrade" header in the request.
        protocol: String,
    },
}

pub struct ConnectResults {
    pub connection: Box<dyn Connection>,
    pub is_reused: bool,
}

/// This holds all the results from fetching a web resource.
pub struct FetchResults {
    /// This contains the parts of the HTTP response returned by the server.
    pub response: Response,

    /// If the connection was upgraded to a higher-level protocol, this will
    /// hold the underlying transport layer connection, which becomes
    /// the caller's responsibility.
    pub connection: Option<Box<dyn Connection>>,
}

pub struct HttpClient {
    pantry: Pantry<ConnectionKey, Box<dyn Connection>>,
}

impl HttpClient {
    async fn connect<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
    ) -> Result<ConnectResults, Error>
        where Host: AsRef<str>
    {
        // Check if a connection is already made, and if so, reuse it.
        let host = host.as_ref();
        if let Some(connection) = self.recycle_connection(host, port, use_tls).await {
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
        connection_use: ConnectionUse,
    ) -> Result<FetchResults, Error>
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
            request.headers.set_header("Accept-Encoding", "gzip, deflate");
            match &connection_use {
                ConnectionUse::SingleResource => {
                    request.headers.set_header("Connection", "Close");
                },
                ConnectionUse::MultipleResources => {},
                ConnectionUse::Upgrade{protocol} => {
                    request.headers.set_header("Upgrade", protocol);
                },
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
            let use_tls = matches!(
                scheme.as_deref(),
                Some("https") | Some("wss")
            );
            loop {
                // Obtain a connection to the server.  The connection
                // might be reused from a previous request.
                let connection_results = self.connect(
                    host,
                    port,
                    use_tls
                ).await?;

                // Attempt to send the request to the server and receive back a
                // response.  If we get disconnected and the connection was
                // reused, we will try again.
                let (mut response, connection) = match Self::transact(
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

                // Handle content encodings.
                response.body = rhymuweb::coding::decode_body(
                    &mut response.headers,
                    response.body
                )
                    .map_err(Error::BadResponse)?;

                // Results depend on how we're supposed to use the connection,
                // and what the response status code was.
                if let (ConnectionUse::Upgrade{..}, 101) = (&connection_use, &response.status_code) {
                    return Ok(FetchResults{
                        response,
                        connection: Some(connection),
                    });
                } else {
                    // Park the connection for possible reuse.
                    if let ConnectionUse::MultipleResources = &connection_use {
                        self.store_connection(host, port, use_tls, connection);
                    }

                    // Return the response received.
                    return Ok(FetchResults{
                        response,
                        connection: None,
                    });
                }
            }
        } else {
            Err(Error::NoTargetAuthority(request.target))
        }
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            pantry: Pantry::new(),
        }
    }

    async fn recycle_connection<Host>(
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
        self.pantry.fetch(connection_key).await
    }

    fn store_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
        connection: Box<dyn Connection>
    )
        where Host: AsRef<str>
    {
        let host = host.as_ref();
        let connection_key = ConnectionKey{
            host: String::from(host),
            port,
            use_tls
        };
        self.pantry.store(connection_key, connection);
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
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

//! This crate provides the [`HttpClient`] type, which can be used to send
//! Hypertext Transfer Protocol (HTTP) 1.1 requests to web servers and receive
//! back responses from them.  Support is also included to upgrade connections
//! to higher-level protocols such as [`WebSockets`], as long as the user takes
//! care of including any required headers and/or body for the request.
//!
//! # Examples
//!
//! ```no_run
//! # extern crate futures;
//! # extern crate rhymessage;
//! # extern crate rhymuri;
//! # extern crate rhymuweb;
//! use futures::executor;
//! use rhymuri::Uri;
//! use rhymuweb::Request;
//! use rhymuweb_client::{
//!     ConnectionUse,
//!     HttpClient,
//! };
//!
//! # fn main() -> Result<(), rhymessage::Error> {
//! let client = HttpClient::new();
//! let mut request = Request::new();
//! request.target = Uri::parse("http://www.example.com/foo?bar").unwrap();
//! let response = executor::block_on(async {
//!     client.fetch(request, ConnectionUse::SingleResource).await
//! })
//! .unwrap()
//! .response;
//! assert_eq!(200, response.status_code);
//! # Ok(())
//! # }
//! ```
//!
//! [`HttpClient`]: struct.HttpClient.html
//! [`WebSockets`]: https://tools.ietf.org/html/rfc6455

#![warn(clippy::pedantic)]
#![warn(missing_docs)]

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
use pantry::{
    Pantry,
    Perishable,
};
use rhymuweb::{
    Request,
    Response,
    ResponseParseStatus,
};
use rustls::ClientConfig as TlsClientConfig;
use std::{
    fmt::Write,
    sync::Arc,
};

/// This is a collection of traits which the connections established by
/// [`HttpClient`] implement.
///
/// [`HttpClient`]: struct.HttpClient.html
pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

#[async_trait]
impl Perishable for Box<dyn Connection> {
    async fn perished(&mut self) {
        let mut buffer = [0; 1];
        self.read(&mut buffer).await.unwrap_or(0);
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
    Upgrade {
        /// This is the value to set for the "Upgrade" header in the request.
        protocol: String,
    },
}

struct ConnectResults {
    connection: Box<dyn Connection>,
    is_reused: bool,
}

/// This holds information to provide to a higher-level protocol in the case
/// where an HTTP connection is upgraded after a response.
pub struct ConnectionUpgrade {
    /// This holds the underlying transport layer connection, which becomes the
    /// caller's responsibility.
    pub connection: Box<dyn Connection>,

    /// This holds any extra bytes received by the client after the end
    /// of the upgrade response.
    pub trailer: Vec<u8>,
}

/// This holds all the results from fetching a web resource.
pub struct FetchResults {
    /// This contains the parts of the HTTP response returned by the server.
    pub response: Response,

    /// If the connection was upgraded to a higher-level protocol, this
    /// contains all the information to provide to that protocol.
    pub upgrade: Option<ConnectionUpgrade>,
}

/// This type is used to fetch resources from web servers using the Hypertext
/// Transfer Protocol (HTTP) 1.1.  Use the [`fetch`] asynchronous function to
/// send a request and obtain a response.
///
/// [`fetch`]: #method.fetch
#[must_use]
pub struct HttpClient {
    tls_config: Option<Arc<TlsClientConfig>>,
    pantry: Pantry<ConnectionKey, Box<dyn Connection>>,
}

impl HttpClient {
    async fn connect<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
    ) -> Result<ConnectResults, Error>
    where
        Host: AsRef<str>,
    {
        // Check if a connection is already made, and if so, reuse it.
        let host = host.as_ref();
        if let Some(connection) =
            self.recycle_connection(host, port, use_tls).await
        {
            return Ok(ConnectResults {
                connection,
                is_reused: true,
            });
        }

        // Connect to the server.
        let address = &format!("{}:{}", host, port);
        let connection = TcpStream::connect(address)
            .await
            .map_err(Error::UnableToConnect)?;

        // Wrap with TLS connector if necessary.
        Ok(ConnectResults {
            connection: if use_tls {
                let tls_connector = match &self.tls_config {
                    Some(tls_config) => TlsConnector::from(tls_config.clone()),
                    None => TlsConnector::default(),
                };
                let tls_connection = tls_connector
                    .connect(host, connection)
                    .await
                    .map_err(Error::TlsHandshake)?;
                Box::new(tls_connection)
            } else {
                Box::new(connection)
            },
            is_reused: false,
        })
    }

    /// Send the given HTTP request and obtain a response from the web server
    /// identified in the request.  Make sure to fill the request with the
    /// correct URI of the resource requested, and provide any necessary
    /// headers (other than `Host`, `Connection`, and `Content-Length`, which
    /// are set automatically by the client).
    ///
    /// Connections to web servers may be reused for multiple connections if
    /// [`ConnectionUse::MultipleResources`] is specified.  Specifying
    /// [`ConnectionUse::Upgrade`] instead will request that the connection be
    /// upgraded to a higher-level protocol after the response is returned.
    ///
    /// # Errors
    ///
    /// * [`Error::HostNotValidText`] &ndash; the host name in authority of the
    ///   target URI contains bytes which could not be parsed as valid text.
    /// * [`Error::NoTargetAuthority`] &ndash; no authority was included in the
    ///   target URI in the request.
    /// * [`Error::UnableToDetermineServerPort`] &ndash; no port number was
    ///   included in the authority of the target URI, and no default port
    ///   number could be determined from the target URI scheme.
    /// * [`Error::BadRequest`] &ndash; the HTTP request could not be encoded as
    ///   a byte stream to be sent to the web server.
    /// * [`Error::UnableToConnect`] &ndash; a connection to the web server
    ///   could not be established.
    /// * [`Error::TlsHandshake`] &ndash; a connection to the web server was
    ///   established, but the connection could not be secured using Transport
    ///   Layer Security (TLS).
    /// * [`Error::Disconnected`] &ndash; the connection to the web server was
    ///   disconnected before the response to the request could be obtained.
    /// * [`Error::UnableToSend`] &ndash; a connection to the web server was
    ///   established, but the request could not be sent using it.
    /// * [`Error::UnableToReceive`] &ndash; a connection to the web server was
    ///   established, but the response could not be received using it.
    /// * [`Error::BadResponse`] &ndash; the response from the web server could
    ///   not be parsed correctly.
    ///
    /// [`ConnectionUse::MultipleResources`]:
    /// enum.ConnectionUse.html#variant.MultipleResources
    /// [`ConnectionUse::Upgrade`]:
    /// enum.ConnectionUse.html#variant.Upgrade
    /// [`Error::HostNotValidText`]: enum.Error.html#variant.HostNotValidText
    /// [`Error::NoTargetAuthority`]: enum.Error.html#variant.NoTargetAuthority
    /// [`Error::UnableToDetermineServerPort`]:
    /// enum.Error.html#variant.UnableToDetermineServerPort
    /// [`Error::BadRequest`]: enum.Error.html#variant.BadRequest
    /// [`Error::UnableToConnect`]: enum.Error.html#variant.UnableToConnect
    /// [`Error::TlsHandshake`]: enum.Error.html#variant.TlsHandshake
    /// [`Error::Disconnected`]: enum.Error.html#variant.Disconnected
    /// [`Error::UnableToSend`]: enum.Error.html#variant.UnableToSend
    /// [`Error::UnableToReceive`]: enum.Error.html#variant.UnableToReceive
    /// [`Error::BadResponse`]: enum.Error.html#variant.BadResponse
    // TODO: Refactor this
    #[allow(clippy::too_many_lines)]
    pub async fn fetch<Req>(
        &self,
        request: Req,
        connection_use: ConnectionUse,
    ) -> Result<FetchResults, Error>
    where
        Req: Into<Request>,
    {
        let mut request: Request = request.into();
        if let Some(authority) = request.target.take_authority() {
            // Remove scheme from the target.
            let scheme = request.target.take_scheme();

            // Determine the socket address of the server given
            // the hostname and port number.
            let port = authority
                .port()
                .or_else(|| match scheme.as_deref() {
                    Some("http") | Some("ws") => Some(80),
                    Some("https") | Some("wss") => Some(443),
                    _ => None,
                })
                .ok_or_else(|| {
                    Error::UnableToDetermineServerPort(request.target.clone())
                })?;

            // Determine the server hostname and include it in the request
            // headers.  If we are connecting on a non-standard port,
            // include the port number as well.
            let host =
                std::str::from_utf8(authority.host()).map_err(|source| {
                    Error::HostNotValidText {
                        host: authority.host().to_vec(),
                        source,
                    }
                })?;
            let mut host_header_value = String::from(host);
            if match (scheme.as_deref(), port) {
                (Some("http"), 80)
                | (Some("ws"), 80)
                | (Some("https"), 443)
                | (Some("wss"), 443) => false,
                (_, _) => true,
            } {
                write!(host_header_value, ":{}", port)
                    .expect("unable to append port number to hostname");
            }
            request.headers.set_header("Host", host_header_value);

            // Store the body size in the request headers.
            if !request.body.is_empty() {
                request.headers.set_header(
                    "Content-Length",
                    request.body.len().to_string(),
                );
            }

            // Set other headers specific to the user agent.
            request.headers.set_header("Accept-Encoding", "gzip, deflate");
            match &connection_use {
                ConnectionUse::SingleResource => {
                    request.headers.set_header("Connection", "Close");
                },
                ConnectionUse::MultipleResources => {},
                ConnectionUse::Upgrade {
                    protocol,
                } => {
                    request.headers.set_header("Connection", "upgrade");
                    request.headers.set_header("Upgrade", protocol);
                },
            }

            // Generate the raw request byte stream.
            let raw_request = request.generate().map_err(Error::BadRequest)?;

            // Attempt to connect to the server, sending the request, and
            // receiving back a response.  This repeats if the connection is
            // dropped and it was reused from a previous connection, to avoid
            // the problem where we lose the race between sending a new request
            // and the server timing out the connection and closing it.
            let use_tls =
                matches!(scheme.as_deref(), Some("https") | Some("wss"));
            loop {
                // Obtain a connection to the server.  The connection
                // might be reused from a previous request.
                let connection_results =
                    self.connect(host, port, use_tls).await?;

                // Attempt to send the request to the server and receive back a
                // response.  If we get disconnected and the connection was
                // reused, we will try again.
                let (mut response, connection, trailer) = match Self::transact(
                    &raw_request,
                    connection_results.connection,
                )
                .await
                {
                    Err(Error::Disconnected)
                        if connection_results.is_reused =>
                    {
                        continue;
                    }
                    Err(error) => Err(error),
                    Ok(results) => Ok(results),
                }?;

                // Handle content encodings.
                response.body = rhymuweb::coding::decode_body(
                    &mut response.headers,
                    response.body,
                )
                .map_err(Error::BadResponse)?;

                // Results depend on how we're supposed to use the connection,
                // and what the response status code was.
                if let (
                    ConnectionUse::Upgrade {
                        ..
                    },
                    101,
                ) = (&connection_use, &response.status_code)
                {
                    return Ok(FetchResults {
                        response,
                        upgrade: Some(ConnectionUpgrade {
                            connection,
                            trailer,
                        }),
                    });
                } else {
                    // Park the connection for possible reuse.
                    if let ConnectionUse::MultipleResources = &connection_use {
                        self.store_connection(host, port, use_tls, connection);
                    }

                    // Return the response received.
                    return Ok(FetchResults {
                        response,
                        upgrade: None,
                    });
                }
            }
        } else {
            Err(Error::NoTargetAuthority(request.target))
        }
    }

    /// Return a new HTTP client which can send requests to web servers and
    /// obtain back responses.  The client will use the given configuration if
    /// it needs to connect using TLS.
    pub fn from_tls_config(tls_config: Arc<TlsClientConfig>) -> Self {
        Self {
            tls_config: Some(tls_config),
            pantry: Pantry::new(),
        }
    }

    /// Return a new HTTP client which can send requests to web servers and
    /// obtain back responses.  The client will use a default configuration
    /// if it needs to connect using TLS.
    pub fn new() -> Self {
        Self {
            tls_config: None,
            pantry: Pantry::new(),
        }
    }

    async fn recycle_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
    ) -> Option<Box<dyn Connection>>
    where
        Host: AsRef<str>,
    {
        let host = host.as_ref();
        let connection_key = ConnectionKey {
            host: String::from(host),
            port,
            use_tls,
        };
        self.pantry.fetch(connection_key).await
    }

    fn store_connection<Host>(
        &self,
        host: Host,
        port: u16,
        use_tls: bool,
        connection: Box<dyn Connection>,
    ) where
        Host: AsRef<str>,
    {
        let host = host.as_ref();
        let connection_key = ConnectionKey {
            host: String::from(host),
            port,
            use_tls,
        };
        self.pantry.store(connection_key, connection);
    }

    async fn transact<RawRequest>(
        raw_request: RawRequest,
        mut connection: Box<dyn Connection>,
    ) -> Result<(Response, Box<dyn Connection>, Vec<u8>), Error>
    where
        RawRequest: AsRef<[u8]>,
    {
        // Send the request to the server.
        connection
            .write_all(raw_request.as_ref())
            .await
            .map_err(Error::UnableToSend)?;

        // Receive the response from the server.
        let mut response = Response::new();
        let mut receive_buffer = Vec::new();
        loop {
            let left_over = receive_buffer.len();
            receive_buffer.resize(left_over + 65536, 0);
            let received = connection
                .read(&mut receive_buffer[left_over..])
                .await
                .map_err(Error::UnableToReceive)
                .and_then(|received| match received {
                    0 => Err(Error::Disconnected),
                    received => Ok(received),
                })?;
            receive_buffer.truncate(left_over + received);
            let response_status = response
                .parse(&mut receive_buffer)
                .map_err(Error::BadResponse)?;
            receive_buffer.drain(0..response_status.consumed);
            let status_code = response.status_code;
            if response_status.status == ResponseParseStatus::Complete {
                return Ok((
                    response,
                    connection,
                    if status_code == 101 {
                        receive_buffer
                    } else {
                        Vec::new()
                    },
                ));
            }
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

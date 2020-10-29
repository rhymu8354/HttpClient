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
use flate2::bufread::{
    DeflateDecoder,
    GzDecoder,
};
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
use std::{
    io::Read as _,
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

pub struct ConnectResults {
    pub connection: Box<dyn Connection>,
    pub is_reused: bool,
}

fn split_at<'a>(
    composite: &'a str,
    delimiter: char
) -> Option<(&'a str, &'a str)> {
    match composite.find(delimiter) {
        Some(delimiter) => Some((
            &composite[..delimiter],
            &composite[delimiter+1..]
        )),
        None => None,
    }
}

pub struct HttpClient {
    pantry: Pantry<ConnectionKey, Box<dyn Connection>>,
}

impl HttpClient {
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

    fn decode_body(response: &mut Response) -> Result<(), Error> {
        let mut codings = response.headers.header_tokens("Content-Encoding");
        let mut body = Vec::new();
        std::mem::swap(&mut body, &mut response.body);
        while !codings.is_empty() {
            let coding = codings.pop().unwrap();
            match coding.as_ref() {
                "gzip" => body = Self::gzip_decode(body)?,
                "deflate" => body = Self::deflate_decode(body)?,
                _ => {
                    codings.push(coding);
                    break;
                },
            };
        }
        std::mem::swap(&mut body, &mut response.body);
        if codings.is_empty() {
            response.headers.remove_header("Content-Encoding");
        } else {
            response.headers.set_header(
                "Content-Encoding",
                codings.join(", ")
            );
        }
        response.headers.set_header(
            "Content-Length",
            response.body.len().to_string()
        );
        Ok(())
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
            request.headers.set_header("Accept-Encoding", "gzip, deflate");
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
                Self::decode_body(&mut response)?;

                // Park the connection for possible reuse.
                if reuse {
                    self.store_connection(host, port, use_tls, connection);
                }

                // Return the response received.
                return Ok(response);
            }
        } else {
            Err(Error::NoTargetAuthority(request.target))
        }
    }

    fn gzip_decode<B>(body: B) -> Result<Vec<u8>, Error>
        where B: AsRef<[u8]>
    {
        let body = body.as_ref();
        let mut decoder = GzDecoder::new(body);
        let mut body = Vec::new();
        decoder.read_to_end(&mut body)
            .map_err(Error::BadContentEncoding)?;
        Ok(body)
    }

    fn decode_body_as_text(response: &Response) -> Option<String> {
        if let Some(content_type) = response.headers.header_value("Content-Type") {
            let (type_subtype, parameters) = match content_type.find(';') {
                Some(delimiter) => (
                    &content_type[..delimiter],
                    &content_type[delimiter+1..]
                ),
                None => (&content_type[..], ""),
            };
            if let Some((r#type, _)) = split_at(type_subtype, '/') {
                if !r#type.eq_ignore_ascii_case("text") {
                    return None;
                }
                if let Some(charset) = parameters.split(';')
                    .map(str::trim)
                    .filter_map(|parameter| split_at(parameter, '='))
                    .find_map(|(name, value)| {
                        if name.eq_ignore_ascii_case("charset") {
                            Some(value)
                        } else {
                            None
                        }
                    })
                {
                    if let Some(encoding) = encoding_rs::Encoding::for_label(charset.as_bytes()) {
                        return encoding.decode_without_bom_handling_and_without_replacement(
                            &response.body[..]
                        )
                            .map(String::from);
                    }
                }
            }
        }
        None
    }

    fn deflate_decode<B>(body: B) -> Result<Vec<u8>, Error>
        where B: AsRef<[u8]>
    {
        let body = body.as_ref();
        let mut decoder = DeflateDecoder::new(body);
        let mut body = Vec::new();
        decoder.read_to_end(&mut body)
            .map_err(Error::BadContentEncoding)?;
        Ok(body)
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

#[cfg(test)]
mod tests {

    #![allow(clippy::string_lit_as_bytes)]
    #![allow(clippy::non_ascii_literal)]

    use super::*;

    #[test]
    fn gzip_decode() {
        let body: &[u8] = &[
            0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x0A, 0xF3, 0x48, 0xCD, 0xC9, 0xC9, 0xD7,
            0x51, 0x08, 0xCF, 0x2F, 0xCA, 0x49, 0x51, 0x04,
            0x00, 0xD0, 0xC3, 0x4A, 0xEC, 0x0D, 0x00, 0x00,
            0x00,
        ];
        let body = HttpClient::gzip_decode(body);
        assert!(body.is_ok());
        let body = body.unwrap();
        assert_eq!("Hello, World!".as_bytes(), body);
    }

    #[test]
    fn gzip_decode_empty_input() {
        let body: &[u8] = &[];
        let body = HttpClient::gzip_decode(body);
        assert!(matches!(
            body,
            Err(Error::BadContentEncoding(_))
        ));
    }

    #[test]
    fn gzip_decode_junk() {
        let body: &[u8] = b"Hello, this is certainly not gzipped data!";
        let body = HttpClient::gzip_decode(body);
        assert!(matches!(
            body,
            Err(Error::BadContentEncoding(_))
        ));
    }

    #[test]
    fn gzip_decode_empty_output() {
        let body: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x08, 0x2d, 0xac, 0xca, 0x5b,
            0x00, 0x03, 0x74, 0x65, 0x73, 0x74, 0x2e, 0x74,
            0x78, 0x74, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let body = HttpClient::gzip_decode(body);
        assert!(body.is_ok());
        let body = body.unwrap();
        assert_eq!("".as_bytes(), body);
    }

    #[test]
    fn deflate_decode() {
        let body: &[u8] = &[
            0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0xd7, 0x51, 0x08,
            0xcf, 0x2f, 0xca, 0x49, 0x51, 0x04, 0x00,
        ];
        let body = HttpClient::deflate_decode(body);
        assert!(body.is_ok());
        let body = body.unwrap();
        assert_eq!("Hello, World!".as_bytes(), body);
    }

    #[test]
    fn deflate_decode_empty_input() {
        let body: &[u8] = &[];
        let body = HttpClient::deflate_decode(body);
        assert!(matches!(
            body,
            Err(Error::BadContentEncoding(_))
        ));
    }

    #[test]
    fn deflate_decode_junk() {
        let body: &[u8] = b"Hello, this is certainly not deflated data!";
        let body = HttpClient::deflate_decode(body);
        assert!(matches!(
            body,
            Err(Error::BadContentEncoding(_))
        ));
    }

    #[test]
    fn deflate_decode_empty_output() {
        let body: &[u8] = &[
            0x03, 0x00,
        ];
        let body = HttpClient::deflate_decode(body);
        assert!(body.is_ok());
        let body = body.unwrap();
        assert_eq!("".as_bytes(), body);
    }

    #[test]
    fn decode_body_not_encoded() {
        let mut response = Response::new();
        response.body = "Hello, World!".into();
        response.headers.set_header(
            "Content-Length",
            response.body.len().to_string()
        );
        HttpClient::decode_body(&mut response).unwrap();
        assert_eq!("Hello, World!".as_bytes(), response.body);
    }

    #[test]
    fn decode_body_gzipped() {
        let mut response = Response::new();
        std::io::Write::write_all(&mut response.body, &[
            0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x0A, 0xF3, 0x48, 0xCD, 0xC9, 0xC9, 0xD7,
            0x51, 0x08, 0xCF, 0x2F, 0xCA, 0x49, 0x51, 0x04,
            0x00, 0xD0, 0xC3, 0x4A, 0xEC, 0x0D, 0x00, 0x00,
            0x00,
        ]).unwrap();
        response.headers.set_header(
            "Content-Length",
            response.body.len().to_string()
        );
        response.headers.set_header("Content-Encoding", "gzip");
        HttpClient::decode_body(&mut response).unwrap();
        assert_eq!("Hello, World!".as_bytes(), response.body);
        assert_eq!(
            response.body.len().to_string(),
            response.headers.header_value("Content-Length").unwrap()
        );
        assert!(!response.headers.has_header("Content-Encoding"));
    }

    #[test]
    fn decode_body_deflated_then_gzipped() {
        let mut response = Response::new();
        std::io::Write::write_all(&mut response.body, &[
            0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xFF, 0xFB, 0xEC, 0x71, 0xF6, 0xE4, 0xC9,
            0xEB, 0x81, 0x1C, 0xE7, 0xF5, 0x4F, 0x79, 0x06,
            0xB2, 0x30, 0x00, 0x00, 0x87, 0x6A, 0xB2, 0x3A,
            0x0F, 0x00, 0x00, 0x00,
        ]).unwrap();
        response.headers.set_header(
            "Content-Length",
            response.body.len().to_string()
        );
        response.headers.set_header("Content-Encoding", "deflate, gzip");
        HttpClient::decode_body(&mut response).unwrap();
        assert_eq!("Hello, World!".as_bytes(), response.body);
        assert_eq!(
            response.body.len().to_string(),
            response.headers.header_value("Content-Length").unwrap()
        );
        assert!(!response.headers.has_header("Content-Encoding"));
    }

    #[test]
    fn decode_body_unknown_coding_then_gzipped() {
        let mut response = Response::new();
        std::io::Write::write_all(&mut response.body, &[
            0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x0A, 0xF3, 0x48, 0xCD, 0xC9, 0xC9, 0xD7,
            0x51, 0x08, 0xCF, 0x2F, 0xCA, 0x49, 0x51, 0x04,
            0x00, 0xD0, 0xC3, 0x4A, 0xEC, 0x0D, 0x00, 0x00,
            0x00,
        ]).unwrap();
        response.headers.set_header(
            "Content-Length",
            response.body.len().to_string()
        );
        response.headers.set_header("Content-Encoding", "foobar, gzip");
        HttpClient::decode_body(&mut response).unwrap();
        assert_eq!("Hello, World!".as_bytes(), response.body);
        assert_eq!(
            response.body.len().to_string(),
            response.headers.header_value("Content-Length").unwrap()
        );
        assert_eq!(
            Some("foobar"),
            response.headers.header_value("Content-Encoding").as_deref()
        );
    }

    #[test]
    fn body_to_string_valid_encoding_iso_8859_1() {
        let mut response = Response::new();
        response.body = b"Tickets to Hogwarts leaving from Platform 9\xbe are \xa310 each".to_vec();
        response.headers.set_header("Content-Type", "text/plain; charset=iso-8859-1");
        assert_eq!(
            Some("Tickets to Hogwarts leaving from Platform 9¾ are £10 each"),
            HttpClient::decode_body_as_text(&response).as_deref()
        );
    }

    #[test]
    fn body_to_string_valid_encoding_utf_8() {
        let mut response = Response::new();
        response.body = "Tickets to Hogwarts leaving from Platform 9¾ are £10 each".as_bytes().to_vec();
        response.headers.set_header("Content-Type", "text/plain; charset=utf-8");
        assert_eq!(
            Some("Tickets to Hogwarts leaving from Platform 9¾ are £10 each"),
            HttpClient::decode_body_as_text(&response).as_deref()
        );
    }

    #[test]
    fn body_to_string_invalid_encoding_utf8() {
        let mut response = Response::new();
        response.body = b"Tickets to Hogwarts leaving from Platform 9\xbe are \xa310 each".to_vec();
        response.headers.set_header("Content-Type", "text/plain; charset=utf-8");
        assert!(HttpClient::decode_body_as_text(&response).is_none());
    }
}

use async_ctrlc::CtrlC;
use async_std::prelude::*;
use async_std::net::TcpStream;
use async_tls::TlsConnector;
use futures::{
    AsyncRead,
    AsyncWrite,
    select
};
use futures::future::FutureExt;
use rhymuri::Uri;
use rhymuweb::{
    Response,
    ResponseParseStatus,
    Request,
};
use rhymuweb_client::Error;
use std::error::Error as _;
use structopt::StructOpt;

trait Stream: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Stream for T {}

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

    // task::sleep(Duration::from_secs(5)).await;
    // response.headers.add_header(Header{
    //     name: "Host".into(),
    //     value: "buddy.local".into(),
    // });
    // Ok(response)
}

async fn fetch<Req>(request: Req) -> Result<Response, Error>
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
        request.headers.set_header("Connection", "Close");

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
        let address = &format!("{}:{}", host, port);

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
        println!("Connecting to '{}'...", address);
        let stream = TcpStream::connect(address).await
            .map_err(Error::UnableToConnect)?;
        println!(
            "Connected (address: {}).",
            stream.peer_addr()
                .map_err(Error::UnableToGetPeerAddress)?
        );

        // Wrap with TLS connector if necessary.
        let stream: Box<dyn Stream> = if matches!(
            scheme.as_deref(),
            Some("https") | Some("wss")
        ) {
            println!("Using TLS.");
            let tls_connector = TlsConnector::default();
            let tls_stream = tls_connector.connect(host, stream).await
                .map_err(Error::TlsHandshake)?;
            Box::new(tls_stream)
        } else {
            println!("Not using TLS.");
            Box::new(stream)
        };
        Ok(transact(raw_request, stream).await?.0)
    } else {
        Err(Error::NoTargetAuthority(request.target))
    }
}

#[derive(Clone, StructOpt)]
struct Opts {
    /// URI of resource to request
    uri: String,
}

fn main() {
    let opts: Opts = Opts::from_args();
    let mut request = Request::new();
    request.target = Uri::parse(opts.uri).unwrap();
    match futures::executor::block_on(async {
        select!(
            response = fetch(request).fuse() => Some(response),
            () = CtrlC::new().unwrap().fuse() => None,
        )
    }) {
        Some(Err(error)) => {
            match error.source() {
                Some(source) => eprintln!("error: {} ({})", error, source),
                None => eprintln!("error: {}", error),
            };
        },
        Some(Ok(response)) => {
            println!("Response:");
            println!("{}", "=".repeat(78));
            println!("{} {}", response.status_code, response.reason_phrase);
            for header in response.headers {
                println!("{}: {}", header.name, header.value);
            }
            println!();
            match String::from_utf8(response.body) {
                Err(_) => println!("(Body cannot be decoded as UTF-8)"),
                Ok(body) => println!("{}", body),
            };
            println!("{}", "=".repeat(78));
        },
        None => {
            println!("(Ctrl+C pressed; aborted)");
        },
    };
}

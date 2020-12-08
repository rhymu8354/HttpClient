use anyhow::anyhow;
use async_ctrlc::CtrlC;
use futures::{
    future::FutureExt,
    select,
};
use rhymuri::Uri;
use rhymuweb::Request;
use rhymuweb_client::HttpClient;
use std::{
    error::Error as _,
    fs::File,
    io::Read,
    sync::Arc,
};
use structopt::StructOpt;

#[derive(Clone, StructOpt)]
struct Opts {
    /// URI of resource to request
    #[structopt(default_value = "http://buddy.local:8080/")]
    uri: String,

    /// Path of optional SSL certificate to allow
    #[structopt(long)]
    cert: Option<std::path::PathBuf>,
}

async fn fetch<UriStr>(
    client: &HttpClient,
    uri: UriStr,
    connection_use: rhymuweb_client::ConnectionUse,
) where
    UriStr: AsRef<str>,
{
    let mut request = Request::new();
    request.target = Uri::parse(uri).unwrap();
    match client.fetch(request, connection_use).await {
        Err(error) => {
            match error.source() {
                Some(source) => eprintln!("error: {} ({})", error, source),
                None => eprintln!("error: {}", error),
            };
        },
        Ok(rhymuweb_client::FetchResults {
            response,
            ..
        }) => {
            println!("Response:");
            println!("{}", "=".repeat(78));
            println!("{} {}", response.status_code, response.reason_phrase);
            for header in &response.headers {
                println!("{}: {}", header.name, header.value);
            }
            println!();
            match rhymuweb::coding::decode_body_as_text(
                &response.headers,
                response.body,
            ) {
                None => println!("(Body cannot be decoded as text)"),
                Some(body) => println!("{}", body),
            };
            println!("{}", "=".repeat(78));
        },
    };
}

fn load_ssl_certificate<T>(cert_path: T) -> anyhow::Result<rustls::ClientConfig>
where
    T: AsRef<std::path::Path>,
{
    let mut cert = Vec::new();
    File::open(cert_path)?.read_to_end(&mut cert)?;
    let mut cert = &cert[..];
    let mut tls_config = rustls::ClientConfig::default();
    tls_config
        .root_store
        .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
    tls_config
        .root_store
        .add_pem_file(&mut cert)
        .map(|_| ())
        .map_err(|()| anyhow!("unable to add SSL certificate to root store"))?;
    Ok(tls_config)
}

async fn main_async() {
    let opts: Opts = Opts::from_args();
    let client = if let Some(cert_path) = opts.cert {
        let tls_config = match load_ssl_certificate(&cert_path) {
            Ok(tls_config) => tls_config,
            Err(error) => {
                eprintln!("Unable to load SSL certificate: {}", error);
                return;
            },
        };
        HttpClient::from_tls_config(Arc::new(tls_config))
    } else {
        HttpClient::default()
    };
    fetch(&client, &opts.uri, rhymuweb_client::ConnectionUse::SingleResource)
        .await;
}

fn main() {
    futures::executor::block_on(async {
        select!(
            () = main_async().fuse() => {},
            () = CtrlC::new().unwrap().fuse() => {
                println!("(Ctrl+C pressed; aborted)");
            },
        )
    });
}

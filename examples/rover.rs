use async_ctrlc::CtrlC;
use futures::select;
use futures::future::FutureExt;
use rhymuri::Uri;
use rhymuweb::Request;
use rhymuweb_client::HttpClient;
use std::error::Error as _;
use structopt::StructOpt;

#[derive(Clone, StructOpt)]
struct Opts {
    /// URI of resource to request
    #[structopt(default_value="http://buddy.local:8080/")]
    uri: String,
}

async fn fetch<UriStr>(
    client: &HttpClient,
    uri: UriStr,
)
    where UriStr: AsRef<str>
{
    let mut request = Request::new();
    request.target = Uri::parse(uri).unwrap();
    match client.fetch(request, rhymuweb_client::ConnectionUse::SingleResource).await {
        Err(error) => {
            match error.source() {
                Some(source) => eprintln!("error: {} ({})", error, source),
                None => eprintln!("error: {}", error),
            };
        },
        Ok(rhymuweb_client::FetchResults{response, ..}) => {
            println!("Response:");
            println!("{}", "=".repeat(78));
            println!("{} {}", response.status_code, response.reason_phrase);
            for header in &response.headers {
                println!("{}: {}", header.name, header.value);
            }
            println!();
            match rhymuweb::coding::decode_body_as_text(
                &response.headers,
                response.body
            ) {
                None => println!("(Body cannot be decoded as text)"),
                Some(body) => println!("{}", body),
            };
            println!("{}", "=".repeat(78));
        },
    };
}

async fn main_async() {
    let opts: Opts = Opts::from_args();
    let client = HttpClient::new();
    fetch(&client, &opts.uri).await;
}

fn main() {
    futures::executor::block_on(async {
        select!(
            () = main_async().fuse() => (),
            () = CtrlC::new().unwrap().fuse() => {
                println!("(Ctrl+C pressed; aborted)");
            },
        )
    });
}

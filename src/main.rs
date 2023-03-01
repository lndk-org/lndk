use futures::executor::block_on;
use tonic_lnd::{Client, ConnectError};

#[tokio::main]
async fn main() {
    let mut client = get_lnd_client().expect("failed to connect");

    let info = client
        .lightning()
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await
        .expect("failed to get info");

    // We only print it here, note that in real-life code you may want to call `.into_inner()` on
    // the response to get the message.
    println!("{:#?}", info);
}

fn get_lnd_client() -> Result<Client, ConnectError> {
    let mut args = std::env::args_os();
    args.next().expect("not even zeroth arg given");
    let address = args
        .next()
        .expect("missing arguments: address, cert file, macaroon file");
    let cert_file = args
        .next()
        .expect("missing arguments: cert file, macaroon file");
    let macaroon_file = args.next().expect("missing argument: macaroon file");
    let address = address.into_string().expect("address is not UTF-8");

    // Connecting to LND requires only address, cert file, and macaroon file
    block_on(tonic_lnd::connect(address, cert_file, macaroon_file))
}

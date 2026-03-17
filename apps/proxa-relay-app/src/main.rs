use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        short,
        long,
        help = "Port to host the relay server on",
        default_value_t = 39201
    )]
    port: u16,

    #[arg(long, help = "Path to the SSL certificate (PEM format)")]
    cert: Option<std::path::PathBuf>,

    #[arg(long, help = "Path to the SSL private key (PEM format)")]
    key: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    env_logger::init();

    proxa_relay_server::start_relay_server(proxa_relay_server::RelayConfig {
        port: args.port,
        cert_path: args.cert,
        key_path: args.key,
    })
    .await
}

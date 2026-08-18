use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "35469".to_owned())
        .parse()?;
    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener = TcpListener::bind(address).await?;
    info!(%address, "Flint AgentCore emulator listening");
    let app = flint::app().await?;
    axum::serve(listener, app).await?;
    Ok(())
}

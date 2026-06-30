//! mc-module entrypoint: boot on `subc-client-rs`'s `serve` (provider role).
//!
//! `serve` owns the handshake (read `--subc <connection-file>`, authenticate, send
//! HELLO{manifest}, await HELLO_ACK, then dispatch route data requests to the
//! handler). The handler opens the single-writer store in `on_hello_ack`.

#![forbid(unsafe_code)]

use std::error::Error;

use mc_module::{manifest, McHandler, DEFAULT_MODULE_ID};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());

    subc_client_rs::serve(manifest(&module_id), McHandler::new()).await?;
    Ok(())
}

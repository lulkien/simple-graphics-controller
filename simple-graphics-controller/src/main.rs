use std::sync::Arc;

use dashmap::DashMap;
use tokio::net::UnixListener;

use crate::{
    resource_manager::query_resource,
    types::{ClientRegistry, OwnerRegistry, ResourceRegistry},
};

mod client_handler;
mod resource_manager;
mod types;

#[tokio::main]
async fn main() {
    let _client_reg: ClientRegistry = Arc::new(DashMap::new());

    let (resource_reg, owner_reg) = query_resource();

    let listener = UnixListener::bind("/tmp/sgc.sock").expect("unixsock bind failed");

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                todo!()
            }
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }
}

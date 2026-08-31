//! Binary entry: wire up logging, resources, and the windowing policy, then
//! hand everything to the [`server`] accept loop.

use std::{collections::HashMap, sync::Arc};

use crate::{
    resource_manager::query_resource,
    windowing::{Policy, PolicyEngine},
};
use anyhow::Context;
use simple_graphics_protocol::Resource;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod client_handler;
mod error;
mod resource_manager;
mod server;
mod types;
mod windowing;

#[cfg(test)]
mod integration_tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // init logger
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting simple-graphics-controller");
    let (resource_reg, advertised) = query_resource();
    let advertised = Arc::new(advertised);
    debug!("Advertised resources (priority order): {advertised:?}");

    // Window policy: SGC_POLICY env, default fair-queue. One policy for all
    // registered resources (per-resource override map is a future knob).
    let policy: Policy = std::env::var("SGC_POLICY")
        .ok()
        .map(|value| value.parse())
        .transpose()
        .context("invalid SGC_POLICY (expected first-owner | latest-owner | fair-queue)")?
        .unwrap_or(Policy::FairQueue);
    info!("Windowing policy: {policy:?}");

    let policies: HashMap<Resource, Policy> = resource_reg
        .iter()
        .map(|entry| (entry.key().clone(), policy))
        .collect();
    let engine = PolicyEngine::spawn(policies);

    server::run(engine, resource_reg, advertised).await
}

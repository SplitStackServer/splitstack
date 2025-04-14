pub mod backend;
pub mod basestation_backend;
mod mqtt;

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;

use crate::config;

pub async fn setup() -> Result<()> {
    let conf = config::get();

    info!("Setting up gateway backends for the different regions");
    for region in &conf.regions {
        if !conf.network.enabled_regions.contains(&region.id) {
            continue;
        }

        info!(
            region_id = %region.id,
            region_common_name = %region.common_name,
            "Setting up gateway backend for region"
        );

        let backend =
            mqtt::MqttBackend::new(&region.id, region.common_name, &region.gateway.backend.mqtt)
                .await
                .context("New MQTT gateway backend error")?;

        backend::set_backend(&region.id, Box::new(backend)).await;

        if conf.basestation.enable {
            info!(
                region_id = %region.id,
                region_common_name = %region.common_name,
                "Setting up basestation backend for region"
            );
            let backend = mqtt::MqttBackend::new_basestation(
                &region.id,
                region.common_name,
                &region.basestation.backend.mqtt,
            )
            .await
            .context("New MQTT gateway backend error")?;

            basestation_backend::set_backend(&region.id, Box::new(backend)).await;
        }
    }

    Ok(())
}

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;

use chirpstack_api::bs;

use super::BasestationBackend;

lazy_static! {
    static ref COMMANDS: RwLock<Vec<bs::ProtoCommand>> = RwLock::new(Vec::new());
    static ref RESPONSES: RwLock<Vec<bs::ProtoResponse>> = RwLock::new(Vec::new());
}

pub async fn reset() {
    COMMANDS.write().await.drain(..);
    RESPONSES.write().await.drain(..);
}

pub struct Backend {}

#[async_trait]
impl BasestationBackend for Backend {
    async fn send_command(&self, cmd: &bs::ProtoCommand) -> Result<()> {
        COMMANDS.write().await.push(cmd.clone());
        Ok(())
    }

    async fn send_response(&self, rsp: &bs::ProtoResponse) -> Result<()> {
        RESPONSES.write().await.push(rsp.clone());
        Ok(())
    }
}

pub async fn get_commands() -> Vec<bs::ProtoCommand> {
    COMMANDS.write().await.drain(..).collect()
}

pub async fn get_responses() -> Vec<bs::ProtoResponse> {
    RESPONSES.write().await.drain(..).collect()
}

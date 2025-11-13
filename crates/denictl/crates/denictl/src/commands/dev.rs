use anyhow::Result;

pub async fn execute(port: u16, watch: bool, debug: bool) -> Result<()> {
    crate::dev::run_dev_server(port, watch, debug).await
}

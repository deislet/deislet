//! `deis dev` - the command-line shell around [`crate::dev`].

use std::path::PathBuf;

use anyhow::Result;

pub struct Args {
    pub port: u16,
    pub runtime_port: Option<u16>,
    pub runtime: Option<PathBuf>,
    pub environment: String,
    pub watch: bool,
    pub debug: bool,
    pub minify: bool,
    pub store: Option<String>,
    pub queue: Option<String>,
    pub secrets: Vec<String>,
}

pub async fn execute(args: Args) -> Result<()> {
    crate::dev::run(crate::dev::Options {
        port: args.port,
        runtime_port: args.runtime_port,
        runtime: args.runtime,
        environment: args.environment,
        watch: args.watch,
        debug: args.debug,
        minify: args.minify,
        store: args.store,
        queue: args.queue,
        secrets: args.secrets,
    })
    .await
}

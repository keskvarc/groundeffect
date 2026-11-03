mod compiler;
mod dds;
mod dsf_compiler;
#[cfg(test)]
mod fixtures;
mod geo;
mod lighting;
mod model;
mod raster;
mod scenery;
mod service;
mod streaming;

use anyhow::{Context, Result, bail};
use std::path::Path;

fn run() -> Result<()> {
    gdal::config::set_config_option("GDAL_CACHEMAX", "64")?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, flag, path] if command == "serve" && flag == "--data-dir" => {
            service::serve(Path::new(path))
        }
        [command, flag, path] if command == "plan" && flag == "--config" => {
            let config = serde_json::from_slice(&raster::bounded_read(Path::new(path), 65536)?)
                .context("reading preparation configuration")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&compiler::inspect(&config)?)?
            );
            Ok(())
        }
        _ => bail!("usage: groundeffect-engine serve --data-dir PATH | plan --config FILE"),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("groundeffect-engine: {error:#}");
        std::process::exit(1);
    }
}

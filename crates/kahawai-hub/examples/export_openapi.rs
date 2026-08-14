use std::io::BufWriter;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: export_openapi <output.json>")?;
    let output = BufWriter::new(std::fs::File::create(path)?);
    serde_json::to_writer_pretty(output, &kahawai_hub::api::openapi_document())?;
    Ok(())
}

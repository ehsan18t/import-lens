use flate2::{Compression, write::GzEncoder};
use rayon::join;
use std::{error::Error, io::Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionSizes {
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
}

pub fn compress_all(source: &str) -> Result<CompressionSizes, Box<dyn Error + Send + Sync>> {
    compress_all_bytes(source.as_bytes())
}

/// The same three compressors over raw bytes, for an artifact that is not text: a wasm module or a
/// font, whose shipped size is simply its bytes (B2). A woff2 is already brotli-internally, so it
/// barely shrinks again here — which is correct, and exactly what it costs on the wire.
pub fn compress_all_bytes(bytes: &[u8]) -> Result<CompressionSizes, Box<dyn Error + Send + Sync>> {
    let (gzip, (brotli, zstd)) = join(
        || gzip_compress(bytes),
        || join(|| brotli_compress(bytes), || zstd_compress(bytes)),
    );

    Ok(CompressionSizes {
        gzip_bytes: gzip? as u64,
        brotli_bytes: brotli? as u64,
        zstd_bytes: zstd? as u64,
    })
}

fn gzip_compress(bytes: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?.len())
}

fn brotli_compress(bytes: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
    let mut output = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 4, 22);
        writer.write_all(bytes)?;
    }
    Ok(output.len())
}

fn zstd_compress(bytes: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
    Ok(zstd::stream::encode_all(bytes, 3)?.len())
}

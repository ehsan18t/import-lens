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
    let bytes = source.as_bytes();
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

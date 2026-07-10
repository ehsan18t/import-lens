use import_lens_daemon::pipeline::compress::compress_all;

#[test]
fn compress_all_returns_all_supported_formats() {
    let sizes =
        compress_all("export const value = 'Import Lens';").expect("compression should succeed");

    assert!(sizes.gzip_bytes > 0);
    assert!(sizes.brotli_bytes > 0);
    assert!(sizes.zstd_bytes > 0);
}

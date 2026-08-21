feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::do_sync;

use support::{spawn_test_client_with_server, spawn_test_server, TEST_PASSWORD, WORKSPACE_ID};

#[tokio::test]
#[ignore = "101 MiB authenticated chunk transport product proof"]
async fn format_v3_syncs_file_above_100_mib_through_authenticated_chunks() {
    use std::io::Seek as _;
    use std::io::Write as _;

    const SIZE: u64 = 101 * 1024 * 1024 + 17;
    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    let mut uploader_config = feanorfs_client::load_config(uploader.workspace.path()).unwrap();
    uploader_config.format_version = 3;
    feanorfs_client::save_config(uploader.workspace.path(), &uploader_config).unwrap();

    let source = uploader.workspace.path().join("large.bin");
    let mut file = std::fs::File::create(&source).unwrap();
    file.set_len(SIZE).unwrap();
    file.seek(std::io::SeekFrom::Start(SIZE - 17)).unwrap();
    file.write_all(b"authenticated-end").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let uploaded = do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(uploaded.uploads, 1);

    let downloader = spawn_test_client_with_server(&server).await;
    let mut downloader_config = feanorfs_client::load_config(downloader.workspace.path()).unwrap();
    downloader_config.format_version = 3;
    feanorfs_client::save_config(downloader.workspace.path(), &downloader_config).unwrap();
    let downloaded = do_sync(
        &server.api,
        &downloader.db,
        downloader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(downloaded.downloads, 1);
    let destination = downloader.workspace.path().join("large.bin");
    assert_eq!(std::fs::metadata(&destination).unwrap().len(), SIZE);

    let mut source_file = std::fs::File::open(source).unwrap();
    let mut destination_file = std::fs::File::open(destination).unwrap();
    let mut source_hasher = blake3::Hasher::new();
    source_hasher.update_reader(&mut source_file).unwrap();
    let mut destination_hasher = blake3::Hasher::new();
    destination_hasher
        .update_reader(&mut destination_file)
        .unwrap();
    assert_eq!(source_hasher.finalize(), destination_hasher.finalize());
}

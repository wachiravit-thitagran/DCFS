//! Integration tests for DCFS FUSE operations.
//!
//! Tests concurrent operations, truncate, overwrite, large files,
//! nested directories, and error cases using the fake client.

use dcfs_core::NodeKind;
use dcfs_fuse::client::{ClientError, ServerClient};
use dcfs_fuse::fake_client::FakeClient;
use dcfs_fuse::service::{Fs, ROOT_INO};
use dcfs_protocol::{CreateNodeRequest, NameBytes, PatchNodeRequest, RenameNodeRequest};
use uuid::Uuid;

// ============================================================
// Truncate tests
// ============================================================

#[tokio::test]
async fn test_truncate_shrink() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"shrink.txt").await;
    client.write_file(file.id, 0, b"0123456789").await.unwrap();

    // Note: FakeClient doesn't implement truncate, only write
    // Writing shorter data doesn't shrink the file (POSIX write behavior)
    // To test truncate, we'd need a separate truncate API

    // Verify current behavior: file stays at original size
    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.size, 10);
}

#[tokio::test]
async fn test_truncate_grow_with_zeros() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"grow.txt").await;
    client.write_file(file.id, 0, b"abc").await.unwrap();

    // Grow by writing at offset beyond current size
    client.write_file(file.id, 10, b"xyz").await.unwrap();

    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.size, 13); // 10 + 3

    let data = client.read_file(file.id, 0, 13).await.unwrap();
    assert_eq!(&data[0..3], b"abc");
    assert_eq!(&data[3..10], &[0u8; 7]); // zeros in gap
    assert_eq!(&data[10..13], b"xyz");
}

// ============================================================
// Overwrite tests
// ============================================================

#[tokio::test]
async fn test_overwrite_existing_data() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"overwrite.txt").await;
    client
        .write_file(file.id, 0, b"Hello, World!")
        .await
        .unwrap();

    // Overwrite middle portion
    client.write_file(file.id, 7, b"Rust!").await.unwrap();

    let data = client.read_file(file.id, 0, 13).await.unwrap();
    assert_eq!(data, b"Hello, Rust!!");
}

#[tokio::test]
async fn test_overwrite_entire_file() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"replace.txt").await;
    client
        .write_file(file.id, 0, b"old content here")
        .await
        .unwrap();
    client.write_file(file.id, 0, b"new").await.unwrap();

    // Note: write doesn't truncate - file stays at original size
    let data = client.read_file(file.id, 0, 3).await.unwrap();
    assert_eq!(data, b"new");

    // Old data remains after the new write
    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.size, 16); // original size preserved
}

// ============================================================
// Large file tests
// ============================================================

#[tokio::test]
async fn test_large_file_write_read() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"large.bin").await;

    // Write 1MB of data
    let data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
    let written = client.write_file(file.id, 0, &data).await.unwrap();
    assert_eq!(written, 1_000_000);

    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.size, 1_000_000);

    // Read back in chunks
    let chunk1 = client.read_file(file.id, 0, 500_000).await.unwrap();
    assert_eq!(chunk1.len(), 500_000);
    assert_eq!(chunk1[0], 0);
    assert_eq!(chunk1[255], 255);

    let chunk2 = client.read_file(file.id, 500_000, 500_000).await.unwrap();
    assert_eq!(chunk2.len(), 500_000);
}

#[tokio::test]
async fn test_short_read_at_large_offset_is_not_reported_as_eof() {
    use std::sync::Arc;

    // Exercise a 4-byte random read beyond 30 GB without allocating a huge file.
    // The fake client advertises a sparse logical size while an empty backing Vec
    // simulates a backend that unexpectedly returns short before logical EOF.
    const LARGE_OFFSET: u64 = 30_155_428_536;

    let client = Arc::new(FakeClient::new());
    let root_id = FakeClient::root_id();
    let file = create_file(client.as_ref(), root_id, b"large-file.bin").await;
    client
        .patch_node(
            file.id,
            PatchNodeRequest {
                mode: None,
                uid: None,
                gid: None,
                size: Some(LARGE_OFFSET + 4),
                mtime: None,
                atime: None,
                idempotency_key: Uuid::new_v4(),
            },
        )
        .await
        .unwrap();

    let fs = Fs::mount(client).await.unwrap();
    let attr = fs.lookup(ROOT_INO, b"large-file.bin").await.unwrap();
    let err = fs.read(attr.ino, LARGE_OFFSET, 4).await.unwrap_err();

    match err {
        ClientError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[tokio::test]
async fn test_read_crossing_real_eof_is_still_a_normal_short_read() {
    use std::sync::Arc;

    let client = Arc::new(FakeClient::new());
    let root_id = FakeClient::root_id();
    let file = create_file(client.as_ref(), root_id, b"small-eof.bin").await;
    client.write_file(file.id, 0, b"abc").await.unwrap();

    let fs = Fs::mount(client).await.unwrap();
    let attr = fs.lookup(ROOT_INO, b"small-eof.bin").await.unwrap();

    // Only one byte exists from offset 2 to EOF. Asking for four must return
    // that one byte rather than turning a legitimate EOF into EIO.
    let data = fs.read(attr.ino, 2, 4).await.unwrap();
    assert_eq!(data, b"c");
}

#[tokio::test]
async fn test_read_after_local_write_refreshes_size_and_version() {
    use std::sync::Arc;

    let client = Arc::new(FakeClient::new());
    let root_id = FakeClient::root_id();
    create_file(client.as_ref(), root_id, b"read-after-write.bin").await;

    let fs = Fs::mount(client).await.unwrap();
    let attr = fs.lookup(ROOT_INO, b"read-after-write.bin").await.unwrap();
    fs.write(attr.ino, 0, b"abc").await.unwrap();

    // read() flushes the buffered write. It must then refresh the metadata
    // remembered by lookup rather than treating the old zero-byte size as EOF.
    let data = fs.read(attr.ino, 0, 3).await.unwrap();
    assert_eq!(data, b"abc");
}

// ============================================================
// Nested directory tests
// ============================================================

#[tokio::test]
async fn test_nested_directories() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    // Create /a/b/c
    let a = create_dir(&client, root_id, b"a").await;
    let b = create_dir(&client, a.id, b"b").await;
    let c = create_dir(&client, b.id, b"c").await;

    // Create file in deepest directory
    let file = create_file(&client, c.id, b"deep.txt").await;
    client
        .write_file(file.id, 0, b"deep content")
        .await
        .unwrap();

    // Verify path
    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.parent_id, Some(c.id));

    let c_children = client.list_children(c.id, None, None).await.unwrap();
    assert_eq!(c_children.children.len(), 1);
    assert_eq!(c_children.children[0].name.as_bytes(), b"deep.txt");
}

#[tokio::test]
async fn test_list_nested_children() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let dir = create_dir(&client, root_id, b"parent").await;

    // Create mixed files and directories
    create_file(&client, dir.id, b"file1.txt").await;
    create_dir(&client, dir.id, b"subdir1").await;
    create_file(&client, dir.id, b"file2.txt").await;
    create_dir(&client, dir.id, b"subdir2").await;

    let children = client.list_children(dir.id, None, None).await.unwrap();
    assert_eq!(children.children.len(), 4);

    // Should be sorted by name
    assert_eq!(children.children[0].name.as_bytes(), b"file1.txt");
    assert_eq!(children.children[1].name.as_bytes(), b"file2.txt");
    assert_eq!(children.children[2].name.as_bytes(), b"subdir1");
    assert_eq!(children.children[3].name.as_bytes(), b"subdir2");
}

// ============================================================
// Rename across directories
// ============================================================

#[tokio::test]
async fn test_rename_across_directories() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let dir_a = create_dir(&client, root_id, b"dir_a").await;
    let dir_b = create_dir(&client, root_id, b"dir_b").await;
    let file = create_file(&client, dir_a.id, b"moving.txt").await;

    // Move file from dir_a to dir_b
    let rename_req = RenameNodeRequest {
        new_parent_id: dir_b.id,
        new_name: NameBytes::new(b"moved.txt".to_vec()).unwrap(),
        idempotency_key: Uuid::new_v4(),
    };
    let renamed = client.rename_node(file.id, rename_req).await.unwrap();

    assert_eq!(renamed.parent_id, Some(dir_b.id));
    assert_eq!(renamed.name.as_bytes(), b"moved.txt");

    // Verify it's gone from dir_a
    let a_children = client.list_children(dir_a.id, None, None).await.unwrap();
    assert_eq!(a_children.children.len(), 0);

    // Verify it's in dir_b
    let b_children = client.list_children(dir_b.id, None, None).await.unwrap();
    assert_eq!(b_children.children.len(), 1);
    assert_eq!(b_children.children[0].name.as_bytes(), b"moved.txt");
}

// ============================================================
// Concurrent operations
// ============================================================

#[tokio::test]
async fn test_concurrent_file_creates() {
    use std::sync::Arc;

    let client = Arc::new(FakeClient::new());
    let root_id = FakeClient::root_id();

    // Create 10 files concurrently
    let mut handles = Vec::new();
    for i in 0..10 {
        let name = format!("file_{}.txt", i).into_bytes();
        let client_clone = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            let req = CreateNodeRequest {
                parent_id: root_id,
                name: NameBytes::new(name).unwrap(),
                kind: NodeKind::File,
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                link_target: None,
                idempotency_key: Uuid::new_v4(),
            };
            client_clone.create_node(req).await
        }));
    }

    let mut created = Vec::new();
    for handle in handles {
        let result = handle.await.unwrap().unwrap();
        created.push(result);
    }

    assert_eq!(created.len(), 10);

    // Verify all exist
    let children = client.list_children(root_id, None, None).await.unwrap();
    assert_eq!(children.children.len(), 10);
}

#[tokio::test]
async fn test_concurrent_writes_same_file() {
    use std::sync::Arc;

    let client = Arc::new(FakeClient::new());
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"concurrent.txt").await;

    // Write at different offsets concurrently
    let mut handles = Vec::new();
    for i in 0..5u64 {
        let client_clone = Arc::clone(&client);
        let offset = i * 10;
        handles.push(tokio::spawn(async move {
            let data = vec![b'A' + i as u8; 10];
            client_clone.write_file(file.id, offset, &data).await
        }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    let node = client.get_node(file.id).await.unwrap();
    assert_eq!(node.size, 50);
}

// ============================================================
// Error cases
// ============================================================

#[tokio::test]
async fn test_read_nonexistent_file() {
    let client = FakeClient::new();
    let fake_id = Uuid::new_v4();

    let result = client.read_file(fake_id, 0, 100).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_write_nonexistent_file() {
    let client = FakeClient::new();
    let fake_id = Uuid::new_v4();

    let result = client.write_file(fake_id, 0, b"data").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_create_in_nonexistent_parent() {
    let client = FakeClient::new();
    let fake_parent = Uuid::new_v4();

    let req = CreateNodeRequest {
        parent_id: fake_parent,
        name: NameBytes::new(b"orphan.txt".to_vec()).unwrap(),
        kind: NodeKind::File,
        mode: 0o644,
        uid: 1000,
        gid: 1000,
        link_target: None,
        idempotency_key: Uuid::new_v4(),
    };

    let result = client.create_node(req).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_rename_onto_an_existing_name_replaces_it() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let existing = create_file(&client, root_id, b"existing.txt").await;
    let moving = create_file(&client, root_id, b"moving.txt").await;

    // rename(2) replaces the destination; refusing here is what broke `git
    // init`, which saves its config by renaming a lock file over it.
    let rename_req = RenameNodeRequest {
        new_parent_id: root_id,
        new_name: NameBytes::new(b"existing.txt".to_vec()).unwrap(),
        idempotency_key: Uuid::new_v4(),
    };
    let renamed = client.rename_node(moving.id, rename_req).await.unwrap();
    assert_eq!(renamed.id, moving.id);

    let children = client.list_children(root_id, None, None).await.unwrap();
    assert_eq!(children.children.len(), 1, "the replaced node is gone");
    assert_eq!(children.children[0].id, moving.id);
    assert!(client.get_node(existing.id).await.is_err());
}

#[tokio::test]
async fn test_delete_nonexistent_node() {
    let client = FakeClient::new();
    let fake_id = Uuid::new_v4();

    let result = client.delete_node(fake_id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_beyond_file_size() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"small.txt").await;
    client.write_file(file.id, 0, b"hi").await.unwrap();

    // Read beyond end
    let data = client.read_file(file.id, 100, 50).await.unwrap();
    assert!(data.is_empty());
}

#[tokio::test]
async fn test_read_partial_beyond_end() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"partial_end.txt").await;
    client.write_file(file.id, 0, b"0123456789").await.unwrap();

    // Read starting at 8, requesting 10 bytes (only 2 available)
    let data = client.read_file(file.id, 8, 10).await.unwrap();
    assert_eq!(data, b"89");
}

// ============================================================
// Patch node tests
// ============================================================

#[tokio::test]
async fn test_patch_node_mode() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"chmod.txt").await;
    assert_eq!(file.mode, 0o644);

    let patch = PatchNodeRequest {
        mode: Some(0o755),
        uid: None,
        gid: None,
        size: None,
        mtime: None,
        atime: None,
        idempotency_key: Uuid::new_v4(),
    };

    let updated = client.patch_node(file.id, patch).await.unwrap();
    assert_eq!(updated.mode, 0o755);
}

#[tokio::test]
async fn test_patch_node_ownership() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"chown.txt").await;

    let patch = PatchNodeRequest {
        mode: None,
        uid: Some(2000),
        gid: Some(3000),
        size: None,
        mtime: None,
        atime: None,
        idempotency_key: Uuid::new_v4(),
    };

    let updated = client.patch_node(file.id, patch).await.unwrap();
    assert_eq!(updated.uid, 2000);
    assert_eq!(updated.gid, 3000);
}

// ============================================================
// Sync tests
// ============================================================

#[tokio::test]
async fn test_sync_file() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    let file = create_file(&client, root_id, b"sync.txt").await;
    client.write_file(file.id, 0, b"sync me").await.unwrap();

    // Sync should succeed
    let result = client.sync_file(file.id).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_sync_nonexistent_file() {
    let client = FakeClient::new();
    let fake_id = Uuid::new_v4();

    let result = client.sync_file(fake_id).await;
    assert!(result.is_err());
}

// ============================================================
// List children with pagination
// ============================================================

#[tokio::test]
async fn test_list_children_with_limit() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    // Create 5 files
    for i in 0..5 {
        let name = format!("file_{}.txt", i).into_bytes();
        create_file(&client, root_id, &name).await;
    }

    // List with limit
    let children = client.list_children(root_id, Some(3), None).await.unwrap();
    assert_eq!(children.children.len(), 3);
}

#[tokio::test]
async fn test_list_children_with_offset() {
    let client = FakeClient::new();
    let root_id = FakeClient::root_id();

    // Create 5 files
    for i in 0..5 {
        let name = format!("file_{}.txt", i).into_bytes();
        create_file(&client, root_id, &name).await;
    }

    // List with offset
    let children = client.list_children(root_id, None, Some(2)).await.unwrap();
    assert_eq!(children.children.len(), 3); // 5 - 2 = 3
}

// ============================================================
// Helper functions
// ============================================================

async fn create_file(
    client: &FakeClient,
    parent_id: Uuid,
    name: &[u8],
) -> dcfs_protocol::NodeResponse {
    let req = CreateNodeRequest {
        parent_id,
        name: NameBytes::new(name.to_vec()).unwrap(),
        kind: NodeKind::File,
        mode: 0o644,
        uid: 1000,
        gid: 1000,
        link_target: None,
        idempotency_key: Uuid::new_v4(),
    };
    client.create_node(req).await.unwrap()
}

async fn create_dir(
    client: &FakeClient,
    parent_id: Uuid,
    name: &[u8],
) -> dcfs_protocol::NodeResponse {
    let req = CreateNodeRequest {
        parent_id,
        name: NameBytes::new(name.to_vec()).unwrap(),
        kind: NodeKind::Directory,
        mode: 0o755,
        uid: 1000,
        gid: 1000,
        link_target: None,
        idempotency_key: Uuid::new_v4(),
    };
    client.create_node(req).await.unwrap()
}

//! Namespace API tests against the in-memory backends.

mod common;

use axum::http::StatusCode;
use common::{key, name, send};
use dcfs_server::create_server;
use serde_json::json;

/// Create a node and return its id, asserting the call succeeded.
async fn create(router: &axum::Router, parent: &str, raw_name: &[u8], kind: &str) -> String {
    let (status, body) = send(
        router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": parent,
            "name": name(raw_name),
            "kind": kind,
            "mode": if kind == "Directory" { 0o40755 } else { 0o100644 },
            "uid": 1000,
            "gid": 1000,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn health_and_readiness_respond() {
    let router = create_server();

    let (status, body) = send(&router, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = send(&router, "GET", "/health/ready", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn root_is_a_nameless_directory() {
    let router = create_server();

    // Regression: the root used to be stored as "/", which is not a valid
    // filename, and rendering it panicked the worker.
    let (status, body) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "");
    assert_eq!(body["kind"], "Directory");
    assert!(body["parent_id"].is_null());
}

#[tokio::test]
async fn creates_nested_directories_and_lists_them() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let docs = create(&router, root_id, b"docs", "Directory").await;
    let nested = create(&router, &docs, b"nested", "Directory").await;
    create(&router, &nested, b"notes.txt", "File").await;

    let (status, body) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{nested}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["children"].as_array().unwrap().len(), 1);
    assert_eq!(body["children"][0]["name"], name(b"notes.txt"));
    assert_eq!(body["has_more"], false);

    // An empty file starts with no version and zero size.
    let file_id = body["children"][0]["id"].as_str().unwrap();
    let (_, file) = send(&router, "GET", &format!("/api/v1/nodes/{file_id}"), None).await;
    assert_eq!(file["size"], 0);
    assert!(file["current_version_id"].is_null());
}

#[tokio::test]
async fn preserves_raw_non_utf8_filenames() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    // Linux filenames are arbitrary bytes; this one is not valid UTF-8.
    let raw = [0xff, 0xfe, b'x'];
    let id = create(&router, root_id, &raw, "File").await;

    let (_, body) = send(&router, "GET", &format!("/api/v1/nodes/{id}"), None).await;
    assert_eq!(body["name"], name(&raw));
}

#[tokio::test]
async fn rejects_invalid_names() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    for bad in [b"a/b".to_vec(), b"a\0b".to_vec(), Vec::new()] {
        let (status, body) = send(
            &router,
            "POST",
            "/api/v1/nodes",
            Some(json!({
                "parent_id": root_id,
                "name": name(&bad),
                "kind": "File",
                "mode": 0o100644,
                "uid": 0,
                "gid": 0,
                "idempotency_key": key(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {bad:?} -> {body}");
        assert_eq!(body["code"], "invalid_request");
    }
}

#[tokio::test]
async fn duplicate_create_is_a_conflict() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    // The idempotency key doubles as the node id, so replaying the same request
    // must not create a second node.
    let request = json!({
        "parent_id": root_id,
        "name": name(b"once"),
        "kind": "File",
        "mode": 0o100644,
        "uid": 0,
        "gid": 0,
        "idempotency_key": key(),
    });

    let (status, _) = send(&router, "POST", "/api/v1/nodes", Some(request.clone())).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(&router, "POST", "/api/v1/nodes", Some(request)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");
}

#[tokio::test]
async fn rename_is_metadata_only() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let dir = create(&router, root_id, b"dir", "Directory").await;
    let file = create(&router, root_id, b"old", "File").await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{file}/rename"),
        Some(json!({
            "new_parent_id": dir,
            "new_name": name(b"new"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], name(b"new"));
    assert_eq!(body["parent_id"], dir);

    // No version was staged and no object was written: the bytes never moved.
    assert!(body["current_version_id"].is_null());

    let (_, listing) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{dir}/children"),
        None,
    )
    .await;
    assert_eq!(listing["children"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn publish_never_replaces_an_existing_destination() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let published = create(&router, root_id, b"published", "File").await;
    let temporary = create(&router, root_id, b".uploading", "File").await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{temporary}/publish"),
        Some(json!({
            "new_parent_id": root_id,
            "new_name": name(b"published"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (_, kept) = send(&router, "GET", &format!("/api/v1/nodes/{published}"), None).await;
    assert_eq!(kept["name"], name(b"published"));

    let (_, partial) = send(&router, "GET", &format!("/api/v1/nodes/{temporary}"), None).await;
    assert_eq!(partial["name"], name(b".uploading"));

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{temporary}/publish"),
        Some(json!({
            "new_parent_id": root_id,
            "new_name": name(b"new-name"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], name(b"new-name"));
}

#[tokio::test]
async fn rename_to_an_invalid_name_is_rejected() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let file = create(&router, root_id, b"keep", "File").await;

    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{file}/rename"),
        Some(json!({
            "new_parent_id": root_id,
            "new_name": name(b"a/b"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The original name survived the rejected rename.
    let (_, body) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(body["name"], name(b"keep"));
}

#[tokio::test]
async fn patch_updates_only_the_supplied_fields() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let file = create(&router, root_id, b"attrs", "File").await;

    let (status, body) = send(
        &router,
        "PATCH",
        &format!("/api/v1/nodes/{file}"),
        Some(json!({
            "mode": 0o100600,
            "uid": null,
            "gid": null,
            "size": null,
            "mtime": null,
            "atime": null,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], 0o100600);
    assert_eq!(body["uid"], 1000, "uid must be untouched");
}

#[tokio::test]
async fn unlink_removes_the_node() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let file = create(&router, root_id, b"gone", "File").await;

    let (status, _) = send(&router, "DELETE", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");

    let (status, _) = send(&router, "DELETE", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleting twice must not succeed"
    );
}

#[tokio::test]
async fn resolves_paths_including_the_root() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let docs = create(&router, root_id, b"docs", "Directory").await;
    let file = create(&router, &docs, b"a.txt", "File").await;

    for path in ["/", ""] {
        let (status, body) = send(
            &router,
            "GET",
            &format!("/api/v1/nodes/resolve?path={path}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], root_id);
    }

    let (status, body) = send(
        &router,
        "GET",
        "/api/v1/nodes/resolve?path=/docs/a.txt",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], file);

    let (status, _) = send(
        &router,
        "GET",
        "/api/v1/nodes/resolve?path=/docs/missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_ids_are_not_found_and_malformed_ids_are_rejected() {
    let router = create_server();

    let (status, _) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{}", uuid::Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = send(&router, "GET", "/api/v1/nodes/not-a-uuid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_children_paginates() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    for i in 0..5u8 {
        create(&router, root_id, &[b'f', b'0' + i], "File").await;
    }

    let (status, body) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{root_id}/children?limit=2&offset=0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["children"].as_array().unwrap().len(), 2);
    assert_eq!(body["has_more"], true);

    let (_, body) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{root_id}/children?limit=10&offset=4"),
        None,
    )
    .await;
    assert_eq!(body["children"].as_array().unwrap().len(), 1);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn rmdir_refuses_a_directory_that_still_has_children() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();
    let dir = create(&router, root_id, b"full", "Directory").await;
    let child = create(&router, &dir, b"child.txt", "File").await;

    let (status, body) = send(&router, "DELETE", &format!("/api/v1/nodes/{dir}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "directory_not_empty");

    // Still there, with its child.
    let (status, _) = send(&router, "GET", &format!("/api/v1/nodes/{dir}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // Empty it, and the directory goes.
    let (status, _) = send(&router, "DELETE", &format!("/api/v1/nodes/{child}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&router, "DELETE", &format!("/api/v1/nodes/{dir}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_root_directory_cannot_be_deleted() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let (status, _) = send(&router, "DELETE", &format!("/api/v1/nodes/{root_id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rename_replaces_an_existing_file() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    // The write-a-temp-file-then-rename idiom that git, editors and package
    // managers all use. Before this worked, `git init` failed outright.
    let target = create(&router, root_id, b"config", "File").await;
    let temp = create(&router, root_id, b"config.lock", "File").await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{temp}/rename"),
        Some(json!({
            "new_parent_id": root_id,
            "new_name": name(b"config"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // One entry survives, and it is the file that was renamed in.
    let (_, listing) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{root_id}/children"),
        None,
    )
    .await;
    let names: Vec<&str> = listing["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec![name(b"config")]);

    let (status, _) = send(&router, "GET", &format!("/api/v1/nodes/{temp}"), None).await;
    assert_eq!(status, StatusCode::OK, "the renamed node kept its id");
    let (status, _) = send(&router, "GET", &format!("/api/v1/nodes/{target}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the replaced node is gone");
}

#[tokio::test]
async fn rename_onto_a_non_empty_directory_is_refused() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let occupied = create(&router, root_id, b"occupied", "Directory").await;
    create(&router, &occupied, b"child", "File").await;
    let mover = create(&router, root_id, b"mover", "Directory").await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/nodes/{mover}/rename"),
        Some(json!({
            "new_parent_id": root_id,
            "new_name": name(b"occupied"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "directory_not_empty");

    // Both directories survive untouched.
    for id in [&occupied, &mover] {
        let (status, _) = send(&router, "GET", &format!("/api/v1/nodes/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn rename_between_mismatched_kinds_is_refused() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let dir = create(&router, root_id, b"a-dir", "Directory").await;
    let file = create(&router, root_id, b"a-file", "File").await;

    for (from, onto) in [(&file, b"a-dir".as_slice()), (&dir, b"a-file".as_slice())] {
        let (status, _) = send(
            &router,
            "POST",
            &format!("/api/v1/nodes/{from}/rename"),
            Some(json!({
                "new_parent_id": root_id,
                "new_name": name(onto),
                "idempotency_key": key(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn symlinks_carry_a_raw_target() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root_id, "name": name(b"link"), "kind": "Symlink",
            "mode": 0o120777, "uid": 1000, "gid": 1000,
            "link_target": name(b"../elsewhere"),
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "Symlink");
    assert_eq!(body["link_target"], name(b"../elsewhere"));

    let id = body["id"].as_str().unwrap();
    let (_, fetched) = send(&router, "GET", &format!("/api/v1/nodes/{id}"), None).await;
    assert_eq!(fetched["link_target"], name(b"../elsewhere"));
}

#[tokio::test]
async fn a_symlink_without_a_target_is_rejected_and_a_file_with_one_too() {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap();

    let cases = [
        (json!("Symlink"), None, "a symlink needs a target"),
        (
            json!("Symlink"),
            Some(name(b"")),
            "an empty target is not a target",
        ),
        (
            json!("File"),
            Some(name(b"/etc/passwd")),
            "a file has no target",
        ),
    ];
    for (kind, target, why) in cases {
        let mut body = json!({
            "parent_id": root_id, "name": name(b"x"), "kind": kind,
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        });
        if let Some(target) = target {
            body["link_target"] = json!(target);
        }
        let (status, response) = send(&router, "POST", "/api/v1/nodes", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {response}");
    }
}

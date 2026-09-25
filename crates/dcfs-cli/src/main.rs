//! Operator commands for a DCFS deployment.
//!
//! ```text
//! dcfs config-check          # read the environment and say what would happen
//! dcfs status [--server URL] # ask a running server how it is
//! dcfs put FILE [--resume]   # upload a local file, carrying on if it broke
//! ```

use std::process::ExitCode;

const HELP: &str = "\
Usage: dcfs <command>

Commands:
  config-check   Read the environment the server would read, report what it
                 would do, and exit non-zero if it would refuse to start.
  status         Ask a running server for its health and part size.
                 --server URL   default $DCFS_SERVER or http://127.0.0.1:8080
                 The bearer token comes from $DCFS_TOKEN or $API_TOKEN.
  put FILE       Upload a local file.
                 --name NAME    store it under this name (default: the
                                file's own name)
                 --resume       carry on with a file that is already there
                                instead of refusing
                 --server URL   as above
                 An upload that breaks can be restarted with --resume: the
                 server reports how much it took, and only the rest is sent.

Secrets are read from the environment and never printed.";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("config-check") => config_check(),
        Some("status") => status(args).await,
        Some("put") => put(args).await,
        Some("-h") | Some("--help") | None => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("dcfs: unknown command {other:?}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

/// Report what the server would do with the current environment, without
/// starting it or touching the database.
fn config_check() -> ExitCode {
    let config = match dcfs_server::config::Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            println!("configuration is invalid: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Everything below mirrors the checks the server makes at startup, so a
    // clean run here means it will come up.
    let mut blocking = Vec::new();
    if config.master_key.is_none() {
        blocking.push("MASTER_KEY is not set (openssl rand -hex 32)");
    }
    if config.api_token.is_none() {
        blocking.push("API_TOKEN is not set (openssl rand -hex 32)");
    }
    let discord = config.discord_webhook_id.is_some() && config.discord_webhook_token.is_some();
    if !discord && config.object_store_path.is_none() {
        blocking.push("set OBJECT_STORE_PATH, or the DISCORD_WEBHOOK_* pair");
    }

    println!(
        "metadata:     {}",
        match &config.database_url {
            Some(_) => format!("postgresql, schema {}", config.database_schema),
            None => "in-memory (development only: lost on restart)".to_string(),
        }
    );
    println!("auto-migrate: {}", config.database_auto_migrate);
    println!(
        "objects:      {}",
        if discord {
            "discord".to_string()
        } else {
            match &config.object_store_path {
                Some(path) => path.display().to_string(),
                None => "not configured".to_string(),
            }
        }
    );
    println!("listen:       {}", config.server_addr);
    println!("part size:    {} bytes", config.chunk_size);
    println!("gc retention: {}s", config.gc_retention.as_secs());
    println!(
        "secrets:      master key {}, api token {}",
        present(config.master_key.is_some()),
        present(config.api_token.is_some())
    );

    if blocking.is_empty() {
        println!("\nthe server would start.");
        ExitCode::SUCCESS
    } else {
        println!("\nthe server would refuse to start:");
        for reason in blocking {
            println!("  - {reason}");
        }
        ExitCode::FAILURE
    }
}

fn present(yes: bool) -> &'static str {
    if yes {
        "set"
    } else {
        "missing"
    }
}

async fn status(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut server =
        std::env::var("DCFS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => match args.next() {
                Some(url) => server = url,
                None => {
                    eprintln!("dcfs: --server needs a URL");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("dcfs: unknown option {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    // Only from the environment: a token on the command line shows up in `ps`.
    let token = std::env::var("DCFS_TOKEN")
        .or_else(|_| std::env::var("API_TOKEN"))
        .ok();

    let server = server.trim_end_matches('/');
    let http = reqwest::Client::new();

    match http.get(format!("{server}/health")).send().await {
        Ok(resp) if resp.status().is_success() => println!("health:    ok"),
        Ok(resp) => {
            println!("health:    HTTP {}", resp.status());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            println!("health:    cannot reach {server}: {e}");
            return ExitCode::FAILURE;
        }
    }

    let Some(token) = token else {
        println!("part size: needs a token; set DCFS_TOKEN");
        return ExitCode::SUCCESS;
    };
    let request = http
        .get(format!("{server}/api/v1/fs"))
        .bearer_auth(token)
        .send()
        .await;
    match request {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(body) => println!("part size: {} bytes", body["chunk_size"]),
                Err(e) => println!("part size: unreadable response: {e}"),
            }
            ExitCode::SUCCESS
        }
        Ok(resp) if resp.status().as_u16() == 401 => {
            println!("part size: unauthorized; the token was refused");
            ExitCode::FAILURE
        }
        Ok(resp) => {
            println!("part size: HTTP {}", resp.status());
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("part size: {e}");
            ExitCode::FAILURE
        }
    }
}

fn checked_resume_offset(remote_size: u64, local_size: u64) -> Result<u64, ()> {
    if remote_size > local_size {
        Err(())
    } else {
        Ok(remote_size)
    }
}

fn next_read_len(total: u64, offset: u64, capacity: usize) -> usize {
    total.saturating_sub(offset).min(capacity as u64) as usize
}

/// Upload a local file, sending only what the server does not already have.
///
/// A large upload can take hours — 30 GB measured at close to four — so the
/// interesting case is the second attempt. The server keeps a write open
/// across requests and reports the size it has actually taken, so resuming is
/// a matter of asking and carrying on from there: no part is sent twice, and
/// nothing starts over.
async fn put(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut server =
        std::env::var("DCFS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let mut path: Option<String> = None;
    let mut name: Option<String> = None;
    let mut resume = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" | "--name" => {
                let Some(value) = args.next() else {
                    eprintln!("dcfs: {arg} needs a value");
                    return ExitCode::from(2);
                };
                if arg == "--server" {
                    server = value;
                } else {
                    name = Some(value);
                }
            }
            "--resume" => resume = true,
            other if other.starts_with('-') => {
                eprintln!("dcfs: unknown option {other:?}");
                return ExitCode::from(2);
            }
            other => path = Some(other.to_string()),
        }
    }

    let Some(path) = path else {
        eprintln!("dcfs: put needs a file\n\n{HELP}");
        return ExitCode::from(2);
    };
    // Only from the environment: a token on the command line shows up in `ps`.
    let Some(token) = std::env::var("DCFS_TOKEN")
        .or_else(|_| std::env::var("API_TOKEN"))
        .ok()
    else {
        eprintln!("dcfs: set $DCFS_TOKEN or $API_TOKEN");
        return ExitCode::FAILURE;
    };

    let local = std::path::Path::new(&path);
    let name = name.unwrap_or_else(|| {
        local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_string())
    });
    let total = match std::fs::metadata(local) {
        Ok(meta) => meta.len(),
        Err(e) => {
            eprintln!("dcfs: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let server = server.trim_end_matches('/');
    let http = reqwest::Client::new();
    let auth = |req: reqwest::RequestBuilder| req.bearer_auth(&token);

    // Send in the server's own part size, so each request is one part.
    let part_size = match auth(http.get(format!("{server}/api/v1/fs"))).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(info) => info["chunk_size"].as_u64().unwrap_or(8 << 20),
            Err(e) => {
                eprintln!("dcfs: cannot read part size: {e}");
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!("dcfs: cannot reach {server}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Upload under a non-final name and publish by rename only after sync.
    // Consumers may scan by filename while an upload is still in progress.
    // Keep the destination hidden until every byte is synced and durable.
    let upload_name = format!(".{name}.dcfs-uploading");

    // Refuse to overwrite a completed destination. --resume is for the hidden
    // upload entry only; the final name means publication already happened.
    let final_existing = auth(http.get(format!("{server}/api/v1/nodes/resolve")))
        .query(&[("path", format!("/{name}"))])
        .send()
        .await;
    if let Ok(resp) = final_existing {
        if resp.status().is_success() {
            let node: serde_json::Value = match resp.json().await {
                Ok(node) => node,
                Err(e) => {
                    eprintln!("dcfs: {name} is there but unreadable: {e}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!(
                "dcfs: {name} already exists ({} bytes); refusing to overwrite a published file",
                node["size"].as_u64().unwrap_or(0)
            );
            return ExitCode::FAILURE;
        }
    }

    // Resume a hidden partial upload, or create one.
    let existing = auth(http.get(format!("{server}/api/v1/nodes/resolve")))
        .query(&[("path", format!("/{upload_name}"))])
        .send()
        .await;
    let (node_id, root_id, mut offset) = match existing {
        Ok(resp) if resp.status().is_success() => {
            let node: serde_json::Value = match resp.json().await {
                Ok(node) => node,
                Err(e) => {
                    eprintln!("dcfs: {upload_name} is there but unreadable: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if !resume {
                eprintln!(
                    "dcfs: partial upload {upload_name} already exists ({} bytes); pass --resume to carry on",
                    node["size"].as_u64().unwrap_or(0)
                );
                return ExitCode::FAILURE;
            }
            let id = node["id"].as_str().unwrap_or_default().to_string();
            let parent = node["parent_id"].as_str().unwrap_or_default().to_string();
            let remote_size = node["size"].as_u64().unwrap_or(0);
            let taken = match checked_resume_offset(remote_size, total) {
                Ok(offset) => offset,
                Err(()) => {
                    eprintln!(
                        "dcfs: partial upload {upload_name} is {remote_size} bytes but local source is only {total}; refusing to publish or truncate it"
                    );
                    return ExitCode::FAILURE;
                }
            };
            (id, parent, taken)
        }
        _ => {
            let root = match auth(http.get(format!("{server}/api/v1/nodes/root")))
                .send()
                .await
            {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(root) => root["id"].as_str().unwrap_or_default().to_string(),
                    Err(e) => {
                        eprintln!("dcfs: cannot read the root: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                Err(e) => {
                    eprintln!("dcfs: cannot reach {server}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let body = serde_json::json!({
                "parent_id": root,
                "name": base64::Engine::encode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                    upload_name.as_bytes(),
                ),
                "kind": "File",
                "mode": 0o100644,
                "uid": 0,
                "gid": 0,
                "idempotency_key": uuid::Uuid::new_v4().to_string(),
            });
            match auth(http.post(format!("{server}/api/v1/nodes")))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(node) => (node["id"].as_str().unwrap_or_default().to_string(), root, 0),
                        Err(e) => {
                            eprintln!("dcfs: cannot read the new file: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
                Ok(resp) => {
                    eprintln!("dcfs: cannot create {upload_name}: HTTP {}", resp.status());
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("dcfs: cannot create {upload_name}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    if offset >= total && total > 0 {
        eprintln!("{upload_name}: already uploaded ({total} bytes); publishing");
    } else if offset > 0 {
        eprintln!("{upload_name}: resuming at {offset} of {total} bytes");
    }

    use std::io::{Read, Seek};
    let mut file = match std::fs::File::open(local) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("dcfs: cannot open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)) {
        eprintln!("dcfs: cannot seek {path}: {e}");
        return ExitCode::FAILURE;
    }

    let started = std::time::Instant::now();
    let mut buf = vec![0u8; part_size as usize];
    while offset < total {
        // Never read past the size captured at startup. If the source grows
        // concurrently, those new bytes belong to a later upload, not this one.
        let wanted = next_read_len(total, offset, buf.len());
        let mut filled = 0;
        while filled < wanted {
            match file.read(&mut buf[filled..wanted]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    eprintln!("dcfs: reading {path} failed at {offset}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if filled == 0 {
            eprintln!(
                "dcfs: source ended at {offset} bytes but was {total} bytes when upload started; partial upload remains as {upload_name}"
            );
            return ExitCode::FAILURE;
        }

        // Retry a part rather than lose an upload that is hours in.
        let mut sent = false;
        for attempt in 1..=3 {
            let resp = auth(http.put(format!("{server}/api/v1/nodes/{node_id}/data")))
                .query(&[("offset", offset.to_string())])
                .body(buf[..filled].to_vec())
                .send()
                .await;
            match resp {
                Ok(resp) if resp.status().is_success() => {
                    sent = true;
                    break;
                }
                Ok(resp) => eprintln!(
                    "  offset {offset}: HTTP {} (try {attempt}/3)",
                    resp.status()
                ),
                Err(e) => eprintln!("  offset {offset}: {e} (try {attempt}/3)"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(attempt * 5)).await;
        }
        if !sent {
            eprintln!("dcfs: giving up at {offset}; rerun with --resume to carry on from here");
            return ExitCode::FAILURE;
        }

        offset += filled as u64;
        let secs = started.elapsed().as_secs_f64();
        eprint!(
            "\r{name}: {:.1}% of {total} bytes, {:.2} MB/s   ",
            offset as f64 / total as f64 * 100.0,
            (offset as f64) / 1e6 / secs.max(0.001)
        );
    }
    eprintln!();

    if offset != total {
        eprintln!(
            "dcfs: source changed during upload; sent {offset} of the original {total} bytes and will not publish"
        );
        return ExitCode::FAILURE;
    }

    let current_size = match std::fs::metadata(local) {
        Ok(meta) => meta.len(),
        Err(e) => {
            eprintln!(
                "dcfs: cannot re-stat {path} before publication: {e}; partial upload remains as {upload_name}"
            );
            return ExitCode::FAILURE;
        }
    };
    if current_size != total {
        eprintln!(
            "dcfs: source size changed during upload from {total} to {current_size} bytes; refusing publication"
        );
        return ExitCode::FAILURE;
    }

    // Sync first, then publish atomically under the requested name.
    match auth(http.post(format!("{server}/api/v1/nodes/{node_id}/sync")))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            eprintln!(
                "dcfs: upload finished but sync returned HTTP {}",
                resp.status()
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("dcfs: upload finished but sync failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    let publish = serde_json::json!({
        "new_parent_id": root_id,
        "new_name": base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            name.as_bytes(),
        ),
        "idempotency_key": uuid::Uuid::new_v4().to_string(),
    });
    match auth(http.post(format!("{server}/api/v1/nodes/{node_id}/publish")))
        .json(&publish)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            println!(
                "{name}: {total} bytes in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Ok(resp) => {
            eprintln!(
                "dcfs: upload synced but publishing {name} returned HTTP {}; partial upload remains as {upload_name}",
                resp.status()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!(
                "dcfs: upload synced but publishing {name} failed: {e}; partial upload remains as {upload_name}"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod put_tests {
    use super::{checked_resume_offset, next_read_len};

    #[test]
    fn resume_rejects_remote_larger_than_local_source() {
        assert_eq!(checked_resume_offset(10, 10), Ok(10));
        assert_eq!(checked_resume_offset(9, 10), Ok(9));
        assert_eq!(checked_resume_offset(11, 10), Err(()));
    }

    #[test]
    fn read_window_never_exceeds_initial_source_size() {
        assert_eq!(next_read_len(10, 0, 8), 8);
        assert_eq!(next_read_len(10, 8, 8), 2);
        assert_eq!(next_read_len(10, 10, 8), 0);
    }
}

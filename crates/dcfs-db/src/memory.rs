//! In-memory metadata repository for testing.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use parking_lot::RwLock;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    CommitGuard, FileChunkRecord, FileVersionRecord, MetadataRepository, NodeGuard, NodeRecord,
    ObjectLocatorRecord, RepositoryError, SessionRecord,
};

/// In-memory metadata repository for deterministic testing.
pub struct MemoryMetadataRepository {
    nodes: RwLock<HashMap<Uuid, NodeRecord>>,
    versions: RwLock<HashMap<Uuid, FileVersionRecord>>,
    chunks: RwLock<HashMap<Uuid, Vec<FileChunkRecord>>>,
    /// Keyed by token hash, with a revoked flag.
    sessions: RwLock<HashMap<String, (SessionRecord, bool)>>,
    locators: RwLock<HashMap<Uuid, ObjectLocatorRecord>>,
    root_id: Uuid,
}

impl MemoryMetadataRepository {
    pub fn new() -> Self {
        let root_id = Uuid::new_v4();
        let now = Utc::now();
        let root = NodeRecord {
            id: root_id,
            parent_id: None,
            name: Vec::new(), // root has no name component

            kind: "directory".to_string(),
            mode: 0o40755,
            uid: 0,
            gid: 0,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            current_version_id: None,
            generation: 0,
            link_target: None,
        };
        let mut nodes = HashMap::new();
        nodes.insert(root_id, root);
        Self {
            nodes: RwLock::new(nodes),
            versions: RwLock::new(HashMap::new()),
            chunks: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            locators: RwLock::new(HashMap::new()),
            root_id,
        }
    }
}

impl Default for MemoryMetadataRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetadataRepository for MemoryMetadataRepository {
    async fn get_node(&self, id: Uuid) -> Result<NodeRecord, RepositoryError> {
        self.nodes
            .read()
            .get(&id)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn get_root(&self) -> Result<NodeRecord, RepositoryError> {
        self.get_node(self.root_id).await
    }

    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NodeRecord>, RepositoryError> {
        let nodes = self.nodes.read();
        let children: Vec<NodeRecord> = nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id))
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        Ok(children)
    }

    async fn find_child(
        &self,
        parent_id: Uuid,
        name: &[u8],
    ) -> Result<NodeRecord, RepositoryError> {
        self.nodes
            .read()
            .values()
            .find(|n| n.parent_id == Some(parent_id) && n.name == name)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn create_node(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        kind: &str,
        mode: i32,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError> {
        let mut nodes = self.nodes.write();
        if nodes.contains_key(&id) {
            return Err(RepositoryError::AlreadyExists);
        }
        let now = Utc::now();
        let record = NodeRecord {
            id,
            parent_id: Some(parent_id),
            name,
            kind: kind.to_string(),
            mode,
            uid,
            gid,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            current_version_id: None,
            generation: 0,
            link_target: None,
        };
        nodes.insert(id, record.clone());
        Ok(record)
    }

    async fn create_symlink(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        target: Vec<u8>,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError> {
        let mut nodes = self.nodes.write();
        if nodes.contains_key(&id) {
            return Err(RepositoryError::AlreadyExists);
        }
        if nodes
            .values()
            .any(|n| n.parent_id == Some(parent_id) && n.name == name)
        {
            return Err(RepositoryError::AlreadyExists);
        }
        let now = Utc::now();
        let record = NodeRecord {
            id,
            parent_id: Some(parent_id),
            name,
            kind: "symlink".to_string(),
            mode: 0o120777,
            uid,
            gid,
            size: target.len() as i64,
            atime: now,
            mtime: now,
            ctime: now,
            current_version_id: None,
            generation: 0,
            link_target: Some(target),
        };
        nodes.insert(id, record.clone());
        Ok(record)
    }

    async fn delete_node(&self, id: Uuid) -> Result<(), RepositoryError> {
        let mut nodes = self.nodes.write();
        nodes.remove(&id).ok_or(RepositoryError::NotFound)?;
        Ok(())
    }

    async fn rename_node(
        &self,
        id: Uuid,
        new_parent_id: Uuid,
        new_name: Vec<u8>,
    ) -> Result<(), RepositoryError> {
        let mut nodes = self.nodes.write();
        if !nodes.contains_key(&id) {
            return Err(RepositoryError::NotFound);
        }

        // Same contract as PgRepository: rename(2) replaces the destination.
        let victim = nodes
            .values()
            .find(|n| n.id != id && n.parent_id == Some(new_parent_id) && n.name == new_name)
            .map(|n| (n.id, n.kind.clone()));
        if let Some((victim_id, kind)) = victim {
            if kind == "directory" && nodes.values().any(|n| n.parent_id == Some(victim_id)) {
                return Err(RepositoryError::DirectoryNotEmpty);
            }
            nodes.remove(&victim_id);
        }

        let node = nodes.get_mut(&id).ok_or(RepositoryError::NotFound)?;
        node.parent_id = Some(new_parent_id);
        node.name = new_name;
        node.ctime = Utc::now();
        Ok(())
    }

    async fn publish_node(
        &self,
        id: Uuid,
        new_parent_id: Uuid,
        new_name: Vec<u8>,
    ) -> Result<(), RepositoryError> {
        let mut nodes = self.nodes.write();
        if !nodes.contains_key(&id) {
            return Err(RepositoryError::NotFound);
        }
        if nodes
            .values()
            .any(|n| n.id != id && n.parent_id == Some(new_parent_id) && n.name == new_name)
        {
            return Err(RepositoryError::AlreadyExists);
        }

        let node = nodes.get_mut(&id).ok_or(RepositoryError::NotFound)?;
        node.parent_id = Some(new_parent_id);
        node.name = new_name;
        node.ctime = Utc::now();
        Ok(())
    }

    async fn update_node_attr(
        &self,
        id: Uuid,
        mode: Option<i32>,
        uid: Option<i32>,
        gid: Option<i32>,
        size: Option<i64>,
        mtime: Option<DateTime<Utc>>,
        atime: Option<DateTime<Utc>>,
    ) -> Result<NodeRecord, RepositoryError> {
        let mut nodes = self.nodes.write();
        let node = nodes.get_mut(&id).ok_or(RepositoryError::NotFound)?;
        if let Some(m) = mode {
            node.mode = m;
        }
        if let Some(u) = uid {
            node.uid = u;
        }
        if let Some(g) = gid {
            node.gid = g;
        }
        if let Some(s) = size {
            node.size = s;
        }
        if let Some(m) = mtime {
            node.mtime = m;
        }
        if let Some(a) = atime {
            node.atime = a;
        }
        node.ctime = Utc::now();
        Ok(node.clone())
    }

    async fn create_staging_version(
        &self,
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        chunk_size: i64,
    ) -> Result<FileVersionRecord, RepositoryError> {
        let mut nodes = self.nodes.write();
        let node = nodes.get_mut(&node_id).ok_or(RepositoryError::NotFound)?;
        node.generation += 1;

        let version_id = Uuid::new_v4();
        let now = Utc::now();
        let record = FileVersionRecord {
            id: version_id,
            node_id,
            base_version_id,
            state: "staging".to_string(),
            size: 0,
            plaintext_hash: String::new(),
            chunk_size,
            created_at: now,
            committed_at: None,
        };
        self.versions.write().insert(version_id, record.clone());
        self.chunks.write().insert(version_id, Vec::new());
        Ok(record)
    }

    async fn lock_node(&self, _node_id: Uuid) -> Result<NodeGuard, RepositoryError> {
        // One process, one copy of this map: the mutex the server already
        // holds is the whole lock.
        Ok(NodeGuard::unlocked())
    }

    async fn try_lock_gc(&self) -> Result<Option<NodeGuard>, RepositoryError> {
        // One process, one sweeper.
        Ok(Some(NodeGuard::unlocked()))
    }

    async fn find_open_staging_version(
        &self,
        node_id: Uuid,
    ) -> Result<FileVersionRecord, RepositoryError> {
        self.versions
            .read()
            .values()
            .filter(|v| v.node_id == node_id && v.state == "staging")
            .max_by_key(|v| v.created_at)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn touch_staging_version(
        &self,
        version_id: Uuid,
        size: i64,
    ) -> Result<(), RepositoryError> {
        if let Some(version) = self.versions.write().get_mut(&version_id) {
            if version.state == "staging" {
                version.size = size;
                version.created_at = Utc::now();
            }
        }
        Ok(())
    }

    async fn reconcile_node_sizes(&self) -> Result<u64, RepositoryError> {
        let versions = self.versions.read();
        let mut nodes = self.nodes.write();
        let staging: Vec<Uuid> = versions
            .values()
            .filter(|v| v.state == "staging")
            .map(|v| v.node_id)
            .collect();

        let mut fixed = 0;
        for node in nodes.values_mut() {
            if node.kind != "file" || staging.contains(&node.id) {
                continue;
            }
            let committed = node
                .current_version_id
                .and_then(|id| versions.get(&id))
                .map(|v| v.size)
                .unwrap_or(0);
            if node.size != committed {
                node.size = committed;
                fixed += 1;
            }
        }
        Ok(fixed)
    }

    async fn attach_chunk(
        &self,
        version_id: Uuid,
        chunk_index: i64,
        logical_offset: i64,
        plaintext_size: i32,
        plaintext_hash: &str,
        object_id: Uuid,
    ) -> Result<(), RepositoryError> {
        let mut chunks = self.chunks.write();
        let version_chunks = chunks
            .get_mut(&version_id)
            .ok_or(RepositoryError::NotFound)?;
        let record = FileChunkRecord {
            version_id,
            chunk_index,
            logical_offset,
            plaintext_size,
            plaintext_hash: plaintext_hash.to_string(),
            object_id,
        };
        // Same contract as PgRepository: re-uploading an index replaces it, and
        // the manifest reads back ordered by index rather than by arrival.
        match version_chunks.binary_search_by_key(&chunk_index, |c| c.chunk_index) {
            Ok(existing) => version_chunks[existing] = record,
            Err(insert_at) => version_chunks.insert(insert_at, record),
        }
        Ok(())
    }

    async fn get_chunks(&self, version_id: Uuid) -> Result<Vec<FileChunkRecord>, RepositoryError> {
        self.chunks
            .read()
            .get(&version_id)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn get_chunk_range(
        &self,
        version_id: Uuid,
        first: i64,
        last: i64,
    ) -> Result<Vec<FileChunkRecord>, RepositoryError> {
        let chunks = self.chunks.read();
        let version_chunks = chunks.get(&version_id).ok_or(RepositoryError::NotFound)?;
        Ok(version_chunks
            .iter()
            .filter(|c| c.chunk_index >= first && c.chunk_index <= last)
            .cloned()
            .collect())
    }

    async fn copy_chunk_range(
        &self,
        from_version: Uuid,
        to_version: Uuid,
        first: i64,
        last: i64,
    ) -> Result<u64, RepositoryError> {
        if first > last {
            return Ok(0);
        }
        let mut chunks = self.chunks.write();
        let carried: Vec<FileChunkRecord> = chunks
            .get(&from_version)
            .map(|source| {
                source
                    .iter()
                    .filter(|c| c.chunk_index >= first && c.chunk_index <= last)
                    .map(|c| FileChunkRecord {
                        version_id: to_version,
                        ..c.clone()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let target = chunks.entry(to_version).or_default();
        let mut copied = 0;
        for record in carried {
            if let Err(at) = target.binary_search_by_key(&record.chunk_index, |c| c.chunk_index) {
                target.insert(at, record);
                copied += 1;
            }
        }
        Ok(copied)
    }

    async fn commit_version(
        &self,
        guard: CommitGuard,
        total_size: i64,
        plaintext_hash: &str,
    ) -> Result<FileVersionRecord, RepositoryError> {
        let mut nodes = self.nodes.write();
        let node = nodes
            .get_mut(&guard.node_id)
            .ok_or(RepositoryError::NotFound)?;

        if node.generation != guard.expected_generation {
            return Err(RepositoryError::Conflict);
        }
        if node.current_version_id != guard.expected_current_version {
            return Err(RepositoryError::Conflict);
        }

        let mut versions = self.versions.write();
        if !versions.contains_key(&guard.version_id) {
            return Err(RepositoryError::NotFound);
        }
        if let Some(old) = node.current_version_id {
            if let Some(old_version) = versions.get_mut(&old) {
                if old_version.state == "committed" {
                    old_version.state = "superseded".to_string();
                }
            }
        }

        let version = versions
            .get_mut(&guard.version_id)
            .ok_or(RepositoryError::NotFound)?;
        version.state = "committed".to_string();
        version.size = total_size;
        version.plaintext_hash = plaintext_hash.to_string();
        version.committed_at = Some(Utc::now());

        node.current_version_id = Some(guard.version_id);
        node.size = total_size;

        Ok(version.clone())
    }

    async fn get_version(&self, id: Uuid) -> Result<FileVersionRecord, RepositoryError> {
        self.versions
            .read()
            .get(&id)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn put_object_locator(
        &self,
        locator: &ObjectLocatorRecord,
    ) -> Result<(), RepositoryError> {
        self.locators
            .write()
            .insert(locator.object_id, locator.clone());
        Ok(())
    }

    async fn get_object_locator(
        &self,
        object_id: Uuid,
    ) -> Result<ObjectLocatorRecord, RepositoryError> {
        self.locators
            .read()
            .get(&object_id)
            .cloned()
            .ok_or(RepositoryError::NotFound)
    }

    async fn touch_object_url(&self, object_id: Uuid, url: &str) -> Result<(), RepositoryError> {
        if let Some(locator) = self.locators.write().get_mut(&object_id) {
            locator.url = url.to_string();
        }
        Ok(())
    }

    async fn delete_object_locator(&self, object_id: Uuid) -> Result<(), RepositoryError> {
        self.locators.write().remove(&object_id);
        Ok(())
    }

    async fn create_session(
        &self,
        id: Uuid,
        token_hash: &str,
        label: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, RepositoryError> {
        let record = SessionRecord {
            id,
            label: label.to_string(),
            created_at: Utc::now(),
            expires_at,
        };
        self.sessions
            .write()
            .insert(token_hash.to_string(), (record.clone(), false));
        Ok(record)
    }

    async fn find_session(&self, token_hash: &str) -> Result<SessionRecord, RepositoryError> {
        let sessions = self.sessions.read();
        let (record, revoked) = sessions.get(token_hash).ok_or(RepositoryError::NotFound)?;
        if *revoked || record.expires_at <= Utc::now() {
            return Err(RepositoryError::NotFound);
        }
        Ok(record.clone())
    }

    async fn revoke_session(&self, id: Uuid) -> Result<bool, RepositoryError> {
        let mut sessions = self.sessions.write();
        for (record, revoked) in sessions.values_mut() {
            if record.id == id && !*revoked {
                *revoked = true;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn purge_expired_sessions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let mut sessions = self.sessions.write();
        let before = sessions.len();
        sessions.retain(|_, (record, _)| record.expires_at >= older_than);
        Ok((before - sessions.len()) as u64)
    }

    async fn collectable_objects(
        &self,
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Uuid>, RepositoryError> {
        let nodes = self.nodes.read();
        let versions = self.versions.read();
        let chunks = self.chunks.read();

        // Same definition of "live" as PgRepository: the current version of a
        // node that still exists, or a version still being staged.
        // Same definition as PgRepository: a staging version protects its
        // chunks only while its upload could still be in progress.
        let live: Vec<Uuid> = versions
            .values()
            .filter(|version| {
                (version.state == "staging" && version.created_at >= older_than)
                    || nodes
                        .get(&version.node_id)
                        .is_some_and(|node| node.current_version_id == Some(version.id))
            })
            .map(|version| version.id)
            .collect();

        let referenced_by_live: Vec<Uuid> = live
            .iter()
            .filter_map(|id| chunks.get(id))
            .flatten()
            .map(|chunk| chunk.object_id)
            .collect();

        let mut collectable = Vec::new();
        for (version_id, version_chunks) in chunks.iter() {
            let Some(version) = versions.get(version_id) else {
                continue;
            };
            if live.contains(version_id) {
                continue;
            }
            if version.committed_at.unwrap_or(version.created_at) >= older_than {
                continue;
            }
            for chunk in version_chunks {
                if !referenced_by_live.contains(&chunk.object_id)
                    && !collectable.contains(&chunk.object_id)
                {
                    collectable.push(chunk.object_id);
                    if collectable.len() as i64 >= limit {
                        return Ok(collectable);
                    }
                }
            }
        }
        Ok(collectable)
    }

    async fn forget_object(&self, object_id: Uuid) -> Result<u64, RepositoryError> {
        let mut chunks = self.chunks.write();
        let mut removed = 0;
        for version_chunks in chunks.values_mut() {
            let before = version_chunks.len();
            version_chunks.retain(|chunk| chunk.object_id != object_id);
            removed += (before - version_chunks.len()) as u64;
        }
        Ok(removed)
    }

    async fn purge_empty_dead_versions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let nodes = self.nodes.read();
        let mut versions = self.versions.write();
        let mut chunks = self.chunks.write();

        let doomed: Vec<Uuid> = versions
            .values()
            .filter(|version| {
                version.committed_at.unwrap_or(version.created_at) < older_than
                    && !nodes
                        .get(&version.node_id)
                        .is_some_and(|node| node.current_version_id == Some(version.id))
                    && chunks.get(&version.id).map_or(true, |c| c.is_empty())
            })
            .map(|version| version.id)
            .collect();

        for id in &doomed {
            versions.remove(id);
            chunks.remove(id);
        }
        Ok(doomed.len() as u64)
    }

    async fn purge_deleted_nodes(
        &self,
        _older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        // The in-memory repository hard-deletes on unlink, so a soft-deleted
        // node never exists here and there is nothing to sweep.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_root_returns_root_node() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        assert_eq!(root.parent_id, None);
        assert!(root.name.is_empty(), "root has no name component");
        assert_eq!(root.kind, "directory");
    }

    #[tokio::test]
    async fn create_and_get_node() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        let node = repo
            .create_node(id, root.id, b"hello".to_vec(), "file", 0o100644, 1000, 1000)
            .await
            .unwrap();
        assert_eq!(node.id, id);
        assert_eq!(node.name, b"hello".to_vec());
        assert_eq!(node.generation, 0);

        let fetched = repo.get_node(id).await.unwrap();
        assert_eq!(fetched.id, id);
    }

    #[tokio::test]
    async fn create_node_already_exists() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"a".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        let err = repo
            .create_node(id, root.id, b"b".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap_err();
        assert!(matches!(err, RepositoryError::AlreadyExists));
    }

    #[tokio::test]
    async fn delete_node() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"x".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        repo.delete_node(id).await.unwrap();
        assert!(matches!(
            repo.get_node(id).await.unwrap_err(),
            RepositoryError::NotFound
        ));
    }

    #[tokio::test]
    async fn delete_node_not_found() {
        let repo = MemoryMetadataRepository::new();
        assert!(matches!(
            repo.delete_node(Uuid::new_v4()).await.unwrap_err(),
            RepositoryError::NotFound
        ));
    }

    #[tokio::test]
    async fn rename_node() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"old".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        repo.rename_node(id, root.id, b"new".to_vec())
            .await
            .unwrap();
        let node = repo.get_node(id).await.unwrap();
        assert_eq!(node.name, b"new".to_vec());
    }

    #[tokio::test]
    async fn update_node_attr() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"f".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        let updated = repo
            .update_node_attr(id, Some(0o100755), None, None, Some(42), None, None)
            .await
            .unwrap();
        assert_eq!(updated.mode, 0o100755);
        assert_eq!(updated.size, 42);
        assert_eq!(updated.uid, 0); // unchanged
    }

    #[tokio::test]
    async fn list_children_with_limit_offset() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        for i in 0..5 {
            repo.create_node(
                Uuid::new_v4(),
                root.id,
                format!("f{i}").into_bytes(),
                "file",
                0o100644,
                0,
                0,
            )
            .await
            .unwrap();
        }
        let page = repo.list_children(root.id, 2, 0).await.unwrap();
        assert_eq!(page.len(), 2);
        let page2 = repo.list_children(root.id, 10, 3).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn staging_version_and_chunks() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"f".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();

        let version = repo.create_staging_version(id, None, 4096).await.unwrap();
        assert_eq!(version.state, "staging");
        assert_eq!(version.chunk_size, 4096);

        let obj = Uuid::new_v4();
        repo.attach_chunk(version.id, 0, 0, 100, "hash0", obj)
            .await
            .unwrap();
        repo.attach_chunk(version.id, 1, 100, 50, "hash1", obj)
            .await
            .unwrap();

        let chunks = repo.get_chunks(version.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].plaintext_size, 50);
    }

    #[tokio::test]
    async fn commit_version_atomic_success() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"f".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();

        let version = repo.create_staging_version(id, None, 4096).await.unwrap();
        let node = repo.get_node(id).await.unwrap();
        assert_eq!(node.generation, 1);

        let guard = CommitGuard {
            node_id: id,
            version_id: version.id,
            expected_generation: 1,
            expected_current_version: None,
        };
        let committed = repo.commit_version(guard, 150, "totalhash").await.unwrap();
        assert_eq!(committed.state, "committed");
        assert_eq!(committed.size, 150);
        assert_eq!(committed.plaintext_hash, "totalhash");
        assert!(committed.committed_at.is_some());

        let node = repo.get_node(id).await.unwrap();
        assert_eq!(node.current_version_id, Some(version.id));
    }

    #[tokio::test]
    async fn commit_version_conflict_on_generation() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"f".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        let v1 = repo.create_staging_version(id, None, 4096).await.unwrap();
        // bump generation again
        let _v2 = repo
            .create_staging_version(id, Some(v1.id), 4096)
            .await
            .unwrap();

        let guard = CommitGuard {
            node_id: id,
            version_id: v1.id,
            expected_generation: 1, // stale
            expected_current_version: None,
        };
        assert!(matches!(
            repo.commit_version(guard, 0, "").await.unwrap_err(),
            RepositoryError::Conflict
        ));
    }

    #[tokio::test]
    async fn commit_version_conflict_on_current_version() {
        let repo = MemoryMetadataRepository::new();
        let root = repo.get_root().await.unwrap();
        let id = Uuid::new_v4();
        repo.create_node(id, root.id, b"f".to_vec(), "file", 0o100644, 0, 0)
            .await
            .unwrap();
        let v1 = repo.create_staging_version(id, None, 4096).await.unwrap();
        let guard1 = CommitGuard {
            node_id: id,
            version_id: v1.id,
            expected_generation: 1,
            expected_current_version: None,
        };
        repo.commit_version(guard1, 10, "h").await.unwrap();

        // now try with stale expected_current_version
        let v2 = repo
            .create_staging_version(id, Some(v1.id), 4096)
            .await
            .unwrap();
        let guard2 = CommitGuard {
            node_id: id,
            version_id: v2.id,
            expected_generation: 2,
            expected_current_version: None, // stale, should be Some(v1.id)
        };
        assert!(matches!(
            repo.commit_version(guard2, 0, "").await.unwrap_err(),
            RepositoryError::Conflict
        ));
    }

    #[tokio::test]
    async fn get_version_not_found() {
        let repo = MemoryMetadataRepository::new();
        assert!(matches!(
            repo.get_version(Uuid::new_v4()).await.unwrap_err(),
            RepositoryError::NotFound
        ));
    }

    #[tokio::test]
    async fn get_chunks_not_found() {
        let repo = MemoryMetadataRepository::new();
        assert!(matches!(
            repo.get_chunks(Uuid::new_v4()).await.unwrap_err(),
            RepositoryError::NotFound
        ));
    }
}

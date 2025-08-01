// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(missing_docs)]

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use itertools::Itertools as _;
use prost::Message as _;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::backend::BackendInitError;
use crate::backend::CommitId;
use crate::backend::MillisSinceEpoch;
use crate::backend::Timestamp;
use crate::content_hash::blake2b_hash;
use crate::dag_walk;
use crate::file_util::IoResultExt as _;
use crate::file_util::PathError;
use crate::file_util::persist_content_addressed_temp_file;
use crate::merge::Merge;
use crate::object_id::HexPrefix;
use crate::object_id::ObjectId;
use crate::object_id::PrefixResolution;
use crate::op_store;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::OpStoreResult;
use crate::op_store::Operation;
use crate::op_store::OperationId;
use crate::op_store::OperationMetadata;
use crate::op_store::RefTarget;
use crate::op_store::RemoteRef;
use crate::op_store::RemoteRefState;
use crate::op_store::RemoteView;
use crate::op_store::RootOperationData;
use crate::op_store::TimestampRange;
use crate::op_store::View;
use crate::op_store::ViewId;
use crate::ref_name::GitRefNameBuf;
use crate::ref_name::RefNameBuf;
use crate::ref_name::RemoteNameBuf;
use crate::ref_name::WorkspaceName;
use crate::ref_name::WorkspaceNameBuf;

// BLAKE2b-512 hash length in bytes
const OPERATION_ID_LENGTH: usize = 64;
const VIEW_ID_LENGTH: usize = 64;

/// Error that may occur during [`SimpleOpStore`] initialization.
#[derive(Debug, Error)]
#[error("Failed to initialize simple operation store")]
pub struct SimpleOpStoreInitError(#[from] pub PathError);

impl From<SimpleOpStoreInitError> for BackendInitError {
    fn from(err: SimpleOpStoreInitError) -> Self {
        Self(err.into())
    }
}

#[derive(Debug)]
pub struct SimpleOpStore {
    path: PathBuf,
    root_data: RootOperationData,
    root_operation_id: OperationId,
    root_view_id: ViewId,
}

impl SimpleOpStore {
    pub fn name() -> &'static str {
        "simple_op_store"
    }

    /// Creates an empty OpStore. Returns error if it already exists.
    pub fn init(
        store_path: &Path,
        root_data: RootOperationData,
    ) -> Result<Self, SimpleOpStoreInitError> {
        let store = Self::new(store_path, root_data);
        store.init_base_dirs()?;
        Ok(store)
    }

    /// Load an existing OpStore
    pub fn load(store_path: &Path, root_data: RootOperationData) -> Self {
        Self::new(store_path, root_data)
    }

    fn new(store_path: &Path, root_data: RootOperationData) -> Self {
        Self {
            path: store_path.to_path_buf(),
            root_data,
            root_operation_id: OperationId::from_bytes(&[0; OPERATION_ID_LENGTH]),
            root_view_id: ViewId::from_bytes(&[0; VIEW_ID_LENGTH]),
        }
    }

    fn init_base_dirs(&self) -> Result<(), PathError> {
        for dir in [self.views_dir(), self.operations_dir()] {
            fs::create_dir(&dir).context(&dir)?;
        }
        Ok(())
    }

    fn views_dir(&self) -> PathBuf {
        self.path.join("views")
    }

    fn operations_dir(&self) -> PathBuf {
        self.path.join("operations")
    }
}

impl OpStore for SimpleOpStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        Self::name()
    }

    fn root_operation_id(&self) -> &OperationId {
        &self.root_operation_id
    }

    fn read_view(&self, id: &ViewId) -> OpStoreResult<View> {
        if *id == self.root_view_id {
            return Ok(View::make_root(self.root_data.root_commit_id.clone()));
        }

        let path = self.views_dir().join(id.hex());
        let buf = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;

        let proto = crate::protos::op_store::View::decode(&*buf)
            .map_err(|err| to_read_error(err.into(), id))?;
        Ok(view_from_proto(proto))
    }

    fn write_view(&self, view: &View) -> OpStoreResult<ViewId> {
        let dir = self.views_dir();
        let temp_file = NamedTempFile::new_in(&dir)
            .context(&dir)
            .map_err(|err| io_to_write_error(err, "view"))?;

        let proto = view_to_proto(view);
        temp_file
            .as_file()
            .write_all(&proto.encode_to_vec())
            .context(temp_file.path())
            .map_err(|err| io_to_write_error(err, "view"))?;

        let id = ViewId::new(blake2b_hash(view).to_vec());

        let new_path = dir.join(id.hex());
        persist_content_addressed_temp_file(temp_file, &new_path)
            .context(&new_path)
            .map_err(|err| io_to_write_error(err, "view"))?;
        Ok(id)
    }

    fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation> {
        if *id == self.root_operation_id {
            return Ok(Operation::make_root(self.root_view_id.clone()));
        }

        let path = self.operations_dir().join(id.hex());
        let buf = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;

        let proto = crate::protos::op_store::Operation::decode(&*buf)
            .map_err(|err| to_read_error(err.into(), id))?;
        let mut operation =
            operation_from_proto(proto).map_err(|err| to_read_error(err.into(), id))?;
        if operation.parents.is_empty() {
            // Repos created before we had the root operation will have an operation without
            // parents.
            operation.parents.push(self.root_operation_id.clone());
        }
        Ok(operation)
    }

    fn write_operation(&self, operation: &Operation) -> OpStoreResult<OperationId> {
        assert!(!operation.parents.is_empty());
        let dir = self.operations_dir();
        let temp_file = NamedTempFile::new_in(&dir)
            .context(&dir)
            .map_err(|err| io_to_write_error(err, "operation"))?;

        let proto = operation_to_proto(operation);
        temp_file
            .as_file()
            .write_all(&proto.encode_to_vec())
            .context(temp_file.path())
            .map_err(|err| io_to_write_error(err, "operation"))?;

        let id = OperationId::new(blake2b_hash(operation).to_vec());

        let new_path = dir.join(id.hex());
        persist_content_addressed_temp_file(temp_file, &new_path)
            .context(&new_path)
            .map_err(|err| io_to_write_error(err, "operation"))?;
        Ok(id)
    }

    fn resolve_operation_id_prefix(
        &self,
        prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>> {
        let op_dir = self.operations_dir();
        let find = || -> io::Result<_> {
            let matches_root = prefix.matches(&self.root_operation_id);
            let hex_prefix = prefix.hex();
            if hex_prefix.len() == OPERATION_ID_LENGTH * 2 {
                // Fast path for full-length ID
                if matches_root || op_dir.join(hex_prefix).try_exists()? {
                    let id = OperationId::from_bytes(prefix.as_full_bytes().unwrap());
                    return Ok(PrefixResolution::SingleMatch(id));
                } else {
                    return Ok(PrefixResolution::NoMatch);
                }
            }

            let mut matched = matches_root.then(|| self.root_operation_id.clone());
            for entry in op_dir.read_dir()? {
                let Ok(name) = entry?.file_name().into_string() else {
                    continue; // Skip invalid UTF-8
                };
                if !name.starts_with(&hex_prefix) {
                    continue;
                }
                let Some(id) = OperationId::try_from_hex(&name) else {
                    continue; // Skip invalid hex
                };
                if matched.is_some() {
                    return Ok(PrefixResolution::AmbiguousMatch);
                }
                matched = Some(id);
            }
            if let Some(id) = matched {
                Ok(PrefixResolution::SingleMatch(id))
            } else {
                Ok(PrefixResolution::NoMatch)
            }
        };
        find()
            .context(&op_dir)
            .map_err(|err| OpStoreError::Other(err.into()))
    }

    #[tracing::instrument(skip(self))]
    fn gc(&self, head_ids: &[OperationId], keep_newer: SystemTime) -> OpStoreResult<()> {
        let to_op_id = |entry: &fs::DirEntry| -> Option<OperationId> {
            let name = entry.file_name().into_string().ok()?;
            OperationId::try_from_hex(name)
        };
        let to_view_id = |entry: &fs::DirEntry| -> Option<ViewId> {
            let name = entry.file_name().into_string().ok()?;
            ViewId::try_from_hex(name)
        };
        let remove_file_if_not_new = |entry: &fs::DirEntry| -> Result<(), PathError> {
            let path = entry.path();
            // Check timestamp, but there's still TOCTOU problem if an existing
            // file is renewed.
            let metadata = entry.metadata().context(&path)?;
            let mtime = metadata.modified().expect("unsupported platform?");
            if mtime > keep_newer {
                tracing::trace!(?path, "not removing");
                Ok(())
            } else {
                tracing::trace!(?path, "removing");
                fs::remove_file(&path).context(&path)
            }
        };

        // Reachable objects are resolved without considering the keep_newer
        // parameter. We could collect ancestors of the "new" operations here,
        // but more files can be added anyway after that.
        let read_op = |id: &OperationId| self.read_operation(id).map(|data| (id.clone(), data));
        let reachable_ops: HashMap<OperationId, Operation> = dag_walk::dfs_ok(
            head_ids.iter().map(read_op),
            |(id, _)| id.clone(),
            |(_, data)| data.parents.iter().map(read_op).collect_vec(),
        )
        .try_collect()?;
        let reachable_views: HashSet<&ViewId> =
            reachable_ops.values().map(|data| &data.view_id).collect();
        tracing::info!(
            reachable_op_count = reachable_ops.len(),
            reachable_view_count = reachable_views.len(),
            "collected reachable objects"
        );

        let prune_ops = || -> Result<(), PathError> {
            let op_dir = self.operations_dir();
            for entry in op_dir.read_dir().context(&op_dir)? {
                let entry = entry.context(&op_dir)?;
                let Some(id) = to_op_id(&entry) else {
                    tracing::trace!(?entry, "skipping invalid file name");
                    continue;
                };
                if reachable_ops.contains_key(&id) {
                    continue;
                }
                // If the operation was added after collecting reachable_views,
                // its view mtime would also be renewed. So there's no need to
                // update the reachable_views set to preserve the view.
                remove_file_if_not_new(&entry)?;
            }
            Ok(())
        };
        prune_ops().map_err(|err| OpStoreError::Other(err.into()))?;

        let prune_views = || -> Result<(), PathError> {
            let view_dir = self.views_dir();
            for entry in view_dir.read_dir().context(&view_dir)? {
                let entry = entry.context(&view_dir)?;
                let Some(id) = to_view_id(&entry) else {
                    tracing::trace!(?entry, "skipping invalid file name");
                    continue;
                };
                if reachable_views.contains(&id) {
                    continue;
                }
                remove_file_if_not_new(&entry)?;
            }
            Ok(())
        };
        prune_views().map_err(|err| OpStoreError::Other(err.into()))?;

        Ok(())
    }
}

fn io_to_read_error(err: PathError, id: &impl ObjectId) -> OpStoreError {
    if err.error.kind() == ErrorKind::NotFound {
        OpStoreError::ObjectNotFound {
            object_type: id.object_type(),
            hash: id.hex(),
            source: Box::new(err),
        }
    } else {
        to_read_error(err.into(), id)
    }
}

fn to_read_error(
    source: Box<dyn std::error::Error + Send + Sync>,
    id: &impl ObjectId,
) -> OpStoreError {
    OpStoreError::ReadObject {
        object_type: id.object_type(),
        hash: id.hex(),
        source,
    }
}

fn io_to_write_error(err: PathError, object_type: &'static str) -> OpStoreError {
    OpStoreError::WriteObject {
        object_type,
        source: Box::new(err),
    }
}

#[derive(Debug, Error)]
enum PostDecodeError {
    #[error("Invalid hash length (expected {expected} bytes, got {actual} bytes)")]
    InvalidHashLength { expected: usize, actual: usize },
}

fn operation_id_from_proto(bytes: Vec<u8>) -> Result<OperationId, PostDecodeError> {
    if bytes.len() != OPERATION_ID_LENGTH {
        Err(PostDecodeError::InvalidHashLength {
            expected: OPERATION_ID_LENGTH,
            actual: bytes.len(),
        })
    } else {
        Ok(OperationId::new(bytes))
    }
}

fn view_id_from_proto(bytes: Vec<u8>) -> Result<ViewId, PostDecodeError> {
    if bytes.len() != VIEW_ID_LENGTH {
        Err(PostDecodeError::InvalidHashLength {
            expected: VIEW_ID_LENGTH,
            actual: bytes.len(),
        })
    } else {
        Ok(ViewId::new(bytes))
    }
}

fn timestamp_to_proto(timestamp: &Timestamp) -> crate::protos::op_store::Timestamp {
    crate::protos::op_store::Timestamp {
        millis_since_epoch: timestamp.timestamp.0,
        tz_offset: timestamp.tz_offset,
    }
}

fn timestamp_from_proto(proto: crate::protos::op_store::Timestamp) -> Timestamp {
    Timestamp {
        timestamp: MillisSinceEpoch(proto.millis_since_epoch),
        tz_offset: proto.tz_offset,
    }
}

fn operation_metadata_to_proto(
    metadata: &OperationMetadata,
) -> crate::protos::op_store::OperationMetadata {
    crate::protos::op_store::OperationMetadata {
        start_time: Some(timestamp_to_proto(&metadata.time.start)),
        end_time: Some(timestamp_to_proto(&metadata.time.end)),
        description: metadata.description.clone(),
        hostname: metadata.hostname.clone(),
        username: metadata.username.clone(),
        is_snapshot: metadata.is_snapshot,
        tags: metadata.tags.clone(),
    }
}

fn operation_metadata_from_proto(
    proto: crate::protos::op_store::OperationMetadata,
) -> OperationMetadata {
    let time = TimestampRange {
        start: timestamp_from_proto(proto.start_time.unwrap_or_default()),
        end: timestamp_from_proto(proto.end_time.unwrap_or_default()),
    };
    OperationMetadata {
        time,
        description: proto.description,
        hostname: proto.hostname,
        username: proto.username,
        is_snapshot: proto.is_snapshot,
        tags: proto.tags,
    }
}

fn commit_predecessors_map_to_proto(
    map: &BTreeMap<CommitId, Vec<CommitId>>,
) -> Vec<crate::protos::op_store::CommitPredecessors> {
    map.iter()
        .map(
            |(commit_id, predecessor_ids)| crate::protos::op_store::CommitPredecessors {
                commit_id: commit_id.to_bytes(),
                predecessor_ids: predecessor_ids.iter().map(|id| id.to_bytes()).collect(),
            },
        )
        .collect()
}

fn commit_predecessors_map_from_proto(
    proto: Vec<crate::protos::op_store::CommitPredecessors>,
) -> BTreeMap<CommitId, Vec<CommitId>> {
    proto
        .into_iter()
        .map(|entry| {
            let commit_id = CommitId::new(entry.commit_id);
            let predecessor_ids = entry
                .predecessor_ids
                .into_iter()
                .map(CommitId::new)
                .collect();
            (commit_id, predecessor_ids)
        })
        .collect()
}

fn operation_to_proto(operation: &Operation) -> crate::protos::op_store::Operation {
    let (commit_predecessors, stores_commit_predecessors) = match &operation.commit_predecessors {
        Some(map) => (commit_predecessors_map_to_proto(map), true),
        None => (vec![], false),
    };
    let mut proto = crate::protos::op_store::Operation {
        view_id: operation.view_id.as_bytes().to_vec(),
        parents: Default::default(),
        metadata: Some(operation_metadata_to_proto(&operation.metadata)),
        commit_predecessors,
        stores_commit_predecessors,
    };
    for parent in &operation.parents {
        proto.parents.push(parent.to_bytes());
    }
    proto
}

fn operation_from_proto(
    proto: crate::protos::op_store::Operation,
) -> Result<Operation, PostDecodeError> {
    let parents = proto
        .parents
        .into_iter()
        .map(operation_id_from_proto)
        .try_collect()?;
    let view_id = view_id_from_proto(proto.view_id)?;
    let metadata = operation_metadata_from_proto(proto.metadata.unwrap_or_default());
    let commit_predecessors = proto
        .stores_commit_predecessors
        .then(|| commit_predecessors_map_from_proto(proto.commit_predecessors));
    Ok(Operation {
        view_id,
        parents,
        metadata,
        commit_predecessors,
    })
}

fn view_to_proto(view: &View) -> crate::protos::op_store::View {
    let mut proto = crate::protos::op_store::View {
        ..Default::default()
    };
    for (name, commit_id) in &view.wc_commit_ids {
        proto
            .wc_commit_ids
            .insert(name.into(), commit_id.to_bytes());
    }
    for head_id in &view.head_ids {
        proto.head_ids.push(head_id.to_bytes());
    }

    proto.bookmarks = bookmark_views_to_proto_legacy(&view.local_bookmarks, &view.remote_views);

    for (name, target) in &view.tags {
        proto.tags.push(crate::protos::op_store::Tag {
            name: name.into(),
            target: ref_target_to_proto(target),
        });
    }

    for (git_ref_name, target) in &view.git_refs {
        proto.git_refs.push(crate::protos::op_store::GitRef {
            name: git_ref_name.into(),
            target: ref_target_to_proto(target),
            ..Default::default()
        });
    }

    proto.git_head = ref_target_to_proto(&view.git_head);

    proto
}

fn view_from_proto(proto: crate::protos::op_store::View) -> View {
    // TODO: validate commit id length?
    let mut view = View::empty();
    // For compatibility with old repos before we had support for multiple working
    // copies
    #[expect(deprecated)]
    if !proto.wc_commit_id.is_empty() {
        view.wc_commit_ids.insert(
            WorkspaceName::DEFAULT.to_owned(),
            CommitId::new(proto.wc_commit_id),
        );
    }
    for (name, commit_id) in proto.wc_commit_ids {
        view.wc_commit_ids
            .insert(WorkspaceNameBuf::from(name), CommitId::new(commit_id));
    }
    for head_id_bytes in proto.head_ids {
        view.head_ids.insert(CommitId::new(head_id_bytes));
    }

    let (local_bookmarks, remote_views) = bookmark_views_from_proto_legacy(proto.bookmarks);
    view.local_bookmarks = local_bookmarks;
    view.remote_views = remote_views;

    for tag_proto in proto.tags {
        let name: RefNameBuf = tag_proto.name.into();
        view.tags
            .insert(name, ref_target_from_proto(tag_proto.target));
    }

    for git_ref in proto.git_refs {
        let name: GitRefNameBuf = git_ref.name.into();
        let target = if git_ref.target.is_some() {
            ref_target_from_proto(git_ref.target)
        } else {
            // Legacy format
            RefTarget::normal(CommitId::new(git_ref.commit_id))
        };
        view.git_refs.insert(name, target);
    }

    #[expect(deprecated)]
    if proto.git_head.is_some() {
        view.git_head = ref_target_from_proto(proto.git_head);
    } else if !proto.git_head_legacy.is_empty() {
        view.git_head = RefTarget::normal(CommitId::new(proto.git_head_legacy));
    }

    view
}

fn bookmark_views_to_proto_legacy(
    local_bookmarks: &BTreeMap<RefNameBuf, RefTarget>,
    remote_views: &BTreeMap<RemoteNameBuf, RemoteView>,
) -> Vec<crate::protos::op_store::Bookmark> {
    op_store::merge_join_bookmark_views(local_bookmarks, remote_views)
        .map(|(name, bookmark_target)| {
            let local_target = ref_target_to_proto(bookmark_target.local_target);
            let remote_bookmarks = bookmark_target
                .remote_refs
                .iter()
                .map(
                    |&(remote_name, remote_ref)| crate::protos::op_store::RemoteBookmark {
                        remote_name: remote_name.into(),
                        target: ref_target_to_proto(&remote_ref.target),
                        state: remote_ref_state_to_proto(remote_ref.state),
                    },
                )
                .collect();
            crate::protos::op_store::Bookmark {
                name: name.into(),
                local_target,
                remote_bookmarks,
            }
        })
        .collect()
}

fn bookmark_views_from_proto_legacy(
    bookmarks_legacy: Vec<crate::protos::op_store::Bookmark>,
) -> (
    BTreeMap<RefNameBuf, RefTarget>,
    BTreeMap<RemoteNameBuf, RemoteView>,
) {
    let mut local_bookmarks: BTreeMap<RefNameBuf, RefTarget> = BTreeMap::new();
    let mut remote_views: BTreeMap<RemoteNameBuf, RemoteView> = BTreeMap::new();
    for bookmark_proto in bookmarks_legacy {
        let bookmark_name: RefNameBuf = bookmark_proto.name.into();
        let local_target = ref_target_from_proto(bookmark_proto.local_target);
        for remote_bookmark in bookmark_proto.remote_bookmarks {
            let remote_name: RemoteNameBuf = remote_bookmark.remote_name.into();
            let state = remote_ref_state_from_proto(remote_bookmark.state).unwrap_or_else(|| {
                // If local bookmark doesn't exist, we assume that the remote bookmark hasn't
                // been merged because git.auto-local-bookmark was off. That's
                // probably more common than deleted but yet-to-be-pushed local
                // bookmark. Alternatively, we could read
                // git.auto-local-bookmark setting here, but that wouldn't always work since the
                // setting could be toggled after the bookmark got merged.
                let is_git_tracking = crate::git::is_special_git_remote(&remote_name);
                let default_state = if is_git_tracking || local_target.is_present() {
                    RemoteRefState::Tracked
                } else {
                    RemoteRefState::New
                };
                tracing::trace!(
                    ?bookmark_name,
                    ?remote_name,
                    ?default_state,
                    "generated tracking state",
                );
                default_state
            });
            let remote_view = remote_views.entry(remote_name).or_default();
            let remote_ref = RemoteRef {
                target: ref_target_from_proto(remote_bookmark.target),
                state,
            };
            remote_view
                .bookmarks
                .insert(bookmark_name.clone(), remote_ref);
        }
        if local_target.is_present() {
            local_bookmarks.insert(bookmark_name, local_target);
        }
    }
    (local_bookmarks, remote_views)
}

fn ref_target_to_proto(value: &RefTarget) -> Option<crate::protos::op_store::RefTarget> {
    let term_to_proto = |term: &Option<CommitId>| crate::protos::op_store::ref_conflict::Term {
        value: term.as_ref().map(|id| id.to_bytes()),
    };
    let merge = value.as_merge();
    let conflict_proto = crate::protos::op_store::RefConflict {
        removes: merge.removes().map(term_to_proto).collect(),
        adds: merge.adds().map(term_to_proto).collect(),
    };
    let proto = crate::protos::op_store::RefTarget {
        value: Some(crate::protos::op_store::ref_target::Value::Conflict(
            conflict_proto,
        )),
    };
    Some(proto)
}

#[expect(deprecated)]
#[cfg(test)]
fn ref_target_to_proto_legacy(value: &RefTarget) -> Option<crate::protos::op_store::RefTarget> {
    if let Some(id) = value.as_normal() {
        let proto = crate::protos::op_store::RefTarget {
            value: Some(crate::protos::op_store::ref_target::Value::CommitId(
                id.to_bytes(),
            )),
        };
        Some(proto)
    } else if value.has_conflict() {
        let ref_conflict_proto = crate::protos::op_store::RefConflictLegacy {
            removes: value.removed_ids().map(|id| id.to_bytes()).collect(),
            adds: value.added_ids().map(|id| id.to_bytes()).collect(),
        };
        let proto = crate::protos::op_store::RefTarget {
            value: Some(crate::protos::op_store::ref_target::Value::ConflictLegacy(
                ref_conflict_proto,
            )),
        };
        Some(proto)
    } else {
        assert!(value.is_absent());
        None
    }
}

fn ref_target_from_proto(maybe_proto: Option<crate::protos::op_store::RefTarget>) -> RefTarget {
    // TODO: Delete legacy format handling when we decide to drop support for views
    // saved by jj <= 0.8.
    let Some(proto) = maybe_proto else {
        // Legacy absent id
        return RefTarget::absent();
    };
    match proto.value.unwrap() {
        crate::protos::op_store::ref_target::Value::CommitId(id) => {
            // Legacy non-conflicting id
            RefTarget::normal(CommitId::new(id))
        }
        #[expect(deprecated)]
        crate::protos::op_store::ref_target::Value::ConflictLegacy(conflict) => {
            // Legacy conflicting ids
            let removes = conflict.removes.into_iter().map(CommitId::new);
            let adds = conflict.adds.into_iter().map(CommitId::new);
            RefTarget::from_legacy_form(removes, adds)
        }
        crate::protos::op_store::ref_target::Value::Conflict(conflict) => {
            let term_from_proto =
                |term: crate::protos::op_store::ref_conflict::Term| term.value.map(CommitId::new);
            let removes = conflict.removes.into_iter().map(term_from_proto);
            let adds = conflict.adds.into_iter().map(term_from_proto);
            RefTarget::from_merge(Merge::from_removes_adds(removes, adds))
        }
    }
}

fn remote_ref_state_to_proto(state: RemoteRefState) -> Option<i32> {
    let proto_state = match state {
        RemoteRefState::New => crate::protos::op_store::RemoteRefState::New,
        RemoteRefState::Tracked => crate::protos::op_store::RemoteRefState::Tracked,
    };
    Some(proto_state as i32)
}

fn remote_ref_state_from_proto(proto_value: Option<i32>) -> Option<RemoteRefState> {
    let proto_state = proto_value?.try_into().ok()?;
    let state = match proto_state {
        crate::protos::op_store::RemoteRefState::New => RemoteRefState::New,
        crate::protos::op_store::RemoteRefState::Tracked => RemoteRefState::Tracked,
    };
    Some(state)
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use itertools::Itertools as _;
    use maplit::btreemap;
    use maplit::hashmap;
    use maplit::hashset;

    use super::*;
    use crate::hex_util;
    use crate::tests::new_temp_dir;

    fn create_view() -> View {
        let new_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::New,
        };
        let tracked_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        let head_id1 = CommitId::from_hex("aaa111");
        let head_id2 = CommitId::from_hex("aaa222");
        let bookmark_main_local_target = RefTarget::normal(CommitId::from_hex("ccc111"));
        let bookmark_main_origin_target = RefTarget::normal(CommitId::from_hex("ccc222"));
        let bookmark_deleted_origin_target = RefTarget::normal(CommitId::from_hex("ccc333"));
        let tag_v1_target = RefTarget::normal(CommitId::from_hex("ddd111"));
        let git_refs_main_target = RefTarget::normal(CommitId::from_hex("fff111"));
        let git_refs_feature_target = RefTarget::from_legacy_form(
            [CommitId::from_hex("fff111")],
            [CommitId::from_hex("fff222"), CommitId::from_hex("fff333")],
        );
        let default_wc_commit_id = CommitId::from_hex("abc111");
        let test_wc_commit_id = CommitId::from_hex("abc222");
        View {
            head_ids: hashset! {head_id1, head_id2},
            local_bookmarks: btreemap! {
                "main".into() => bookmark_main_local_target,
            },
            tags: btreemap! {
                "v1.0".into() => tag_v1_target,
            },
            remote_views: btreemap! {
                "origin".into() => RemoteView {
                    bookmarks: btreemap! {
                        "main".into() => tracked_remote_ref(&bookmark_main_origin_target),
                        "deleted".into() => new_remote_ref(&bookmark_deleted_origin_target),
                    },
                },
            },
            git_refs: btreemap! {
                "refs/heads/main".into() => git_refs_main_target,
                "refs/heads/feature".into() => git_refs_feature_target,
            },
            git_head: RefTarget::normal(CommitId::from_hex("fff111")),
            wc_commit_ids: btreemap! {
                WorkspaceName::DEFAULT.to_owned() => default_wc_commit_id,
                "test".into() => test_wc_commit_id,
            },
        }
    }

    fn create_operation() -> Operation {
        let pad_id_bytes = |hex: &str, len: usize| {
            let mut bytes = hex_util::decode_hex(hex).unwrap();
            bytes.resize(len, b'\0');
            bytes
        };
        Operation {
            view_id: ViewId::new(pad_id_bytes("aaa111", VIEW_ID_LENGTH)),
            parents: vec![
                OperationId::new(pad_id_bytes("bbb111", OPERATION_ID_LENGTH)),
                OperationId::new(pad_id_bytes("bbb222", OPERATION_ID_LENGTH)),
            ],
            metadata: OperationMetadata {
                time: TimestampRange {
                    start: Timestamp {
                        timestamp: MillisSinceEpoch(123456789),
                        tz_offset: 3600,
                    },
                    end: Timestamp {
                        timestamp: MillisSinceEpoch(123456800),
                        tz_offset: 3600,
                    },
                },
                description: "check out foo".to_string(),
                hostname: "some.host.example.com".to_string(),
                username: "someone".to_string(),
                is_snapshot: false,
                tags: hashmap! {
                    "key1".to_string() => "value1".to_string(),
                    "key2".to_string() => "value2".to_string(),
                },
            },
            commit_predecessors: Some(btreemap! {
                CommitId::from_hex("111111") => vec![],
                CommitId::from_hex("222222") => vec![
                    CommitId::from_hex("333333"),
                    CommitId::from_hex("444444"),
                ],
            }),
        }
    }

    #[test]
    fn test_hash_view() {
        // Test exact output so we detect regressions in compatibility
        assert_snapshot!(
            ViewId::new(blake2b_hash(&create_view()).to_vec()).hex(),
            @"f426676b3a2f7c6b9ec8677cb05ed249d0d244ab7e86a7c51117e2d8a4829db65e55970c761231e2107d303bf3d33a1f2afdd4ed2181f223e99753674b20a35e"
        );
    }

    #[test]
    fn test_hash_operation() {
        // Test exact output so we detect regressions in compatibility
        assert_snapshot!(
            OperationId::new(blake2b_hash(&create_operation()).to_vec()).hex(),
            @"b544c80b5ededdd64d0f10468fa636a06b83c45d94dd9bdac95319f7fe11fee536506c5c110681dee6233e69db7647683e732939a3ec88e867250efd765fea18"
        );
    }

    #[test]
    fn test_read_write_view() {
        let temp_dir = new_temp_dir();
        let root_data = RootOperationData {
            root_commit_id: CommitId::from_hex("000000"),
        };
        let store = SimpleOpStore::init(temp_dir.path(), root_data).unwrap();
        let view = create_view();
        let view_id = store.write_view(&view).unwrap();
        let read_view = store.read_view(&view_id).unwrap();
        assert_eq!(read_view, view);
    }

    #[test]
    fn test_read_write_operation() {
        let temp_dir = new_temp_dir();
        let root_data = RootOperationData {
            root_commit_id: CommitId::from_hex("000000"),
        };
        let store = SimpleOpStore::init(temp_dir.path(), root_data).unwrap();
        let operation = create_operation();
        let op_id = store.write_operation(&operation).unwrap();
        let read_operation = store.read_operation(&op_id).unwrap();
        assert_eq!(read_operation, operation);
    }

    #[test]
    fn test_bookmark_views_legacy_roundtrip() {
        let new_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::New,
        };
        let tracked_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        let local_bookmark1_target = RefTarget::normal(CommitId::from_hex("111111"));
        let local_bookmark3_target = RefTarget::normal(CommitId::from_hex("222222"));
        let git_bookmark1_target = RefTarget::normal(CommitId::from_hex("333333"));
        let remote1_bookmark1_target = RefTarget::normal(CommitId::from_hex("444444"));
        let remote2_bookmark2_target = RefTarget::normal(CommitId::from_hex("555555"));
        let remote2_bookmark4_target = RefTarget::normal(CommitId::from_hex("666666"));
        let local_bookmarks = btreemap! {
            "bookmark1".into() => local_bookmark1_target.clone(),
            "bookmark3".into() => local_bookmark3_target.clone(),
        };
        let remote_views = btreemap! {
            "git".into() => RemoteView {
                bookmarks: btreemap! {
                    "bookmark1".into() => tracked_remote_ref(&git_bookmark1_target),
                },
            },
            "remote1".into() => RemoteView {
                bookmarks: btreemap! {
                    "bookmark1".into() => tracked_remote_ref(&remote1_bookmark1_target),
                },
            },
            "remote2".into() => RemoteView {
                bookmarks: btreemap! {
                    // "bookmark2" is non-tracking. "bookmark4" is tracking, but locally deleted.
                    "bookmark2".into() => new_remote_ref(&remote2_bookmark2_target),
                    "bookmark4".into() => tracked_remote_ref(&remote2_bookmark4_target),
                },
            },
        };

        let bookmarks_legacy = bookmark_views_to_proto_legacy(&local_bookmarks, &remote_views);
        assert_eq!(
            bookmarks_legacy
                .iter()
                .map(|proto| &proto.name)
                .sorted()
                .collect_vec(),
            vec!["bookmark1", "bookmark2", "bookmark3", "bookmark4"],
        );

        let (local_bookmarks_reconstructed, remote_views_reconstructed) =
            bookmark_views_from_proto_legacy(bookmarks_legacy);
        assert_eq!(local_bookmarks_reconstructed, local_bookmarks);
        assert_eq!(remote_views_reconstructed, remote_views);
    }

    #[test]
    fn test_ref_target_change_delete_order_roundtrip() {
        let target = RefTarget::from_merge(Merge::from_removes_adds(
            vec![Some(CommitId::from_hex("111111"))],
            vec![Some(CommitId::from_hex("222222")), None],
        ));
        let maybe_proto = ref_target_to_proto(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // If it were legacy format, order of None entry would be lost.
        let target = RefTarget::from_merge(Merge::from_removes_adds(
            vec![Some(CommitId::from_hex("111111"))],
            vec![None, Some(CommitId::from_hex("222222"))],
        ));
        let maybe_proto = ref_target_to_proto(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);
    }

    #[test]
    fn test_ref_target_legacy_roundtrip() {
        let target = RefTarget::absent();
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        let target = RefTarget::normal(CommitId::from_hex("111111"));
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // N-way conflict
        let target = RefTarget::from_legacy_form(
            [CommitId::from_hex("111111"), CommitId::from_hex("222222")],
            [
                CommitId::from_hex("333333"),
                CommitId::from_hex("444444"),
                CommitId::from_hex("555555"),
            ],
        );
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // Change-delete conflict
        let target = RefTarget::from_legacy_form(
            [CommitId::from_hex("111111")],
            [CommitId::from_hex("222222")],
        );
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);
    }
}

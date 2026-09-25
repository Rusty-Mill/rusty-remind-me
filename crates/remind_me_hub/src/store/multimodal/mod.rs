//! Hub storage on the embedded `rusty_multimodal_db` engine (ADR-0021).
//!
//! The hub's third backend: no database server and no SQL. Four engine
//! stores, one per table, live in one data directory, with every record
//! in memory and each write logged and `fsync`'d before it returns. The
//! wire behaviour is the SQLite store's, method for method; the route
//! suite runs against both.
//!
//! # Where it differs from the SQL stores
//!
//! - **Ids are capped at 64 bytes and may not contain NUL** (ADR-0021,
//!   decision 3). A longer id fails the record as a storage error, so it
//!   counts as `failed` and stays in the sender's outbox.
//! - **One lock, owned here** (decision 5). Every write holds it through
//!   its `fsync`, and pulls wait on it; SQLite in WAL mode let reads run
//!   alongside a write. A push is applied in chunks of [`GROUP_COMMIT`]
//!   records, each under one hold of the lock and one sync per table, so a
//!   pull waits for at most one chunk. A panic while it is held does not
//!   take the hub down with it: the lock recovers, as the SQLite store's
//!   mutex does.
//! - **A failed sync stops every later write** until the hub restarts. The
//!   chunk it covered is applied in memory but may not be on disk, so
//!   carrying on could acknowledge writes built on ones a crash loses.
//!   [`HubStore::ping`] then fails, so `/health` reports it.
//! - **`hub_seq` never goes backwards,** even across a compaction that
//!   deletes the row holding the highest one. The counter lives in memory
//!   and restarts from the highest `hub_seq` stored, so before deleting any
//!   memory, [`HubStore::compact_tombstones`] records the counter in a
//!   floor file the next open starts above.
//! - **The insert logs grow until compaction.** [`MultimodalHubStore::compact`]
//!   folds them; the binary runs it on a schedule, and the tombstone
//!   compaction route runs it too.
//! - **A cursor timestamp is read as an instant.** A `since` that is not
//!   canonical is canonicalised; one that is not a timestamp at all is a
//!   storage error. The SQL stores compare cursor bytes as sent.

mod keys;
mod rows;
pub mod snapshot;

use super::{
    stable_group_order, Counts, GraphPullQuery, HubStore, MemoryCounts, PullCursor, PullQuery,
    Stats, StoreError, StoreResult, NO_CATEGORY, UNATTRIBUTED,
};
use crate::canon::now_canonical;
use crate::record::{EntityRecord, LinkRecord, MemoryRecord, Record};
use keys::{IdKey, LinkKey, MAX_ID_KEY};
use rows::{ByEngineId, EntityRow, Keyset, LinkRow, MemoryRow, Micros, RelationRow, Seq};
use rusty_multimodal_db_engine::durability::{sync_parent_dir, DurabilityError};
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::query::{
    AllIds, Compact, Delete, GetById, Insert, PageBy, RangeBy, Replace,
};
use rusty_multimodal_db_engine::generic::store::{GroupCommit, Ordered};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, OrderedField, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use uuid::Uuid;

pub use keys::ID_CAP;

type Core<R, Slot> = GenericMmapStore<R, ByEngineId, Slot>;
type MemoryTable = Ordered<Ordered<Core<MemoryRow, Seq>, MemoryRow, Keyset>, MemoryRow, Seq>;
type EntityTable = Ordered<Core<EntityRow, Micros>, EntityRow, Keyset>;
type LinkTable = Ordered<Core<LinkRow, Micros>, LinkRow, Keyset>;
type RelationTable = Ordered<Core<RelationRow, Micros>, RelationRow, Keyset>;

/// The greatest engine id, so a cursor `(key, UUID_MAX)` excludes every
/// row at `key` itself. A v5 id can never be all ones (its version bits).
const UUID_MAX: Uuid = Uuid::from_u128(u128::MAX);
/// The greatest link key, as [`MAX_ID_KEY`] is for ids.
const MAX_LINK_KEY: LinkKey = [0xFF; keys::LINK_KEY_LEN];

/// The file inside the data directory that marks it in use.
const LOCK_FILE: &str = "hub.lock";
/// The file holding the highest `hub_seq` ever issued before a deletion.
const SEQ_FLOOR_FILE: &str = "hub_seq.floor";

/// Records applied per group commit. Large enough that a push's syncs stop
/// dominating it, small enough that a pull waiting on the lock is not held
/// up for a whole 1000-record push (`examples/pull_latency.rs`).
pub const GROUP_COMMIT: usize = 64;

/// Everything behind the lock.
struct Tables {
    memories: MemoryTable,
    entities: EntityTable,
    links: LinkTable,
    relations: RelationTable,
    /// The `hub_seq` the next applied memory write gets.
    next_seq: i64,
    /// Writes since the last compaction, so a scheduled compaction with
    /// nothing to fold is free.
    writes_since_compact: u64,
    /// Why a group commit's sync failed, once one has: every later write
    /// is refused with it (see the module docs).
    sync_failure: Option<StoreError>,
}

impl Tables {
    fn defer_sync(&mut self) {
        self.memories.defer_sync();
        self.entities.defer_sync();
        self.links.defer_sync();
        self.relations.defer_sync();
    }

    /// Sync all four tables, each tried even if an earlier one failed, so
    /// none is left deferring its syncs.
    fn commit(&mut self) -> Result<(), DurabilityError> {
        let results = [
            self.memories.commit(),
            self.entities.commit(),
            self.links.commit(),
            self.relations.commit(),
        ];
        results.into_iter().collect()
    }

    /// Record a failed sync, which refuses every later write, and return
    /// the error each write gets.
    fn sync_failed(&mut self, e: DurabilityError) -> StoreError {
        let failure = StoreError(format!(
            "could not sync the engine store, so it refuses writes until the hub restarts: {e}"
        ));
        self.sync_failure = Some(failure.clone());
        failure
    }
}

/// A hub store in one data directory.
pub struct MultimodalHubStore {
    dir: PathBuf,
    tables: RwLock<Tables>,
    /// Writers queue here before asking for the write lock, so at most one
    /// writer ever waits on `tables`. `std`'s `RwLock` lets a waiting writer
    /// go ahead of waiting readers; with several pushes queued on it, a
    /// pull would wait for every one of them, and under steady pushes it
    /// never got in (a pull p50 of 10 s in `examples/pull_latency.rs`).
    writers: Mutex<()>,
    /// Held for the store's lifetime: its OS lock is what keeps a second
    /// hub off the same directory (ADR-0021, Consequences: there is no
    /// separate server to take `rusty_multimodal_db`'s ADR-0092 lock).
    _dir_lock: File,
}

fn engine_err(e: impl std::fmt::Display) -> StoreError {
    StoreError(e.to_string())
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> StoreError {
    StoreError(format!("{what} {}: {e}", path.display()))
}

/// Open the engine store at `path`, or create an empty one there.
fn open_core<R, Slot>(path: &Path) -> Result<Core<R, Slot>, DurabilityError>
where
    R: IndexedField<ByEngineId, Id = Uuid>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::ScanValue: MmapFieldValue,
{
    if path.exists() {
        Core::open_portable(path)
    } else {
        Core::create(Vec::new(), path)
    }
}

impl MultimodalHubStore {
    /// Open (or create) a hub in `dir`.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created, another process holds it,
    /// or a table's files cannot be read (including one written under a
    /// different record layout, which the engine refuses by name).
    pub fn open(dir: &Path) -> StoreResult<Self> {
        std::fs::create_dir_all(dir).map_err(|e| io_err("could not create", dir, e))?;
        let dir_lock = take_dir_lock(dir)?;

        let path = |name: &str| table_path(dir, name);
        let memories = open_core(&path(MEMORIES)).map_err(engine_err)?;
        let entities = open_core(&path(ENTITIES)).map_err(engine_err)?;
        let links = open_core(&path(LINKS)).map_err(engine_err)?;
        let relations = open_core(&path(RELATIONS)).map_err(engine_err)?;
        Self::assemble(dir, dir_lock, memories, entities, links, relations)
    }

    /// Create a hub in `dir` holding `snapshot`, every row written in one
    /// pass with its `hub_seq` and `origin_node` kept exactly (ADR-0021,
    /// decision 7). The next `hub_seq` issued is above both the highest
    /// copied and the source's own high-water mark.
    ///
    /// `dir` must not exist or must be empty: this is how a hub moves onto
    /// the engine, never a merge into one.
    ///
    /// # Errors
    ///
    /// Fails if `dir` holds anything, if the snapshot has a row
    /// [`snapshot::Snapshot::validate`] rejects (the copy tool drops those
    /// first, or refuses), or on any I/O error.
    pub fn create_from_snapshot(dir: &Path, snapshot: &snapshot::Snapshot) -> StoreResult<Self> {
        if let Some(rejected) = snapshot.validate().first() {
            return Err(StoreError(format!(
                "the snapshot holds a row the engine cannot store: {rejected}"
            )));
        }
        ensure_empty_dir(dir)?;
        std::fs::create_dir_all(dir).map_err(|e| io_err("could not create", dir, e))?;
        let dir_lock = take_dir_lock(dir)?;
        write_seq_floor(dir, snapshot.seq_floor())?;

        let rows = snapshot.rows()?;
        let path = |name: &str| table_path(dir, name);
        let memories = Core::create(rows.memories, &path(MEMORIES)).map_err(engine_err)?;
        let entities = Core::create(rows.entities, &path(ENTITIES)).map_err(engine_err)?;
        let links = Core::create(rows.links, &path(LINKS)).map_err(engine_err)?;
        let relations = Core::create(rows.relations, &path(RELATIONS)).map_err(engine_err)?;
        Self::assemble(dir, dir_lock, memories, entities, links, relations)
    }

    /// Wrap the four open engine stores in their sort orders and start the
    /// `hub_seq` counter above everything issued so far.
    fn assemble(
        dir: &Path,
        dir_lock: File,
        memories: Core<MemoryRow, Seq>,
        entities: Core<EntityRow, Micros>,
        links: Core<LinkRow, Micros>,
        relations: Core<RelationRow, Micros>,
    ) -> StoreResult<Self> {
        let memories: MemoryTable = Ordered::new(Ordered::new(memories));
        let stored_max = PageBy::<MemoryRow, Seq>::page_by_desc(&memories, None, 1)
            .first()
            .and_then(|id| memories.get(*id))
            .map_or(0, |row| row.hub_seq);
        let next_seq = stored_max.max(read_seq_floor(dir)?) + 1;

        Ok(Self {
            dir: dir.to_path_buf(),
            tables: RwLock::new(Tables {
                memories,
                entities: Ordered::new(entities),
                links: Ordered::new(links),
                relations: Ordered::new(relations),
                next_seq,
                writes_since_compact: 0,
                sync_failure: None,
            }),
            writers: Mutex::new(()),
            _dir_lock: dir_lock,
        })
    }

    /// Fold every table's insert log into its record blob, if anything was
    /// written since the last time. Returns whether it compacted.
    ///
    /// Holds the write lock for the whole run, so pushes and pulls wait on
    /// it. Safe to interrupt: the engine's compaction is crash-safe step by
    /// step.
    pub fn compact(&self) -> StoreResult<bool> {
        let mut tables = self.write()?;
        compact_tables(&mut tables)
    }

    fn read(&self) -> RwLockReadGuard<'_, Tables> {
        self.tables.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The write lock, recovered after a panic (ADR-0021, decision 5): one
    /// panicking request must not make every later request panic too.
    ///
    /// Taken behind the writer queue: see [`Self::writers`]. Refused after
    /// a failed sync (see the module docs).
    fn write(&self) -> StoreResult<WriteGuard<'_>> {
        let queued = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        let mut guard = WriteGuard {
            tables: self.tables.write().unwrap_or_else(PoisonError::into_inner),
            _queued: queued,
        };
        if let Some(failure) = &guard.sync_failure {
            return Err(failure.clone());
        }
        // A writer that panicked inside a group commit left it open. Close
        // it, syncing what that writer wrote, so every writer starts with
        // each write synced. With nothing pending this syncs nothing.
        if let Err(e) = guard.commit() {
            return Err(guard.sync_failed(e));
        }
        Ok(guard)
    }

    /// Apply `chunk` under one hold of the write lock and one sync per
    /// table.
    fn apply_chunk(&self, chunk: &[Record], origin: Option<&str>) -> Vec<StoreResult<bool>> {
        let mut t = match self.write() {
            Ok(t) => t,
            Err(e) => return vec![Err(e); chunk.len()],
        };
        t.defer_sync();
        let results = chunk
            .iter()
            .map(|record| apply_one(&mut t, record, origin))
            .collect();
        match t.commit() {
            Ok(()) => results,
            Err(e) => vec![Err(t.sync_failed(e)); chunk.len()],
        }
    }
}

/// The write lock and the writer-queue place it was taken behind. Fields
/// drop in order, so the write lock goes first and a pull waiting on it
/// gets in before the next writer can ask for it.
struct WriteGuard<'a> {
    tables: RwLockWriteGuard<'a, Tables>,
    _queued: MutexGuard<'a, ()>,
}

impl std::ops::Deref for WriteGuard<'_> {
    type Target = Tables;
    fn deref(&self) -> &Tables {
        &self.tables
    }
}

impl std::ops::DerefMut for WriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Tables {
        &mut self.tables
    }
}

const MEMORIES: &str = "memories";
const ENTITIES: &str = "entities";
const LINKS: &str = "memory_entities";
const RELATIONS: &str = "entity_relations";

fn table_path(dir: &Path, table: &str) -> PathBuf {
    dir.join(format!("{table}.mmap"))
}

impl MultimodalHubStore {
    /// Refuse a copy target that already holds anything, before anything
    /// is read: [`Self::create_from_snapshot`] refuses it too, but only
    /// after the whole source has been read.
    ///
    /// # Errors
    ///
    /// Fails if `dir` holds anything or cannot be read.
    pub fn check_copy_target(dir: &Path) -> StoreResult<()> {
        ensure_empty_dir(dir)
    }
}

/// Refuse a target directory that already holds anything.
fn ensure_empty_dir(dir: &Path) -> StoreResult<()> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => match entries.next() {
            None => Ok(()),
            Some(_) => Err(StoreError(format!(
                "{} is not empty; a hub is only ever copied into an empty directory",
                dir.display()
            ))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("could not read", dir, e)),
    }
}

/// Take the data directory's OS lock without waiting.
fn take_dir_lock(dir: &Path) -> StoreResult<File> {
    let path = dir.join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io_err("could not open", &path, e))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => StoreError(format!(
            "{} is in use by another hub process",
            dir.display()
        )),
        std::fs::TryLockError::Error(e) => io_err("could not lock", &path, e),
    })?;
    Ok(file)
}

fn read_seq_floor(dir: &Path) -> StoreResult<i64> {
    let path = dir.join(SEQ_FLOOR_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| StoreError(format!("{} is not a number: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(io_err("could not read", &path, e)),
    }
}

/// Durably record that `hub_seq`s up to `floor` have been issued: written
/// aside, synced, renamed over, and the directory synced.
fn write_seq_floor(dir: &Path, floor: i64) -> StoreResult<()> {
    let path = dir.join(SEQ_FLOOR_FILE);
    let staging = dir.join(format!("{SEQ_FLOOR_FILE}.tmp"));
    let mut file = File::create(&staging).map_err(|e| io_err("could not create", &staging, e))?;
    file.write_all(floor.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| io_err("could not write", &staging, e))?;
    std::fs::rename(&staging, &path).map_err(|e| io_err("could not install", &path, e))?;
    sync_parent_dir(&path).map_err(|e| io_err("could not sync the directory of", &path, e))
}

fn compact_tables(tables: &mut Tables) -> StoreResult<bool> {
    if tables.writes_since_compact == 0 {
        return Ok(false);
    }
    tables.memories.compact().map_err(engine_err)?;
    tables.entities.compact().map_err(engine_err)?;
    tables.links.compact().map_err(engine_err)?;
    tables.relations.compact().map_err(engine_err)?;
    tables.writes_since_compact = 0;
    Ok(true)
}

/// Up to `limit` rows of one sort order after `after`, keeping those
/// `keep` accepts. Walks further pages as needed, so a filter never
/// shortens a page that has more matching rows behind it.
fn page_where<L, R, M>(
    layer: &L,
    mut after: Option<(R::Key, Uuid)>,
    limit: usize,
    keep: impl Fn(&R) -> bool,
) -> StoreResult<Vec<R>>
where
    L: PageBy<R, M> + GetById<R>,
    R: OrderedField<M, Id = Uuid>,
{
    let mut out = Vec::new();
    loop {
        let ids = layer.page_by(after, limit);
        for id in &ids {
            let row = layer.get(*id).ok_or_else(|| {
                StoreError(format!("a sort index names {id}, which no record holds"))
            })?;
            after = Some((<R as OrderedField<M>>::order_key(&row), *id));
            if keep(&row) {
                out.push(row);
                if out.len() == limit {
                    return Ok(out);
                }
            }
        }
        if ids.len() < limit {
            return Ok(out);
        }
    }
}

/// The lower bound "strictly after every row at `key`".
fn past<K>(key: K) -> Bound<(K, Uuid)> {
    Bound::Excluded((key, UUID_MAX))
}

/// Every row of one sort order strictly after `after`.
fn rows_after<L, R, M>(layer: &L, after: (R::Key, Uuid)) -> Vec<R>
where
    L: RangeBy<R, M> + GetById<R>,
    R: OrderedField<M, Id = Uuid>,
{
    layer
        .range_by(Bound::Excluded(after), Bound::Unbounded)
        .into_iter()
        .filter_map(|id| layer.get(id))
        .collect()
}

/// Every row of a table, in no particular order.
fn all_rows<S, R>(table: &S) -> Vec<R>
where
    S: AllIds<R> + GetById<R>,
    R: rusty_multimodal_db_engine::generic::traits::Record,
{
    table
        .all_ids()
        .into_iter()
        .filter_map(|id| table.get(id))
        .collect()
}

/// The keyset cursor `(timestamp, id)` for one of the `IdKey` tables:
/// strictly after every row at `since` whose id is `<= since_id`.
fn id_cursor(since: &str, since_id: &str) -> StoreResult<((i64, IdKey), Uuid)> {
    Ok(((keys::micros(since)?, keys::id_key(since_id)), UUID_MAX))
}

/// The cursor "strictly after `since`" for one of the `IdKey` tables.
fn after_instant(since: &str) -> StoreResult<((i64, IdKey), Uuid)> {
    Ok(((keys::micros(since)?, MAX_ID_KEY), UUID_MAX))
}

/// Group rows by a label, with the reference's fallback for an empty one.
fn group<'a>(labels: impl Iterator<Item = Option<&'a str>>, fallback: &str) -> Vec<(String, i64)> {
    let mut groups: BTreeMap<String, i64> = BTreeMap::new();
    for label in labels {
        let label = label.filter(|l| !l.is_empty()).unwrap_or(fallback);
        *groups.entry(label.to_string()).or_default() += 1;
    }
    stable_group_order(groups)
}

/// Keep a row unless `exclude_node` names the node that pushed it.
fn not_excluded<'a>(query: &'a PullQuery) -> impl Fn(Option<&str>) -> bool + 'a {
    move |origin| match (&query.exclude_node, query.full) {
        (Some(node), false) => origin != Some(node.as_str()),
        _ => true,
    }
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn apply_memory(t: &mut Tables, m: &MemoryRecord, origin: Option<&str>) -> StoreResult<bool> {
    keys::check_id("memory id", &m.id)?;
    let mut row = MemoryRow::new(m, origin, t.next_seq)?;
    match t.memories.get(row.engine_id) {
        None => t.memories.insert(row).map_err(engine_err)?,
        Some(local) => {
            keys::ensure_same_id(&local.id, &m.id)?;
            // LWW, exactly the SQL upsert's `WHERE excluded.updated_at >
            // memories.updated_at`: canonical timestamps compare as bytes.
            if m.updated_at <= local.updated_at {
                return Ok(false);
            }
            // The SQL upsert's SET list leaves `created_at` alone.
            row.created_at = local.created_at;
            t.memories.replace(row).map_err(engine_err)?;
        }
    }
    t.next_seq += 1;
    Ok(true)
}

/// Entity upsert: LWW on `updated_at`, aliases always union-merged. The
/// SQLite store's `apply_entity`, rule for rule; see it for why an
/// LWW-losing enrichment bumps `updated_at`.
fn apply_entity(t: &mut Tables, e: &EntityRecord, origin: Option<&str>) -> StoreResult<bool> {
    keys::check_id("entity id", &e.id)?;
    let incoming = EntityRow::new(e, origin)?;
    let Some(local) = t.entities.get(incoming.engine_id) else {
        t.entities.insert(incoming).map_err(engine_err)?;
        return Ok(true);
    };
    keys::ensure_same_id(&local.id, &e.id)?;

    let mut merged = local.aliases.clone();
    for alias in &e.aliases {
        if !merged.contains(alias) {
            merged.push(alias.clone());
        }
    }

    if e.updated_at > local.updated_at {
        let row = EntityRow {
            kind: e.kind.clone().or(local.kind),
            aliases: merged,
            created_at: local.created_at,
            ..incoming
        };
        t.entities.replace(row).map_err(engine_err)?;
        return Ok(true);
    }

    let fill_kind = local.kind.clone().or_else(|| e.kind.clone());
    if merged == local.aliases && fill_kind == local.kind {
        return Ok(false);
    }
    let updated_at = now_canonical();
    let row = EntityRow {
        kind: fill_kind,
        aliases: merged,
        updated_at_us: keys::micros(&updated_at)?,
        updated_at,
        origin_node: None,
        ..local
    };
    t.entities.replace(row).map_err(engine_err)?;
    Ok(true)
}

/// Apply one record, counting it towards the next compaction if it
/// changed anything.
fn apply_one(t: &mut Tables, record: &Record, origin: Option<&str>) -> StoreResult<bool> {
    let applied = match record {
        Record::Memory(m) => apply_memory(t, m, origin)?,
        Record::Entity(e) => apply_entity(t, e, origin)?,
        Record::Link(l) => apply_link(t, l)?,
        Record::EntityRelation(r) => {
            keys::check_id("entity_relation id", &r.id)?;
            let row = RelationRow::new(r, origin)?;
            match t.relations.get(row.engine_id) {
                Some(local) => {
                    keys::ensure_same_id(&local.id, &r.id)?;
                    false
                }
                None => {
                    t.relations.insert(row).map_err(engine_err)?;
                    true
                }
            }
        }
    };
    if applied {
        t.writes_since_compact += 1;
    }
    Ok(applied)
}

fn apply_link(t: &mut Tables, l: &LinkRecord) -> StoreResult<bool> {
    keys::check_id("link memory_id", &l.memory_id)?;
    keys::check_id("link entity_id", &l.entity_id)?;
    let row = LinkRow::new(l)?;
    if let Some(local) = t.links.get(row.engine_id) {
        // Links are immutable: an existing one is `ON CONFLICT DO NOTHING`.
        keys::ensure_same_id(&local.memory_id, &l.memory_id)?;
        keys::ensure_same_id(&local.entity_id, &l.entity_id)?;
        return Ok(false);
    }
    t.links.insert(row).map_err(engine_err)?;
    Ok(true)
}

impl HubStore for MultimodalHubStore {
    /// Nothing to do: the store opens ready. A change to a record layout is
    /// a conversion under a new schema tag, not a migration (ADR-0021).
    fn migrate(&self) -> StoreResult<()> {
        Ok(())
    }

    /// The store is in-process, so it is reachable whenever the hub is,
    /// unless a sync has failed (see the module docs).
    fn ping(&self) -> StoreResult<()> {
        match &self.read().sync_failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }

    fn apply_record(&self, record: &Record, origin: Option<&str>) -> StoreResult<bool> {
        self.apply_chunk(std::slice::from_ref(record), origin)
            .pop()
            .unwrap_or_else(|| Err(StoreError("a one-record chunk gave no result".into())))
    }

    /// In chunks of [`GROUP_COMMIT`], releasing the write lock between
    /// them so pulls get in.
    fn apply_records(&self, records: &[Record], origin: Option<&str>) -> Vec<StoreResult<bool>> {
        records
            .chunks(GROUP_COMMIT)
            .flat_map(|chunk| self.apply_chunk(chunk, origin))
            .collect()
    }

    fn stats(&self) -> StoreResult<Stats> {
        let t = self.read();
        let memories: Vec<MemoryRow> = all_rows(&t.memories);
        let by_updated = t.memories.inner();
        let edge = |ids: Vec<Uuid>| {
            ids.first()
                .and_then(|id| by_updated.get(*id))
                .map(|row| row.updated_at)
        };
        Ok(Stats {
            total: count(memories.len()),
            tombstones: count(memories.iter().filter(|m| m.deleted_at.is_some()).count()),
            oldest_updated_at: edge(PageBy::<MemoryRow, Keyset>::page_by(by_updated, None, 1)),
            newest_updated_at: edge(PageBy::<MemoryRow, Keyset>::page_by_desc(
                by_updated, None, 1,
            )),
            by_origin_node: group(
                memories.iter().map(|m| m.origin_node.as_deref()),
                UNATTRIBUTED,
            ),
            by_category: group(
                memories.iter().map(|m| Some(m.category.as_str())),
                NO_CATEGORY,
            ),
            entities: count(t.entities.indexed_len()),
            memory_entities: count(t.links.indexed_len()),
            entity_relations: count(t.relations.indexed_len()),
        })
    }

    fn count_tables(&self, wanted: &[&str]) -> StoreResult<Counts> {
        let t = self.read();
        let mut counts = Counts::default();
        for name in wanted {
            match *name {
                "memories" => {
                    let total = count(t.memories.indexed_len());
                    let tombstones = count(
                        all_rows::<_, MemoryRow>(&t.memories)
                            .iter()
                            .filter(|m| m.deleted_at.is_some())
                            .count(),
                    );
                    counts.memories = Some(MemoryCounts {
                        total,
                        live: Some(total - tombstones),
                        tombstones: Some(tombstones),
                    });
                }
                "entities" => counts.entities = Some(count(t.entities.indexed_len())),
                "memory_entities" => counts.memory_entities = Some(count(t.links.indexed_len())),
                "entity_relations" => {
                    counts.entity_relations = Some(count(t.relations.indexed_len()));
                }
                _ => {}
            }
        }
        Ok(counts)
    }

    /// Exact counts are as cheap as an estimate here, and the route reports
    /// exact counts when this is `None` — the honest label.
    fn approx_count_tables(&self, _wanted: &[&str]) -> StoreResult<Option<Counts>> {
        Ok(None)
    }

    fn count_tables_since(&self, wanted: &[&str], since: &str) -> StoreResult<Counts> {
        let t = self.read();
        let since_us = keys::micros(since)?;
        let mut counts = Counts::default();
        for name in wanted {
            match *name {
                "memories" => {
                    let n = RangeBy::<MemoryRow, Keyset>::range_count(
                        t.memories.inner(),
                        past((since_us, MAX_ID_KEY)),
                        Bound::Unbounded,
                    );
                    // No live/tombstone split, as on the SQL stores.
                    counts.memories = Some(MemoryCounts {
                        total: count(n),
                        live: None,
                        tombstones: None,
                    });
                }
                "entities" => {
                    let n = t
                        .entities
                        .range_count(past((since_us, MAX_ID_KEY)), Bound::Unbounded);
                    counts.entities = Some(count(n));
                }
                "memory_entities" => {
                    let n = t
                        .links
                        .range_count(past((since_us, MAX_LINK_KEY)), Bound::Unbounded);
                    counts.memory_entities = Some(count(n));
                }
                "entity_relations" => {
                    let n = t
                        .relations
                        .range_count(past((since_us, MAX_ID_KEY)), Bound::Unbounded);
                    counts.entity_relations = Some(count(n));
                }
                _ => {}
            }
        }
        Ok(counts)
    }

    fn count_by_origin_node(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        let rows = self.memories_since(since)?;
        Ok(group(
            rows.iter().map(|m| m.origin_node.as_deref()),
            UNATTRIBUTED,
        ))
    }

    fn count_by_category(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        let rows = self.memories_since(since)?;
        Ok(group(
            rows.iter().map(|m| Some(m.category.as_str())),
            NO_CATEGORY,
        ))
    }

    fn compact_tombstones(&self, cutoff: &str) -> StoreResult<usize> {
        let mut t = self.write()?;
        let doomed: Vec<MemoryRow> = all_rows::<_, MemoryRow>(&t.memories)
            .into_iter()
            .filter(|m| m.deleted_at.as_deref().is_some_and(|d| d < cutoff))
            .collect();
        if !doomed.is_empty() {
            // Before any delete: the row holding the highest `hub_seq` may be
            // among these, and the next open must not issue it again.
            write_seq_floor(&self.dir, t.next_seq - 1)?;
            let doomed_ids: HashSet<&str> = doomed.iter().map(|m| m.id.as_str()).collect();
            let orphaned: Vec<Uuid> = all_rows::<_, LinkRow>(&t.links)
                .into_iter()
                .filter(|l| doomed_ids.contains(l.memory_id.as_str()))
                .map(|l| l.engine_id)
                .collect();
            for m in &doomed {
                t.memories.delete(m.engine_id).map_err(engine_err)?;
            }
            for id in orphaned {
                t.links.delete(id).map_err(engine_err)?;
            }
            t.writes_since_compact += 1;
        }
        // The admin route is where an operator reclaims space, so it folds
        // the insert logs too.
        compact_tables(&mut t)?;
        Ok(doomed.len())
    }

    fn pull_memories(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        let t = self.read();
        let admit = not_excluded(query);
        let keep = |m: &MemoryRow| admit(m.origin_node.as_deref());
        let rows = match &query.cursor {
            PullCursor::Seq(seq) => {
                page_where::<_, _, Seq>(&t.memories, Some((*seq, UUID_MAX)), query.limit, keep)?
            }
            PullCursor::Keyset { since, since_id } => page_where::<_, _, Keyset>(
                t.memories.inner(),
                Some(id_cursor(since, since_id)?),
                query.limit,
                keep,
            )?,
            PullCursor::Since(since) => page_where::<_, _, Keyset>(
                t.memories.inner(),
                Some(after_instant(since)?),
                query.limit,
                keep,
            )?,
        };
        // The rows are copies: serialise them after letting writers in.
        drop(t);
        Ok(rows.iter().map(MemoryRow::to_wire).collect())
    }

    fn pull_entities(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        let t = self.read();
        let cursor = match &query.cursor {
            PullCursor::Keyset { since, since_id } => id_cursor(since, since_id)?,
            PullCursor::Since(since) => id_cursor(since, "")?,
            // Entities have no hub_seq; as on the SQL stores, a seq cursor
            // degrades to the epoch rather than returning nothing.
            PullCursor::Seq(_) => id_cursor(crate::EPOCH, "")?,
        };
        let keep = not_excluded(query);
        let rows =
            page_where::<_, _, Keyset>(&t.entities, Some(cursor), query.limit, |e: &EntityRow| {
                keep(e.origin_node.as_deref())
            })?;
        drop(t);
        Ok(rows.iter().map(EntityRow::to_wire).collect())
    }

    fn pull_links(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        let t = self.read();
        let cursor = (
            (keys::micros(&query.since)?, keys::link_key(&query.since_id)),
            UUID_MAX,
        );
        let rows =
            page_where::<_, _, Keyset>(&t.links, Some(cursor), query.limit, |_: &LinkRow| true)?;
        drop(t);
        Ok(rows.iter().map(LinkRow::to_wire).collect())
    }

    fn pull_entity_relations(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        let t = self.read();
        let cursor = id_cursor(&query.since, &query.since_id)?;
        let rows = page_where::<_, _, Keyset>(
            &t.relations,
            Some(cursor),
            query.limit,
            |_: &RelationRow| true,
        )?;
        drop(t);
        Ok(rows.iter().map(RelationRow::to_wire).collect())
    }
}

impl MultimodalHubStore {
    /// Memories updated after `since`, or all of them.
    fn memories_since(&self, since: Option<&str>) -> StoreResult<Vec<MemoryRow>> {
        let t = self.read();
        Ok(match since {
            Some(since) => rows_after::<_, _, Keyset>(t.memories.inner(), after_instant(since)?),
            None => all_rows(&t.memories),
        })
    }
}

#[cfg(test)]
mod tests;

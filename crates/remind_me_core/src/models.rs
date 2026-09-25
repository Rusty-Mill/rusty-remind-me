use serde::{Deserialize, Serialize};

/// Output format for search operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    #[default]
    Markdown,
    Json,
}

/// RRF retrieval strategy profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalStrategy {
    #[default]
    Auto,
    Balanced,
    KeywordFavored,
    SemanticFavored,
}

/// Input for `remind_me_entity`: look up one entity by name or alias.
///
/// Read-only, and deliberately carries no `kind`. `remind_me_entity` used to
/// upsert here — same tool name as the reference, opposite effect — so a
/// mistyped name returned `found=false` from `remind_me` while silently
/// *creating* a junk entity in this crate. The write moved to
/// [`crate::entity::upsert_entity`]'s own tool (`remind_me_entity_upsert`);
/// this matches `EntityLookupInput` (`models.py:283`) field for field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityLookupInput {
    /// Entity name or alias. Resolution is case- and whitespace-insensitive.
    pub name: String,
    /// Maximum facts and maximum linked memories to return.
    #[serde(default = "default_entity_lookup_limit")]
    pub limit: usize,
}

fn default_entity_lookup_limit() -> usize {
    20
}

/// Inclusive bounds the reference enforces on `EntityLookupInput::limit`
/// (`ge=1, le=100`).
pub const ENTITY_LOOKUP_LIMIT_MIN: usize = 1;
pub const ENTITY_LOOKUP_LIMIT_MAX: usize = 100;

/// An entity mentioned by a memory (Knowledge Graph).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityInput {
    pub name: String,
    pub kind: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Core Memory data structure stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub source: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    pub capture_id: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub superseded_by: Option<String>,
    pub decay_rate: f64,
    pub vitality: f64,
    /// Write-time importance prior. Now a real column, so `effective_vitality`
    /// no longer has to treat the stored `vitality` as a stand-in.
    pub base_weight: f64,
    pub access_count: i64,
    pub accessed_at: String,
    /// Source document this memory was chunked out of, and its position in it.
    /// Both NULL unless an importer produced the row — which is why
    /// `include_neighbors` finds nothing for a manually added memory.
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
    /// When this memory should be surfaced as a reminder, or `None` for no
    /// reminder. Stored as UTC.
    pub remind_at: Option<String>,
    /// The "don't surface by default" flag. Carried on every memory rather
    /// than only where it is filtered on, so a rendered memory can say so.
    pub sensitive: bool,

    // The six below were stored but never serialised, so a client could not
    // see them at all (#198). They are `Option` even where the column is NOT
    // NULL, because this database is shared with `remind_me` and a row that
    // predates a column's default arrives as NULL; a non-optional `get` would
    // turn that into a hard error on read rather than a missing value.
    /// Classification: `decision`, `fact`, `reference`, … or `unclassified`.
    /// `remind_me_reclassify` writes this, and until #198 nothing could read
    /// it back.
    pub memory_type: Option<String>,
    /// Lifecycle state, `active` unless something archived it.
    pub status: Option<String>,
    /// Which node authored the row, for multi-machine sync.
    pub node_id: Option<String>,
    /// Which client wrote it — `claude-code`, `claude-desktop`, `unknown`.
    pub client: Option<String>,
    /// The capture this memory was distilled out of, if any. Distinct from
    /// `capture_id`, which the reference keeps separately.
    pub source_capture_id: Option<String>,
    /// Tombstone marker. Always null in tool responses, whose queries filter
    /// `deleted_at IS NULL` — emitted anyway because the reference emits it,
    /// and a key that appears on one implementation and not the other is the
    /// divergence this closes.
    pub deleted_at: Option<String>,
}

/// Input model for adding a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAddInput {
    pub content: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
    /// Mark this memory sensitive: a "don't surface by default" convenience
    /// flag, **not access control**. This is a single-user store and anyone
    /// with the database reads everything regardless — the flag only keeps a
    /// memory out of ordinary search and list results unless asked for.
    #[serde(default)]
    pub sensitive: bool,
}

fn default_category() -> String {
    "general".to_string()
}

fn default_source() -> String {
    "manual".to_string()
}

/// Input model for searching memories.
///
/// [`Default`] exists for callers that build one programmatically from stored
/// state — a saved search, say — rather than from a tool call, so they can set
/// the handful of fields they care about without restating every expansion
/// flag. It is hand-written rather than derived **on purpose**: a derived
/// `Default` gives `limit: 0` and `token_budget: 0`, which is not a neutral
/// starting point but a search that structurally cannot return anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchInput {
    pub query: String,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub include_dormant: bool,
    #[serde(default)]
    pub min_vitality: f64,
    #[serde(default)]
    pub verbose: bool,
    /// Surface 1-hop entity-graph neighbours of the results.
    #[serde(default)]
    pub expand_entities: bool,
    /// Surface sibling chunks of the same source document. Only fires for
    /// import-produced memories — anything else has no `doc_id`.
    #[serde(default)]
    pub include_neighbors: bool,
    /// Surface memories frequently retrieved alongside these.
    ///
    /// Include memories marked sensitive. Off by default, so a sensitive
    /// memory never surfaces in an ordinary search — see
    /// [`MemoryAddInput::sensitive`] for why this is not access control.
    #[serde(default)]
    pub include_sensitive: bool,
    /// Which RRF weight profile to fuse with.
    ///
    /// Defaults to [`RetrievalStrategy::Auto`], which routes on query shape.
    /// The other three pin a preset — an escape hatch, and the thing to reach
    /// for when A/B testing retrieval rather than in ordinary use.
    ///
    /// **Precedence against the environment.** This selects a *multiplier
    /// profile* applied on top of whatever
    /// [`crate::retrieval::RrfWeights::from_env`] resolved, so the
    /// `REMIND_ME_RRF_W_*` variables still set the baseline and a per-call
    /// strategy scales it. `Balanced` applies the identity multiplier, so it
    /// reproduces the configured baseline exactly rather than overriding it
    /// back to the built-in defaults — which is what lets an operator zero a
    /// signal and have it stay zeroed under every strategy.
    #[serde(default)]
    pub strategy: RetrievalStrategy,
    /// Only controls *surfacing*. Associations are reinforced on every search
    /// regardless — see [`crate::expansion::record_co_retrieval`].
    #[serde(default)]
    pub expand_co_retrieval: bool,
    /// Prepend the L2/L3 persona as a context bootstrap (#255).
    ///
    /// Off by default. The bootstrap is injected *regardless of whether it
    /// matches the query*, which is the whole point of it — promoted rows are
    /// ordinary memories and already reachable by keyword or vector match, so
    /// a flag that merely let them rank would add nothing. It also means the
    /// bootstrap spends budget on every search that asks for it, which is why
    /// it is opt-in and why the reserve is capped
    /// ([`crate::promotion::BOOTSTRAP_RESERVE_MAX`]).
    #[serde(default)]
    pub bootstrap: bool,
}

impl Default for MemorySearchInput {
    /// Every field at the same value `serde` would supply for an absent key,
    /// so a programmatically-built input and a minimal JSON one behave
    /// identically.
    fn default() -> Self {
        Self {
            query: String::new(),
            category: None,
            tags: None,
            limit: default_limit(),
            token_budget: default_token_budget(),
            response_format: ResponseFormat::default(),
            include_dormant: false,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            include_sensitive: false,
            strategy: RetrievalStrategy::default(),
            expand_co_retrieval: false,
            bootstrap: false,
        }
    }
}

fn default_limit() -> usize {
    20
}

fn default_token_budget() -> usize {
    800
}

/// Input model for listing memories with filters and pagination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryListInput {
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to match.
    pub tags: Option<Vec<String>>,
    pub source: Option<String>,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Include memories marked sensitive. Off by default, so a sensitive
    /// memory never surfaces in an ordinary list — see
    /// [`MemoryAddInput::sensitive`] for why this is not access control.
    #[serde(default)]
    pub include_sensitive: bool,
}

fn default_list_limit() -> usize {
    20
}

/// Inclusive bounds the reference enforces on `MemoryListInput::limit`.
pub const LIST_LIMIT_MIN: usize = 1;
pub const LIST_LIMIT_MAX: usize = 100;

/// A page of memories plus the total matching the same filters.
///
/// `total` counts every row the filters match, not just the returned page, so
/// callers can paginate without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryListResult {
    /// How many are in `memories` — this page, not the whole set (#201).
    ///
    /// Redundant against `memories.len()` and emitted anyway, because the
    /// reference's shared list envelope is `{count, memories, total}` and a
    /// client written against it reads `count`. `total` answers a different
    /// question: how many exist behind the page.
    pub count: usize,
    pub memories: Vec<Memory>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

/// Input model for updating a memory. Every field but `memory_id` is optional;
/// omitted fields are left untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUpdateInput {
    pub memory_id: String,
    pub content: Option<String>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
    /// Set or clear the sensitive flag. `None` leaves it alone, which is why
    /// this is an `Option<bool>` rather than a `bool` — without the third
    /// state, every update that did not mention the flag would clear it.
    ///
    /// Not named by issue #105, which lists only add/search/list. Included
    /// because the reference has it (`models.py:382`) and because without it
    /// a memory marked sensitive at creation can never be unmarked.
    #[serde(default)]
    pub sensitive: Option<bool>,
    /// Clear this memory's `superseded_by` pointer, un-hiding it from search,
    /// entity and subject/predicate lookups.
    ///
    /// The recovery path for a false-positive contradiction-supersession — a
    /// reused generic `(subject, predicate)` pair that wrongly superseded an
    /// unrelated memory. Does *not* touch the memory that did the superseding,
    /// matching `crud.py:410`.
    ///
    /// A plain `bool`, not `Option<bool>`, because unlike [`Self::sensitive`]
    /// there is no "set it back on" direction to express: re-superseding is
    /// something `remind_me_add` does by detecting a contradiction, never
    /// something an update asserts directly. The reference types it the same
    /// way (`models.py:391`, `default=False`).
    #[serde(default)]
    pub clear_superseded: bool,
}

/// Outcome of an update attempt.
///
/// `NoFields` is distinct from `Updated` because the reference reports
/// "nothing to update" rather than silently touching `updated_at`.
#[derive(Debug, Clone)]
pub enum UpdateOutcome {
    Updated(Box<Memory>),
    NotFound,
    NoFields,
}

/// Input model for deleting a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDeleteInput {
    pub memory_id: String,
}

/// One memory's annotation: SPO triple fields and entity mentions.
///
/// Every field but `memory_id` is optional; omitted ones are left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAnnotation {
    pub memory_id: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of annotations to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateInput {
    pub annotations: Vec<MemoryAnnotation>,
}

/// Inclusive bounds the reference enforces on an annotation batch.
pub const ANNOTATE_BATCH_MIN: usize = 1;
pub const ANNOTATE_BATCH_MAX: usize = 100;

/// What happened to one annotation that was applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationApplied {
    pub memory_id: String,
    /// Number of *new* mention links created for this memory.
    pub entities_linked: usize,
}

/// Why one annotation in the batch could not be applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationError {
    pub memory_id: String,
    pub error: String,
}

/// Outcome of an annotation batch.
///
/// Per-item rather than all-or-nothing: one unknown `memory_id` does not
/// discard the rest of the batch, matching the reference, which collects
/// `errors` and continues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateResult {
    /// How many annotations were applied — `results.len()`, emitted because
    /// the reference emits it (#201) and a caller checking "did that work?"
    /// should not have to measure an array to find out.
    pub annotated: usize,
    pub results: Vec<AnnotationApplied>,
    pub errors: Vec<AnnotationError>,
}

/// One memory's classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryClassification {
    pub memory_id: String,
    pub memory_type: String,
}

/// A batch of classifications to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyInput {
    pub classifications: Vec<MemoryClassification>,
}

/// Inclusive bounds the reference enforces on a classification batch.
pub const RECLASSIFY_BATCH_MIN: usize = 1;
pub const RECLASSIFY_BATCH_MAX: usize = 100;

/// Outcome of a classification batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyResult {
    pub updated: usize,
    pub not_found: Vec<String>,
    pub total: usize,
}

/// Request for a batch of memories still awaiting classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyBatchInput {
    #[serde(default = "default_reclassify_batch_size")]
    pub batch_size: usize,
}

fn default_reclassify_batch_size() -> usize {
    20
}

/// One unclassified memory, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnclassifiedMemory {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub tags: Vec<String>,
}

/// A page of memories awaiting classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyBatchResult {
    pub memories: Vec<UnclassifiedMemory>,
    pub total_unclassified: usize,
}

/// The value `memory_type` holds until something classifies it.
pub const UNCLASSIFIED: &str = "unclassified";

/// Input model for `remind_me_consolidate`.
///
/// Two-step workflow, matching the reference's issue #55 update: call with
/// `dry_run: true` (the default) to see the clusters a threshold finds, then
/// write a short `summaries` entry per cluster worth merging and call again
/// with `dry_run: false`. A cluster whose canonical id has no entry in
/// `summaries` is reported in `skipped_no_summary`, not merged with a raw
/// line union.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateInput {
    /// Minimum cosine similarity to cluster memories together. Clamped to
    /// `CONSOLIDATE_SIMILARITY_MIN..=CONSOLIDATE_SIMILARITY_MAX` rather than
    /// rejected, matching this port's convention elsewhere (e.g.
    /// `EntityTraverseInput::hops`).
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f64,
    /// If true (the default), report clusters without modifying data.
    #[serde(default = "default_consolidate_dry_run")]
    pub dry_run: bool,
    /// Limit consolidation to this category.
    #[serde(default)]
    pub category: Option<String>,
    /// Maximum memories to consider. Clamped to
    /// `CONSOLIDATE_LIMIT_MIN..=CONSOLIDATE_LIMIT_MAX`.
    #[serde(default = "default_consolidate_limit")]
    pub limit: usize,
    /// `{canonical_id: summary}`, one entry per cluster (from a prior
    /// `dry_run: true` call) to actually merge when `dry_run` is false.
    #[serde(default)]
    pub summaries: Option<std::collections::HashMap<String, String>>,
}

fn default_similarity_threshold() -> f64 {
    0.85
}

fn default_consolidate_dry_run() -> bool {
    true
}

fn default_consolidate_limit() -> usize {
    500
}

/// Inclusive bounds the reference enforces on `similarity_threshold`.
pub const CONSOLIDATE_SIMILARITY_MIN: f64 = 0.5;
pub const CONSOLIDATE_SIMILARITY_MAX: f64 = 1.0;
/// Inclusive bounds the reference enforces on `limit`.
pub const CONSOLIDATE_LIMIT_MIN: usize = 10;
pub const CONSOLIDATE_LIMIT_MAX: usize = 5000;

/// Input model for `remind_me_feedback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackInput {
    pub memory_id: String,
    pub signal: crate::vitality::FeedbackSignal,
    /// The search query this feedback relates to. Supplying it makes the
    /// feedback contextual rather than a global judgement on the memory.
    pub query: Option<String>,
}

/// Search result item containing memory and diagnostic ranking scores.
///
/// Each `*_score` is that signal's already-weighted contribution to `score`
/// (i.e. `weight / (k + rank)` in RRF-rank fusion, or `weight *
/// normalized_magnitude` in RRF-score fusion) — not the raw rank or
/// magnitude itself. `fts_score` and `idf_score` are always `Some` (the
/// keyword tier always runs); `vec_score` is `None` only when the semantic
/// tier never ran at all (see [`crate::retrieval::rank_rrf`]'s doc comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f64,
    pub fts_score: Option<f64>,
    pub vec_score: Option<f64>,
    pub recency_score: Option<f64>,
    pub vitality_score: Option<f64>,
    pub idf_score: Option<f64>,
    /// The fractional nudge [`crate::vitality::apply_feedback_adjustment`]
    /// applied to `score` from query-contextual feedback, or `None` when no
    /// stored feedback was similar enough to this query to count.
    pub feedback_adjustment: Option<f64>,
    /// The cross-encoder logit [`crate::reranker`] scored this pair at, or
    /// `None` when reranking did not run or this result was in the tail it
    /// left alone. Unlike the `*_score` fields it is **not** a component of
    /// `score`: reranking permutes the head rather than contributing to the
    /// fused total, so folding it in would double-count the signal.
    pub rerank_score: Option<f64>,
}

/// Input model for `remind_me_entity_traverse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityTraverseInput {
    /// Entity name **or alias** to start from; casing and spacing are ignored.
    pub name: String,
    /// Traversal depth. Clamped to 1..=3, matching the reference's bounds.
    #[serde(default = "default_traverse_hops")]
    pub hops: u32,
    /// Only follow edges whose relation label matches this exactly.
    #[serde(default)]
    pub relation: Option<String>,
    /// Maximum edges returned across all hops. Clamped to 1..=100.
    #[serde(default = "default_traverse_cap")]
    pub cap: usize,
}

fn default_traverse_hops() -> u32 {
    1
}

fn default_traverse_cap() -> usize {
    crate::entity::RELATION_TRAVERSAL_CAP
}

/// Request for a batch of raw imports still awaiting normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeBatchInput {
    #[serde(default = "default_normalize_batch_size")]
    pub batch_size: usize,
}

fn default_normalize_batch_size() -> usize {
    20
}

/// Inclusive bounds the reference enforces on a normalization batch request.
pub const NORMALIZE_BATCH_MIN: usize = 1;
pub const NORMALIZE_BATCH_MAX: usize = 100;

/// Inclusive bounds on an *apply* batch.
///
/// Deliberately half the read bound: the reference asks for up to 100 rows to
/// review and accepts up to 50 distillations back. Not a transcription slip.
pub const NORMALIZE_APPLY_MIN: usize = 1;
pub const NORMALIZE_APPLY_MAX: usize = 50;

/// One raw import awaiting normalization, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnnormalizedMemory {
    pub id: String,
    /// First 1000 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub source: String,
    pub tags: Vec<String>,
    /// Lifted out of `metadata` because it is the one field a reviewer needs to
    /// tell two chunks of an import apart.
    pub filename: Option<String>,
}

/// A page of raw imports awaiting normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeBatchResult {
    pub memories: Vec<UnnormalizedMemory>,
    /// The whole backlog, not just this page, so a caller can tell whether
    /// another round is worth requesting.
    pub total_unnormalized: usize,
}

/// One distillation of a raw import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationEntry {
    pub memory_id: String,
    pub question: String,
    pub summary: String,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    /// Entities the distillation mentions. A raw import is never entity-linked
    /// automatically, so without these the normalized memory is invisible to
    /// `remind_me_entity` and `remind_me_entity_traverse`.
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of distillations to write back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeApplyInput {
    pub normalizations: Vec<NormalizationEntry>,
}

/// One successfully written normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationOutcome {
    /// The raw memory the distillation came from.
    pub memory_id: String,
    /// The **new** memory holding the distillation.
    pub normalized_id: String,
}

/// One entry that could not be applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationError {
    pub memory_id: String,
    pub error: String,
}

/// Outcome of an apply batch.
///
/// Unknown ids are reported rather than failing the batch, matching
/// `reclassify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeApplyResult {
    pub normalized: usize,
    pub results: Vec<NormalizationOutcome>,
    pub errors: Vec<NormalizationError>,
}

/// Input model for `remind_me_auto_capture`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCaptureInput {
    /// The verbatim exchange.
    pub conversation: String,
    /// The distillation of it.
    pub summary: String,
    /// Falls back to the summary's first line, truncated, when empty.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Category for the **summary**. The dialog's category is always
    /// [`DIALOG_CATEGORY`].
    #[serde(default = "default_capture_category")]
    pub category: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

fn default_capture_category() -> String {
    "conversation".to_string()
}

/// Category the verbatim half of a capture is always stored under.
///
/// Not the caller's `category` — that names the summary. `extract_batch`
/// excludes this category, so getting the two the wrong way round would flood
/// the annotation backlog with raw transcripts.
pub const DIALOG_CATEGORY: &str = "dialog";
/// Category the structural skeleton of a capture is stored under.
///
/// Excluded from `extract_batch` for the same reason [`DIALOG_CATEGORY`] is,
/// and more so: a skeleton is structure, not prose. There are no facts in it
/// to extract, and offering one to the annotation loop would spend a model
/// call to discover that.
pub const SKELETON_CATEGORY: &str = "skeleton";
/// `source` both halves of a capture are stored under.
pub const CAPTURE_SOURCE: &str = "auto_capture";
/// Longest title derived from a summary when none is supplied.
pub const CAPTURE_TITLE_CHARS: usize = 80;

/// Write (or replace) the structural skeleton of a capture.
///
/// # Why the ranges are lines and not characters
///
/// A skeleton is produced by the calling agent's model, the same way
/// `remind_me_decompose`'s facts are. Asking a model for exact character
/// offsets into a long transcript invites off-by-N errors that are invisible
/// at write time and surface later as a drill-down returning the middle of
/// someone else's sentence. Lines are countable, checkable by eye against the
/// stored dialog, and are the unit a transcript is already structured in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonWriteInput {
    pub capture_id: String,
    /// Mermaid source for the diagram.
    pub mermaid: String,
    /// Node id to inclusive, 1-based line range in the capture's dialog.
    #[serde(default)]
    pub nodes: std::collections::BTreeMap<String, (usize, usize)>,
}

/// A capture's skeleton: the diagram, and what each node points at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skeleton {
    pub capture_id: String,
    pub skeleton_id: String,
    pub mermaid: String,
    pub nodes: std::collections::BTreeMap<String, (usize, usize)>,
}

/// The slice of dialog one skeleton node stands for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonSlice {
    pub capture_id: String,
    pub node: String,
    /// Inclusive, 1-based.
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

/// The two linked memories a capture produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureResult {
    pub capture_id: String,
    pub dialog_id: String,
    pub summary_id: String,
    pub title: String,
    pub tags: Vec<String>,
    /// The summary's category; the dialog's is always [`DIALOG_CATEGORY`].
    pub category: String,
}

/// A capture retrieved by its `capture_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    pub capture_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<Memory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Memory>,
    /// Rows sharing the `capture_id` that are neither half — present so a
    /// malformed capture is visible rather than silently dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<Memory>,
}

/// Request for a batch of memories still awaiting entity/triple extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractBatchInput {
    #[serde(default = "default_extract_batch_size")]
    pub batch_size: usize,
}

fn default_extract_batch_size() -> usize {
    20
}

/// Inclusive bounds the reference enforces on an extraction batch request.
pub const EXTRACT_BATCH_MIN: usize = 1;
pub const EXTRACT_BATCH_MAX: usize = 100;

/// One memory awaiting extraction, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnannotatedMemory {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    /// Carried here but not by [`UnclassifiedMemory`] — an extractor benefits
    /// from knowing what a memory has already been classified as.
    pub memory_type: String,
    pub tags: Vec<String>,
}

/// A page of memories awaiting extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractBatchResult {
    pub memories: Vec<UnannotatedMemory>,
    pub total_unannotated: usize,
}

/// One atomic fact extracted from a capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicFact {
    pub content: String,
    /// Classification, if already known. Defaults to [`UNCLASSIFIED`].
    #[serde(default)]
    pub memory_type: Option<String>,
    /// Merged with the parent capture's tags.
    #[serde(default)]
    pub extra_tags: Vec<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of facts to write against one capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeInput {
    pub capture_id: String,
    pub facts: Vec<AtomicFact>,
}

/// Inclusive bounds the reference enforces on a decomposition.
pub const DECOMPOSE_FACTS_MIN: usize = 1;
pub const DECOMPOSE_FACTS_MAX: usize = 50;

/// `category` every decomposed fact is stored under.
pub const FACT_CATEGORY: &str = "fact";
/// Category a scenario — knowledge organised around one entity or topic,
/// synthesised from several facts — is stored under (#208).
pub const SCENARIO_CATEGORY: &str = "scenario";
/// Category a persona statement — something durably true about the user — is
/// stored under (#208).
pub const PERSONA_CATEGORY: &str = "persona";

/// A step of the refinement ladder.
///
/// `CaptureToFact` is listed because the *backlog* for it is worth reporting
/// alongside the others, not because this module promotes it —
/// `remind_me_decompose` does, and `promote` refuses the rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    CaptureToFact,
    FactToScenario,
    ScenarioToPersona,
}

impl Rung {
    /// Stable string form, used as the stored `rung` value. Written by hand
    /// rather than derived from `Debug` so a rename of the variant cannot
    /// silently orphan every row already stored under the old spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CaptureToFact => "capture_to_fact",
            Self::FactToScenario => "fact_to_scenario",
            Self::ScenarioToPersona => "scenario_to_persona",
        }
    }
}

/// Something ready to move up a rung.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionCandidate {
    pub rung: Rung,
    /// The memories a promotion would be built from.
    pub source_ids: Vec<String>,
    pub snippet: String,
    /// Why this is a candidate, in words — a listing that only returns ids
    /// makes the caller guess what it is looking at.
    pub reason: String,
    /// The entity or topic the group formed around, where one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouped_by: Option<String>,
}

/// Inclusive bounds and default on a `remind_me_promotion_candidates` limit.
///
/// Previously two unlinked JSON/Rust literals (issue #283): the MCP schema
/// hardcoded `1`/`100` directly and `promotion::promotion_candidates` clamped
/// with its own `1`/`100` literal, so the two could drift without either side
/// noticing. Named here and referenced from both, matching
/// `CONTRADICTION_LIMIT_MIN`/`MAX` and `RECALIBRATE_LIMIT_MIN`/`MAX`.
pub const PROMOTION_LIMIT_MIN: usize = 1;
pub const PROMOTION_LIMIT_MAX: usize = 100;
pub const PROMOTION_LIMIT_DEFAULT: usize = 20;

/// Accept a distillation of some sources into the rung above them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteInput {
    pub rung: Rung,
    pub source_ids: Vec<String>,
    /// The distilled text.
    pub content: String,
}

/// What a promotion produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionResult {
    pub promoted_id: String,
    pub rung: Rung,
    pub category: String,
    pub source_ids: Vec<String>,
}

/// What a memory came from, and what was built on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub memory_id: String,
    /// The rung below: memories this was promoted from.
    pub sources: Vec<String>,
    /// The rung above: memories promoted from this.
    pub derived: Vec<String>,
}

/// One durable statement about the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaStatement {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub created_at: String,
    /// How many of its sources still stand. Zero means withheld — see
    /// [`crate::promotion::persona`].
    pub surviving_sources: usize,
}
/// `source` every decomposed fact is stored under.
pub const DECOMPOSITION_SOURCE: &str = "decomposition";

/// Outcome of a decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeResult {
    pub created: usize,
    pub fact_ids: Vec<String>,
    pub capture_id: String,
    /// The parent capture's tags, which every fact inherited.
    pub parent_tags_inherited: Vec<String>,
    pub entities_linked: usize,
    pub relations_linked: usize,
    /// Memories superseded because a fact contradicted them.
    pub superseded_ids: Vec<String>,
}

/// Request for a batch of captures still awaiting decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeBatchInput {
    #[serde(default = "default_decompose_batch_size")]
    pub batch_size: usize,
}

fn default_decompose_batch_size() -> usize {
    20
}

/// Inclusive bounds on a decomposition batch request.
pub const DECOMPOSE_BATCH_MIN: usize = 1;
pub const DECOMPOSE_BATCH_MAX: usize = 100;

/// One capture awaiting decomposition, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndecomposedCapture {
    pub id: String,
    pub capture_id: String,
    /// First 500 characters of the content.
    pub content_snippet: String,
    pub category: String,
    pub tags: Vec<String>,
}

/// A page of captures awaiting decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeBatchResult {
    pub memories: Vec<UndecomposedCapture>,
    pub total_undecomposed: usize,
}

/// Serialisation format for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// One indented JSON array.
    #[default]
    Json,
    /// One JSON record per line.
    Jsonl,
}

/// Input model for `remind_me_export_memories`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportInput {
    #[serde(default)]
    pub format: ExportFormat,
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to be exported.
    pub tags: Option<Vec<String>>,
    /// Destination file. Must be inside the allowed export roots. When omitted,
    /// the payload is returned inline.
    pub file_path: Option<String>,
    /// Include the entity graph as `record_type`-tagged records. Defaults to
    /// true — a backup should be complete.
    #[serde(default = "default_include_graph")]
    pub include_graph: bool,
    /// Include soft-deleted and superseded memories.
    ///
    /// **Off by default, and that default is load-bearing.** Export records
    /// are stamped `role: "assistant"` so the importer reads them as live
    /// content, which means a round-trip of an export that carried tombstones
    /// and superseded facts would resurrect them as fresh live memories. The
    /// reference makes the same argument in this field's own docstring
    /// (`models.py:799`) and gates both conditions on it (`exporter.py:163`).
    ///
    /// Set it only for a genuine full-backup or audit export — not for moving
    /// memories between machines.
    #[serde(default)]
    pub include_deleted: bool,
}

fn default_include_graph() -> bool {
    true
}

/// Outcome of an export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    /// Always `"ok"` on this path — an export that failed returns an error
    /// rather than this struct. Emitted because the reference emits it (#201),
    /// so a client branching on `status` has something to branch on.
    pub status: &'static str,
    /// Memory records only; the graph counts are reported separately.
    pub exported: usize,
    pub format: ExportFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    /// The payload, when no `file_path` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// How to parse an imported file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    /// Detect: `.json`/`.jsonl` are chat; markdown and text are sniffed for
    /// chat role markers.
    #[default]
    Auto,
    Chat,
    /// Per-section or per-paragraph chunking. `.md`/`.markdown`/`.txt` only.
    Document,
    /// An Obsidian note: document chunking plus frontmatter, `[[wikilink]]`
    /// and inline `#tag` extraction. Markdown only — the conventions are
    /// Markdown conventions.
    Obsidian,
    /// A PDF: per-page text extraction. Requires the `pdf` feature.
    Pdf,
    /// An image: OCR the whole image into one document. Requires the `ocr`
    /// feature.
    Image,
    /// A recording: transcribed per Whisper segment, each carrying its time
    /// range. Requires the `audio` feature.
    Audio,
    /// A saved Readwise export: one memory per highlight. Must be requested
    /// explicitly — never chosen by `Auto`, because a Readwise export and a
    /// chat export are both an unadorned `.json` and guessing wrong silently
    /// corrupts working chat imports.
    Readwise,
}

/// Message extraction strategies a chat import accepts.
pub const EXTRACT_MODES: [&str; 5] = [
    "assistant_messages",
    "user_messages",
    "all_messages",
    "conversations",
    "summaries",
];

/// Inclusive bounds on an import's chunk size.
pub const IMPORT_MAX_LENGTH_MIN: usize = 100;
pub const IMPORT_MAX_LENGTH_MAX: usize = 50_000;

fn default_import_category() -> String {
    "chat_import".to_string()
}

fn default_extract_mode() -> String {
    "assistant_messages".to_string()
}

fn default_import_max_length() -> usize {
    10_000
}

fn default_recursive() -> bool {
    true
}

/// Input model for `remind_me_import_chat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatImportInput {
    pub file_path: String,
    #[serde(default = "default_import_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_extract_mode")]
    pub extract_mode: String,
    #[serde(default = "default_import_max_length")]
    pub max_length: usize,
    #[serde(default)]
    pub kind: ImportKind,
}

/// Input model for `remind_me_import_directory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkImportDirInput {
    pub directory: String,
    #[serde(default = "default_import_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_extract_mode")]
    pub extract_mode: String,
    #[serde(default = "default_import_max_length")]
    pub max_length: usize,
    #[serde(default = "default_recursive")]
    pub recursive: bool,
    #[serde(default)]
    pub kind: ImportKind,
}

/// Inclusive bounds on how many `dbs` items one call pulls.
pub const DBS_IMPORT_LIMIT_MIN: usize = 1;
pub const DBS_IMPORT_LIMIT_MAX: usize = 2000;

fn default_dbs_limit() -> usize {
    500
}

/// Input model for `remind_me_import_dbs`.
///
/// `source` and `item_type` are empty rather than `Option` to match the
/// reference's own model, where empty means "no filter".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbsImportInput {
    /// Path to the `dbs` SQLite archive, inside the allowed import roots.
    pub db_path: String,
    /// Restrict to one `dbs` source name, or empty for all.
    #[serde(default)]
    pub source: String,
    /// Restrict to one `dbs` `item_kind`, or empty for all.
    #[serde(default)]
    pub item_type: String,
    #[serde(default = "default_dbs_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Extra tags added to every imported memory.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Report what would be imported without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Inclusive bounds on how many MemPalace drawers one call pulls.
pub const MEMPALACE_IMPORT_LIMIT_MIN: usize = 1;
pub const MEMPALACE_IMPORT_LIMIT_MAX: usize = 2000;

fn default_mempalace_limit() -> usize {
    500
}

/// Input model for `remind_me_import_mempalace`.
///
/// No path field: unlike [`DbsImportInput`], the store location is operator
/// configuration (`REMIND_ME_MEMPALACE_PATH`), not a per-call argument — see
/// `docs/adr/0001-mempalace-chroma-sqlite-read.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempalaceImportInput {
    /// Restrict to one wing (project), or empty for all.
    #[serde(default)]
    pub wing: String,
    /// Restrict to one room within the wing, or empty for all.
    #[serde(default)]
    pub room: String,
    #[serde(default = "default_mempalace_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Category for a drawer with no restorable frontmatter category.
    #[serde(default)]
    pub category: String,
    /// Extra tags added to every imported memory.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Report what would be imported without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Counts from one import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportStats {
    pub memories_created: usize,
    /// Messages (chat) or chunks (document) before per-message chunking.
    pub raw_entries: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub entities_restored: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub links_restored: usize,
    /// Links whose endpoints were not present, so could not be restored.
    #[serde(skip_serializing_if = "is_zero")]
    pub links_skipped_dangling: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub relations_restored: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub relations_skipped_dangling: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What happened to one imported file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ImportOutcome {
    Imported {
        import_id: String,
        kind: ImportKind,
        file: String,
        #[serde(flatten)]
        stats: ImportStats,
    },
    /// The same content has been imported before.
    Skipped {
        reason: String,
        file: String,
        import_id: String,
    },
    Failed {
        file: String,
        reason: String,
    },
}

/// Outcome of a directory import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkImportResult {
    pub files_seen: usize,
    pub files_imported: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub memories_created: usize,
    pub results: Vec<ImportOutcome>,
}

// ---------------------------------------------------------------------------
// HTTP API (`remind_me_api`) — bulk operations and paginated search
// ---------------------------------------------------------------------------
//
// These have no MCP-tool equivalent: the reference's bulk routes exist only
// on its dashboard-facing HTTP surface (a dashboard selects a batch from a
// list/search result, then acts on exactly that selection), and paginated
// search with a `total`/`has_more` envelope is likewise an HTTP-only shape —
// the MCP `remind_me_search` tool returns a ranked, token-budgeted list
// instead.

/// A caller-supplied id list, capped for the same reason `ConsolidateInput`
/// caps its candidate pool: a bounded worst case per request. Dashboard
/// selections are user-driven and typically small; this is a safety limit,
/// not an expected size.
pub const BULK_IDS_MAX: usize = 200;

/// Result of a bulk delete: which ids were removed, which named nothing live.
///
/// A missing id does not fail the batch — the rest still applies — so a
/// caller can tell which selections were stale without retrying one at a
/// time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkDeleteResult {
    pub deleted: Vec<String>,
    pub not_found: Vec<String>,
}

/// How [`BulkTagInput`] combines its `tags` with each memory's existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMode {
    /// Union onto the memory's existing tags (the default).
    #[default]
    Add,
    /// Drop these tags if present; anything else is left alone.
    Remove,
    /// Replace the memory's tags wholesale.
    Set,
}

/// Input for a bulk tag operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkTagInput {
    pub ids: Vec<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub mode: TagMode,
}

/// Result of a bulk tag operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkTagResult {
    pub updated: Vec<String>,
    pub not_found: Vec<String>,
}

/// Input to the paginated `GET /api/memories/search` route.
///
/// `query` has already had any `entity:` token stripped by the caller (see
/// [`crate::fts::extract_entity_token`]); `entity` carries that token
/// separately so the entity-scoped path can apply its own rules
/// (superseded exclusion) rather than folding them into the FTS predicate.
#[derive(Debug, Clone)]
pub struct SearchPageInput {
    pub query: String,
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to match.
    pub tags: Option<Vec<String>>,
    pub entity: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

/// A page of [`SearchPageInput`] results, with the same pagination envelope
/// as [`MemoryListResult`] — a client pages through search results the same
/// way it pages through a list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchPageResult {
    pub total: usize,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub memories: Vec<Memory>,
    /// Set when an `entity:` token named an entity this store has never
    /// heard of — the result is a real empty page, not a query error, and
    /// the message says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Importance recalibration (gap T7, issue #102)
// ---------------------------------------------------------------------------

/// Request for a batch of memories whose importance classification may be
/// stale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidatesInput {
    #[serde(default = "default_recalibrate_limit")]
    pub limit: usize,
}

fn default_recalibrate_limit() -> usize {
    20
}

/// Inclusive bounds the reference enforces on a recalibration request.
pub const RECALIBRATE_LIMIT_MIN: usize = 1;
pub const RECALIBRATE_LIMIT_MAX: usize = 100;

/// One memory put forward for importance review.
///
/// Every field is something a reviewer needs to judge from without a second
/// round trip: what it says, how it was classified, how important it was
/// assumed to be, and how long it has gone untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidate {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub memory_type: Option<String>,
    pub base_weight: f64,
    pub access_count: i64,
    /// `None` for a memory never retrieved since it was written — which is
    /// itself part of why it is a candidate, so it is reported rather than
    /// collapsed into `created_at`.
    pub accessed_at: Option<String>,
    pub created_at: String,
}

/// A page of recalibration candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidatesResult {
    pub candidates: Vec<RecalibrateCandidate>,
    /// The whole backlog behind the `limit`, so a caller can tell whether
    /// another round is worth requesting.
    pub total_candidates: i64,
}

// ---------------------------------------------------------------------------
// Import rollback (gap T8, issue #103)
// ---------------------------------------------------------------------------

/// Which import ledger an undo targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UndoImportKind {
    Chat,
    Dbs,
    Mempalace,
}

/// Request to roll back a previous import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoImportInput {
    pub import_kind: UndoImportKind,
    /// Scope to one import run. For `chat` this is the `chat_imports`
    /// `import_id`; for `dbs` the `dbs_source`; for `mempalace` a `drawer_id`
    /// prefix. `None` targets every record of that kind.
    #[serde(default)]
    pub import_id: Option<String>,
    /// Defaults to **true**. Bulk deletion that propagates over sync is opt-in,
    /// not opt-out — the asymmetry between an accidental dry run and an
    /// accidental deletion is the whole argument.
    #[serde(default = "default_undo_dry_run")]
    pub dry_run: bool,
    #[serde(default = "default_undo_limit")]
    pub limit: usize,
}

fn default_undo_dry_run() -> bool {
    true
}

fn default_undo_limit() -> usize {
    500
}

/// Inclusive bounds the reference enforces on an undo batch.
pub const UNDO_IMPORT_LIMIT_MIN: usize = 1;
pub const UNDO_IMPORT_LIMIT_MAX: usize = 5000;

/// Outcome of an undo call, dry run or otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoImportResult {
    pub import_kind: UndoImportKind,
    /// Human-readable description of what was targeted, echoed back so a
    /// caller can see whether their scope meant what they thought.
    pub scope: String,
    pub matched: usize,
    pub dry_run: bool,
    /// Whether this was a tombstone or an outright delete, and why.
    pub mode: String,
    pub removed: usize,
    /// What is left after this call — the resumability signal.
    pub remaining: usize,
    pub tracking_rows_removed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

// ---------------------------------------------------------------------------
// Saved and watched searches (gap T3, issue #108)
// ---------------------------------------------------------------------------

/// Result cap a poll uses.
///
/// Higher than a tool call's default: a poll diffs result sets, and a match
/// dropped by a tighter limit would look like it had stopped matching.
pub const POLL_RESULT_LIMIT: usize = 100;

/// The filters stored alongside a saved search's query, as the `filters` JSON
/// column holds them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSearchFilters {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub include_sensitive: bool,
}

/// A stored query plus its filters, under a unique name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub id: String,
    pub name: String,
    pub query: String,
    pub filters: SavedSearchFilters,
    /// Whether polling reports this search's new matches. Does **not** narrow
    /// what running it returns — see `saved_searches`' module docs.
    pub watch: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Request to create or update a saved search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveSearchInput {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub include_sensitive: bool,
    #[serde(default)]
    pub watch: bool,
}

/// Request naming one saved search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchNameInput {
    pub name: String,
}

// ---------------------------------------------------------------------------
// Edit history (gap T4, issue #109)
// ---------------------------------------------------------------------------

/// One snapshot of a memory's tracked columns, taken before an edit replaced
/// them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRevision {
    pub id: i64,
    pub memory_id: String,
    pub content: String,
    pub category: String,
    /// Stored form (a JSON array string), matching the `memories` column it
    /// snapshots, so a revert can write it straight back.
    pub tags: String,
    pub metadata: String,
    /// `None` for a revision captured before the column existed.
    pub sensitive: Option<bool>,
    pub edited_at: String,
    pub revision_reason: Option<String>,
}

/// Request for a memory's revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryInput {
    pub memory_id: String,
    #[serde(default = "default_history_limit")]
    pub limit: usize,
    /// Markdown or JSON, defaulting to markdown as the reference does
    /// (`RevisionHistoryInput.response_format`, `models.py:510`).
    #[serde(default)]
    pub response_format: ResponseFormat,
}

fn default_history_limit() -> usize {
    // 10, not 20 -- the reference's `RevisionHistoryInput.limit` default
    // (`models.py:504`). The bounds already matched; only the default drifted,
    // so a caller omitting `limit` was getting a different number of revisions
    // from each implementation with nothing announcing it.
    10
}

/// Input for `remind_me_stats`.
///
/// Only a response format — the reference's `MemoryStatsInput`
/// (`models.py:750`) carries nothing else, and its models are `extra="forbid"`,
/// so anything more would be rejected there while being accepted here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStatsInput {
    #[serde(default)]
    pub response_format: ResponseFormat,
}

pub const HISTORY_LIMIT_MIN: usize = 1;
pub const HISTORY_LIMIT_MAX: usize = 100;

/// Request to restore a memory to a prior revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertInput {
    pub memory_id: String,
    pub revision_id: i64,
    /// Free text recorded on the revision the revert itself creates.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What a revert did.
///
/// The two not-found cases are distinct because they need different fixes: a
/// missing memory means the id is wrong, a missing revision means the revision
/// id is wrong or belongs to a different memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RevertOutcome {
    Reverted {
        revision_id: i64,
    },
    /// The memory already holds this revision's values.
    NoChange,
    MemoryNotFound,
    RevisionNotFound,
}

// ---------------------------------------------------------------------------
// Contradiction candidates (gap T6, issue #110)
// ---------------------------------------------------------------------------

/// One side of a candidate pair, carrying enough to judge it without a second
/// round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionSide {
    pub id: String,
    /// First 500 characters, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub memory_type: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub created_at: String,
}

/// Two memories that might assert incompatible things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionCandidate {
    pub memory_a: ContradictionSide,
    pub memory_b: ContradictionSide,
    /// Names of the entities both memories mention, in name order.
    ///
    /// This is *why the pair was surfaced at all* — the candidate query joins
    /// on `memory_entities` — so without it the caller sees two memories and
    /// no indication of what connects them. Which entity is shared changes how
    /// the conflict reads, and re-deriving the join caller-side is work the
    /// producer has already done.
    pub shared_entities: Vec<String>,
}

/// A page of candidate pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionCandidatesResult {
    pub candidates: Vec<ContradictionCandidate>,
    /// The size of the WHOLE queue, not of what remains after the cursor.
    pub total_candidates: i64,
    /// Cursor to pass back as `after_a`/`after_b` for the next page. Null on a
    /// short page, which is the queue being exhausted — callers loop until it
    /// is null rather than comparing counts against `total_candidates`, which
    /// does not shrink as they page.
    pub next_after_a: Option<String>,
    pub next_after_b: Option<String>,
    pub has_more: bool,
}

/// Request for a batch of contradiction candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionCandidatesInput {
    #[serde(default = "default_contradiction_limit")]
    pub limit: usize,
    /// Keyset cursor on the `(id_a, id_b)` the query already orders by. Both
    /// or neither — see [`ContradictionCandidatesInput::cursor`].
    #[serde(default)]
    pub after_a: Option<String>,
    #[serde(default)]
    pub after_b: Option<String>,
}

impl ContradictionCandidatesInput {
    /// The validated cursor: `Ok(Some(..))` for both set, `Ok(None)` for
    /// neither, `Err` for exactly one.
    ///
    /// Half a cursor is rejected rather than ignored. Silently dropping it
    /// would page from the *start* while the caller believed it was paging
    /// from where it left off — the same "every call returns the same first
    /// page" failure this cursor exists to fix, only now invisible because the
    /// caller is passing something.
    pub fn cursor(&self) -> std::result::Result<Option<(&str, &str)>, &'static str> {
        match (&self.after_a, &self.after_b) {
            (Some(a), Some(b)) => Ok(Some((a.as_str(), b.as_str()))),
            (None, None) => Ok(None),
            _ => Err(
                "after_a and after_b must be passed together — a half cursor \
                      would silently page from the start",
            ),
        }
    }
}

fn default_contradiction_limit() -> usize {
    20
}

pub const CONTRADICTION_LIMIT_MIN: usize = 1;
pub const CONTRADICTION_LIMIT_MAX: usize = 100;

// ---------------------------------------------------------------------------
// Vault digest (gap T5, issue #111)
// ---------------------------------------------------------------------------

/// One memory as the digest lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestRecentMemory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

/// The digest's underlying data.
///
/// `reminders_*` and `sync` are `Option` because their subsystems do not exist
/// yet (#116, #114). `None` means "nothing here can tell", which is different
/// from "nothing to report" — the renderer omits those sections rather than
/// printing a "none" that would read as an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestData {
    pub generated_at: String,
    pub since_days: i64,
    pub recent_memories: Vec<DigestRecentMemory>,
    /// The true count, uncapped, so the `MAX_RECENT_MEMORIES` cap is visible.
    pub recent_total: i64,
    pub vitality: crate::vitality::VitalityReport,
    pub reminders_upcoming: Vec<DigestReminder>,
    pub reminders_overdue: Vec<DigestReminder>,
    pub sync: SyncStatus,
}

/// One reminder as the digest lists it.
///
/// A trimmed projection rather than the whole memory: the digest is a scan,
/// and a full memory block per reminder would bury the four other sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestReminder {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remind_at: Option<String>,
    pub content: String,
}

/// Request for a vault digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestInput {
    #[serde(default = "default_digest_since_days")]
    pub since_days: i64,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

fn default_digest_since_days() -> i64 {
    crate::digest::DEFAULT_SINCE_DAYS
}

// ---------------------------------------------------------------------------
// Analytics snapshots (gap A1, issue #112)
// ---------------------------------------------------------------------------

/// One day's recorded shape of the vault.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyticsSnapshot {
    pub captured_at: String,
    pub total_memories: i64,
    pub vitality_buckets: std::collections::BTreeMap<String, usize>,
    pub category_counts: std::collections::BTreeMap<String, i64>,
}

/// What a capture attempt did.
///
/// `AlreadyToday` is a normal outcome rather than an error: the caller is a
/// poll loop, and being asked twice in a day is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapturedSnapshot {
    Captured { id: i64 },
    AlreadyToday { id: i64 },
}

// ---------------------------------------------------------------------------
// Sync status (gap T2a, issue #114)
// ---------------------------------------------------------------------------

/// Which way the outbox backlog is moving.
///
/// `Unknown` is a real answer, not a failure: a direction needs two
/// observations, and the first call after a restart only has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainVerdict {
    Draining,
    Stalled,
    Growing,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxStatus {
    pub pending: i64,
    pub sent: i64,
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_pending: Option<String>,
    pub drain: DrainVerdict,
    /// Change per minute since the previous observation. Negative is draining.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_minute: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TombstoneStatus {
    pub total: i64,
    pub compactable_now: i64,
}

/// One remote's contact state.
///
/// The three timestamps are wall-clock contact times, never content cursors —
/// see `sync::status`' module docs for why conflating them makes a quiet
/// healthy peer indistinguishable from a wedged one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub remote_id: String,
    pub last_attempt_at: String,
    pub last_push_at: String,
    pub last_pull_at: String,
    /// False when the timestamps are still at their epoch default — which is
    /// what separates "never tried" from "tried and failing".
    pub ever_contacted: bool,
    pub pending: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncStatus {
    Disabled {
        missing: Vec<String>,
        hint: String,
    },
    Enabled {
        node_id: String,
        hub_url: String,
        outbox: OutboxStatus,
        tombstones: TombstoneStatus,
        remotes: Vec<RemoteStatus>,
    },
}

/// Request to reset a remote's pull cursors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRepairInput {
    #[serde(default = "default_repair_remote")]
    pub remote_id: String,
}

fn default_repair_remote() -> String {
    "hub".to_string()
}

// ---------------------------------------------------------------------------
// Reconcile (gap T2b, issue #115)
// ---------------------------------------------------------------------------

/// A remote's `/count` response.
///
/// Deserialised from both the peer server's endpoint and the hub's, which
/// return the same shape by design — see `sync::server`'s `handle_count`.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCounts {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub version: Option<String>,
    pub memories: RemoteMemoryCounts,
    /// Absent from the peer server, which reports no per-category breakdown.
    #[serde(default)]
    pub by_category: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteMemoryCounts {
    pub total: i64,
    #[serde(default)]
    pub tombstones: i64,
}

/// One category where the two sides disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryDrift {
    pub category: String,
    pub local: i64,
    pub remote: i64,
    /// `remote - local`. Negative means this node is ahead.
    pub delta: i64,
}

/// How a reconcile classified the drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReconcileVerdict {
    InSync,
    PullLag,
    /// This node holds records the remote does not — pushes are not landing.
    /// The only direction that means data is at risk.
    NodeAhead,
    Fault,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReconcileReport {
    /// The remote could not be reached, or sync is not configured. A verdict
    /// here would be a guess, so the reachability problem is the answer.
    Unavailable {
        /// Serialised as `hint`, matching the reference (#201). The field
        /// keeps the name that describes what it holds; only the wire key is
        /// aligned, because a client written against `remind_me` reads `hint`
        /// and found nothing here.
        #[serde(rename = "hint")]
        reason: String,
    },
    Compared {
        remote_id: String,
        remote_role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote_version: Option<String>,
        verdict: ReconcileVerdict,
        hints: Vec<String>,
        local_total: i64,
        remote_total: i64,
        local_tombstones: i64,
        remote_tombstones: i64,
        /// Only categories that disagree. A hundred agreeing rows would bury
        /// the two that matter.
        drift: Vec<CategoryDrift>,
        categories_agreeing: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_pull_age_seconds: Option<i64>,
    },
}

/// Request to reconcile against one peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcilePeerInput {
    pub node_id: String,
}

// ---------------------------------------------------------------------------
// Reminders
// ---------------------------------------------------------------------------

/// Which reminders `remind_me_list_reminders` surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReminderWindow {
    /// Set and still in the future.
    #[default]
    Upcoming,
    /// Due, and not yet recorded in `reminder_deliveries` — typically because
    /// the scheduler was offline when it came due.
    Overdue,
    /// The union of both.
    All,
}

/// Input for `remind_me_set_reminder`: set or clear a memory's reminder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetReminderInput {
    pub memory_id: String,
    /// ISO-8601 timestamp. Naive timestamps are read as UTC. Omitted or null
    /// clears an existing reminder instead of setting one.
    #[serde(default)]
    pub remind_at: Option<String>,
}

/// What a `set_reminder` call did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SetReminderOutcome {
    /// No live memory with that id.
    NotFound {
        memory_id: String,
    },
    /// The timestamp could not be parsed, or was not in the future. Rejected
    /// rather than stored, because a reminder that can never fire is a
    /// silently broken one.
    Rejected {
        reason: String,
    },
    Cleared {
        memory_id: String,
    },
    Set {
        memory_id: String,
        /// Canonicalized to UTC, which is what was actually stored.
        remind_at: String,
    },
}

/// Input for `remind_me_list_reminders`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRemindersInput {
    #[serde(default)]
    pub when: ReminderWindow,
    #[serde(default = "default_reminder_limit")]
    pub limit: i64,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

fn default_reminder_limit() -> i64 {
    20
}

pub const REMINDER_LIMIT_MIN: i64 = 1;
pub const REMINDER_LIMIT_MAX: i64 = 100;

impl Default for ListRemindersInput {
    fn default() -> Self {
        Self {
            when: ReminderWindow::default(),
            limit: default_reminder_limit(),
            response_format: ResponseFormat::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stale code-reference candidates (#260, limit constants added for #283)
// ---------------------------------------------------------------------------

/// Inclusive bounds and default on a `remind_me_stale_candidates` limit.
///
/// `code_refs::stale_candidates` lives outside this module (it owns its own
/// `StaleCandidate`/`StaleRef` types), but its limit bound is named here
/// anyway, matching where every other MCP-surface limit constant lives —
/// `code_refs.rs` imports these rather than keeping a second copy.
pub const STALE_CANDIDATES_LIMIT_MIN: usize = 1;
pub const STALE_CANDIDATES_LIMIT_MAX: usize = 100;
pub const STALE_CANDIDATES_LIMIT_DEFAULT: usize = 20;

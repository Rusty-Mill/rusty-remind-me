//! First-class project records (ADR-0027).
//!
//! A project is an entity with a live `project.stage` claim. Every piece of
//! project state is one fact memory whose predicate is in the closed
//! `project.*` set and whose metadata cites the memories that support it.
//! Nothing here adds a table: claims sync, supersede and search like any fact.
//!
//! The typed [`Project`] is a projection. It is built from live claims and
//! nothing outside this module constructs one, so a project without evidence,
//! or with a stage outside the enum, cannot be represented. A malformed claim
//! that arrives by sync is reported in [`ProjectList::invalid`], never dropped.
//!
//! Claims are append-only. History is the chain of claim memories for one
//! `(project, predicate)`; undoing a claim writes a compensating claim that
//! restores the earlier value and names what it reverts. The generic write
//! paths refuse `project.*` predicates ([`refuse_reserved_predicate`]) and
//! edits to claim memories ([`refuse_claim_edit`]), so this module is the
//! only writer.

use crate::db::memories::{Memories, NewMemory};
use crate::db::{Result, Store, StoreError};
use crate::entity::{
    entity_id, get_entity_by_id, link_memory_entity, normalize_entity_name, resolve_entity,
    upsert_entity,
};
use crate::models::{EntityInput, Memory, FACT_CATEGORY};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

/// Every claim predicate starts with this.
pub const PREDICATE_PREFIX: &str = "project.";
/// Tag on every claim memory.
pub const CLAIM_TAG: &str = "project-record";
/// Tag on a `because` sentence saved as evidence.
pub const EVIDENCE_TAG: &str = "project-evidence";
/// `source` of every memory this module writes.
const SOURCE: &str = "project";
/// Key of the claim's block inside a memory's metadata.
const META_KEY: &str = "project_claim";

/// How far a project has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Idea,
    Designed,
    Built,
}

/// Whether anyone is working on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Dormant,
    Abandoned,
}

/// The closed set of claim predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Stage,
    Status,
    Repo,
    Path,
    Tracker,
    Parent,
}

macro_rules! closed_enum {
    ($ty:ident, $what:literal, $($variant:ident => $text:literal),+ $(,)?) => {
        impl $ty {
            pub const ALL: &'static [$ty] = &[$($ty::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($ty::$variant => $text),+ }
            }
        }

        impl FromStr for $ty {
            type Err = StoreError;

            fn from_str(s: &str) -> Result<Self> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $($text => Ok($ty::$variant),)+
                    other => Err(StoreError::Invalid(format!(
                        concat!("unknown ", $what, " '{}' (expected one of: {})"),
                        other,
                        [$($text),+].join(", "),
                    ))),
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

closed_enum!(Stage, "stage", Idea => "idea", Designed => "designed", Built => "built");
closed_enum!(Status, "status", Active => "active", Dormant => "dormant", Abandoned => "abandoned");
closed_enum!(
    Predicate, "project predicate",
    Stage => "stage", Status => "status", Repo => "repo",
    Path => "path", Tracker => "tracker", Parent => "parent",
);

impl Predicate {
    /// The stored predicate, e.g. `project.stage`.
    pub fn qualified(self) -> String {
        format!("{PREDICATE_PREFIX}{}", self.as_str())
    }

    /// Parse a stored predicate such as `project.stage`.
    pub fn from_qualified(predicate: &str) -> Result<Self> {
        let normalized = normalize_entity_name(predicate);
        let short = normalized.strip_prefix(PREDICATE_PREFIX).ok_or_else(|| {
            StoreError::Invalid(format!("'{predicate}' is not a project predicate"))
        })?;
        short.parse()
    }
}

/// A repository URL, normalised to `https://host/owner/name`.
// Serialize only: a value is built by `parse`, never deserialized around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RepoUrl(String);

impl RepoUrl {
    /// Accepts `https://`, `http://`, `ssh://[user@]host/...` and scp-style
    /// `user@host:owner/name`, with or without `.git`.
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid = || StoreError::Invalid(format!("'{raw}' is not a repository URL"));
        let raw_trimmed = raw.trim();
        if raw_trimmed.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        let rest = ["https://", "http://", "ssh://"]
            .iter()
            .find_map(|scheme| raw_trimmed.strip_prefix(scheme))
            .map(|r| r.to_string())
            .or_else(|| scp_style(raw_trimmed))
            .ok_or_else(invalid)?;
        let rest = rest.rsplit_once('@').map_or(rest.as_str(), |(_, r)| r);
        let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
        let host = host
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let segments: Vec<&str> = path.split('/').collect();
        let bad_segment = |s: &&str| s.is_empty() || *s == "." || *s == "..";
        if host.is_empty() || segments.len() < 2 || segments.iter().any(bad_segment) {
            return Err(invalid());
        }
        Ok(Self(format!("https://{host}/{}", segments.join("/"))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `git@host:owner/name` as `host/owner/name`, or `None` when it is not one.
fn scp_style(raw: &str) -> Option<String> {
    if raw.contains("://") {
        return None;
    }
    let (user_host, path) = raw.split_once(':')?;
    let host = user_host.rsplit_once('@').map_or(user_host, |(_, h)| h);
    (!host.is_empty() && !path.is_empty()).then(|| format!("{host}/{path}"))
}

/// A path relative to a repository root, `/`-separated, never escaping it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid =
            |why: &str| StoreError::Invalid(format!("'{raw}' is not a repository path: {why}"));
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(invalid("empty"));
        }
        if trimmed.contains('\\') {
            return Err(invalid("use '/' separators"));
        }
        if trimmed.starts_with('/') || trimmed.chars().nth(1) == Some(':') {
            return Err(invalid("must be relative"));
        }
        if trimmed
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(invalid("empty, '.' or '..' segment"));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A rusty_tick list reference, `tick:list:<uuid>` (ADR-0012's `tick:` scheme).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TickListId(String);

impl TickListId {
    const PREFIX: &'static str = "tick:list:";

    pub fn parse(raw: &str) -> Result<Self> {
        let invalid =
            || StoreError::Invalid(format!("'{raw}' is not a tick:list:<uuid> reference"));
        let id = raw.trim().strip_prefix(Self::PREFIX).ok_or_else(invalid)?;
        let uuid = uuid::Uuid::parse_str(id).map_err(|_| invalid())?;
        Ok(Self(format!("{}{}", Self::PREFIX, uuid.hyphenated())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One validated claim value, tagged by its predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "predicate", content = "value", rename_all = "snake_case")]
pub enum ClaimValue {
    Stage(Stage),
    Status(Status),
    Repo(RepoUrl),
    Path(RepoPath),
    Tracker(TickListId),
    /// The parent project's name.
    Parent(String),
}

impl ClaimValue {
    /// Validate `value` for `predicate`, the boundary every caller crosses.
    pub fn parse(predicate: Predicate, value: &str) -> Result<Self> {
        Ok(match predicate {
            Predicate::Stage => Self::Stage(value.parse()?),
            Predicate::Status => Self::Status(value.parse()?),
            Predicate::Repo => Self::Repo(RepoUrl::parse(value)?),
            Predicate::Path => Self::Path(RepoPath::parse(value)?),
            Predicate::Tracker => Self::Tracker(TickListId::parse(value)?),
            Predicate::Parent => {
                let name = value.split_whitespace().collect::<Vec<_>>().join(" ");
                if name.is_empty() {
                    return Err(StoreError::Invalid("parent project name is empty".into()));
                }
                Self::Parent(name)
            }
        })
    }

    pub fn predicate(&self) -> Predicate {
        match self {
            Self::Stage(_) => Predicate::Stage,
            Self::Status(_) => Predicate::Status,
            Self::Repo(_) => Predicate::Repo,
            Self::Path(_) => Predicate::Path,
            Self::Tracker(_) => Predicate::Tracker,
            Self::Parent(_) => Predicate::Parent,
        }
    }

    /// The stored object.
    pub fn object(&self) -> String {
        match self {
            Self::Stage(s) => s.as_str().to_string(),
            Self::Status(s) => s.as_str().to_string(),
            Self::Repo(r) => r.as_str().to_string(),
            Self::Path(p) => p.as_str().to_string(),
            Self::Tracker(t) => t.as_str().to_string(),
            Self::Parent(name) => name.clone(),
        }
    }

    /// Refuse a value that did not come out of [`ClaimValue::parse`], such as
    /// a hand-built `Parent("")`: the variants are public, so `set` checks.
    fn validated(&self) -> Result<()> {
        if ClaimValue::parse(self.predicate(), &self.object())? != *self {
            return Err(StoreError::Invalid(format!(
                "{} '{}' is not in canonical form",
                self.predicate().qualified(),
                self.object()
            )));
        }
        Ok(())
    }

    /// Equality as the predicate defines it. A parent is an entity name, so
    /// it compares the way entity names do; every other value is already
    /// canonical, and a path stays case-sensitive.
    pub fn same_as(&self, other: &ClaimValue) -> bool {
        match (self, other) {
            (Self::Parent(a), Self::Parent(b)) => {
                normalize_entity_name(a) == normalize_entity_name(b)
            }
            _ => self == other,
        }
    }

    /// The value a stored claim memory holds, `None` when it does not parse.
    fn of(memory: &Memory) -> Option<Self> {
        let predicate = Predicate::from_qualified(pred(memory)).ok()?;
        ClaimValue::parse(predicate, memory.object.as_deref()?).ok()
    }
}

/// The memory ids behind a claim. Never empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Evidence(Vec<String>);

impl Evidence {
    /// `None` when no non-blank id remains. Order-preserving, de-duplicated.
    pub fn new(ids: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut seen = HashSet::new();
        let ids: Vec<String> = ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && seen.insert(id.clone()))
            .collect();
        (!ids.is_empty()).then_some(Self(ids))
    }

    pub fn ids(&self) -> &[String] {
        &self.0
    }
}

/// What a claim memory's metadata records under `project_claim`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimMeta {
    evidence: Vec<String>,
    confirmed_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reverts: Option<String>,
}

impl ClaimMeta {
    fn from_metadata(metadata: &Value) -> std::result::Result<Self, String> {
        let block = metadata
            .get(META_KEY)
            .ok_or_else(|| format!("metadata has no '{META_KEY}' block"))?;
        serde_json::from_value(block.clone()).map_err(|e| format!("bad '{META_KEY}': {e}"))
    }
}

/// A value and why it is believed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Claim<T> {
    pub value: T,
    pub evidence: Evidence,
    /// The claim memory itself.
    pub claim_id: String,
    pub confirmed_at: String,
    pub confirmed_by: String,
    /// The claim this one compensates for, when it is a reversal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
}

/// Where a project lives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Anchors {
    pub repo: Option<Claim<RepoUrl>>,
    pub path: Option<Claim<RepoPath>>,
    pub tracker: Option<Claim<TickListId>>,
    pub parent: Option<Claim<String>>,
}

/// The entity a project is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityRef {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
}

/// One project, as its live claims describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    pub entity: EntityRef,
    pub stage: Claim<Stage>,
    pub status: Claim<Status>,
    pub anchors: Anchors,
}

/// A claim or project the projection could not build, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidRecord {
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    pub reason: String,
}

/// Every project, by stage then name, and everything that failed to project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProjectList {
    pub projects: Vec<Project>,
    pub invalid: Vec<InvalidRecord>,
}

/// One claim in a project's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimRecord {
    pub claim_id: String,
    pub predicate: String,
    pub value: String,
    pub evidence: Vec<String>,
    pub confirmed_by: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
}

/// A request to record claims about one project.
#[derive(Debug, Clone)]
pub struct SetInput {
    pub project: String,
    pub claims: Vec<ClaimValue>,
    /// Ids of live memories supporting every claim in this call.
    pub evidence: Vec<String>,
    /// A sentence saved as a fact memory and cited as evidence.
    pub because: Option<String>,
    pub confirmed_by: String,
    /// The proposal key, when this confirms one.
    pub proposal: Option<String>,
}

/// A request to undo the current claim `claim_id`.
#[derive(Debug, Clone)]
pub struct RevertInput {
    pub project: String,
    pub claim_id: String,
    pub because: String,
    pub confirmed_by: String,
}

/// What a write did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WriteOutcome {
    /// The project's canonical name.
    pub project: String,
    /// New claim memory ids.
    pub written: Vec<String>,
    /// Predicates whose live value already matched.
    pub unchanged: Vec<Predicate>,
    /// Claim ids the new claims superseded.
    pub superseded: Vec<String>,
    /// The memory the `because` sentence was saved as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because_id: Option<String>,
}

// --- guards on the generic write paths -------------------------------------

/// Whether `predicate` belongs to this module.
pub fn is_reserved_predicate(predicate: &str) -> bool {
    normalize_entity_name(predicate).starts_with(PREDICATE_PREFIX)
}

/// Refuse a generic write that names a `project.*` predicate.
pub fn refuse_reserved_predicate(predicate: Option<&str>) -> Result<()> {
    match predicate {
        Some(p) if is_reserved_predicate(p) => Err(StoreError::Invalid(format!(
            "predicate '{p}' is reserved for project records; use remind_me_project"
        ))),
        _ => Ok(()),
    }
}

/// Refuse an edit or delete of a claim memory: claims are append-only.
pub fn refuse_claim_edit(memory: &Memory) -> Result<()> {
    if memory
        .predicate
        .as_deref()
        .is_some_and(is_reserved_predicate)
    {
        return Err(StoreError::Invalid(format!(
            "memory {} is a project claim and cannot be edited or deleted; use remind_me_project revert",
            memory.id
        )));
    }
    Ok(())
}

// --- reads -----------------------------------------------------------------

/// Every project and every record that failed to project.
pub fn list(store: &Store<'_>) -> Result<ProjectList> {
    let mut out = ProjectList::default();
    let (groups, orphans) = claims_by_subject(store)?;
    out.invalid
        .extend(orphans.into_iter().map(|m| InvalidRecord {
            subject: String::new(),
            claim_id: Some(m.id),
            reason: format!("{} claim has no subject", pred_or_none(&m.predicate)),
        }));
    for (_, chain) in groups {
        if let Some(project) = project_from(store, &chain, &mut out.invalid)? {
            out.projects.push(project);
        }
    }
    out.projects.sort_by(|a, b| {
        (a.stage.value, normalize_entity_name(&a.entity.name))
            .cmp(&(b.stage.value, normalize_entity_name(&b.entity.name)))
    });
    Ok(out)
}

/// One project by name or alias. `Ok(None)` when it has no claims; an error
/// naming the reason when its claims do not form a valid project.
pub fn get(store: &Store<'_>, name: &str) -> Result<Option<Project>> {
    let subject = canonical_subject(store, name)?;
    let chain = claims_for(store, &subject)?;
    if chain.is_empty() {
        return Ok(None);
    }
    let mut invalid = Vec::new();
    let project = project_from(store, &chain, &mut invalid)?;
    match invalid.into_iter().next() {
        Some(record) => Err(StoreError::Invalid(format!(
            "project '{}' is invalid: {}",
            record.subject, record.reason
        ))),
        None => Ok(project),
    }
}

/// A project's claims, superseded ones included, oldest first; only
/// `predicate`'s when given.
pub fn history(
    store: &Store<'_>,
    name: &str,
    predicate: Option<Predicate>,
) -> Result<Vec<ClaimRecord>> {
    let subject = canonical_subject(store, name)?;
    let chain = claims_for(store, &subject)?;
    if chain.is_empty() {
        return Err(StoreError::NotFound);
    }
    Ok(chain
        .iter()
        .filter(|m| predicate.is_none_or(|p| Predicate::from_qualified(pred(m)).ok() == Some(p)))
        .map(|m| {
            let meta = ClaimMeta::from_metadata(&m.metadata).ok();
            ClaimRecord {
                claim_id: m.id.clone(),
                predicate: pred(m).to_string(),
                value: m.object.clone().unwrap_or_default(),
                evidence: meta
                    .as_ref()
                    .map(|c| c.evidence.clone())
                    .unwrap_or_default(),
                confirmed_by: meta
                    .as_ref()
                    .map(|c| c.confirmed_by.clone())
                    .unwrap_or_default(),
                created_at: m.created_at.clone(),
                superseded_by: m.superseded_by.clone(),
                reverts: meta.and_then(|c| c.reverts),
            }
        })
        .collect())
}

fn pred(memory: &Memory) -> &str {
    memory.predicate.as_deref().unwrap_or_default()
}

fn pred_or_none(predicate: &Option<String>) -> &str {
    predicate.as_deref().unwrap_or("project.*")
}

/// Claim memories grouped by normalised subject, each oldest first, and the
/// claims with no subject at all, which no project can own.
type ClaimGroups = (BTreeMap<String, Vec<Memory>>, Vec<Memory>);

fn claims_by_subject(store: &Store<'_>) -> Result<ClaimGroups> {
    let mut groups: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
    let mut orphans = Vec::new();
    for memory in Memories::new(store).with_predicate_prefix(PREDICATE_PREFIX)? {
        let subject = memory
            .subject
            .as_deref()
            .map(normalize_entity_name)
            .filter(|s| !s.is_empty());
        match subject {
            Some(subject) => groups.entry(subject).or_default().push(memory),
            None => orphans.push(memory),
        }
    }
    Ok((groups, orphans))
}

fn claims_for(store: &Store<'_>, subject: &str) -> Result<Vec<Memory>> {
    Ok(claims_by_subject(store)?
        .0
        .remove(&normalize_entity_name(subject))
        .unwrap_or_default())
}

/// The entity's canonical name when `name` resolves (by name or alias), else
/// `name` itself.
fn canonical_subject(store: &Store<'_>, name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Invalid("project name is empty".into()));
    }
    Ok(resolve_entity(store, trimmed)?.map_or_else(|| trimmed.to_string(), |e| e.name))
}

/// The newest unsuperseded claim per predicate. `chain` is oldest first, so
/// the last one wins: two nodes that confirmed concurrently agree.
fn live_claims(chain: &[Memory]) -> BTreeMap<String, &Memory> {
    let mut live = BTreeMap::new();
    for memory in chain.iter().filter(|m| m.superseded_by.is_none()) {
        live.insert(normalize_entity_name(pred(memory)), memory);
    }
    live
}

fn typed_claim<T>(
    memory: &Memory,
    parse: impl Fn(&str) -> Result<T>,
) -> std::result::Result<Claim<T>, String> {
    let object = memory.object.as_deref().ok_or("claim has no object")?;
    let value = parse(object).map_err(|e| e.to_string())?;
    let meta = ClaimMeta::from_metadata(&memory.metadata)?;
    let evidence = Evidence::new(meta.evidence).ok_or("claim cites no evidence")?;
    Ok(Claim {
        value,
        evidence,
        claim_id: memory.id.clone(),
        confirmed_at: memory.created_at.clone(),
        confirmed_by: meta.confirmed_by,
        reverts: meta.reverts,
    })
}

/// Build one project from its claim chain, recording what is invalid.
fn project_from(
    store: &Store<'_>,
    chain: &[Memory],
    invalid: &mut Vec<InvalidRecord>,
) -> Result<Option<Project>> {
    let Some(first) = chain.first() else {
        return Ok(None);
    };
    let subject = first.subject.clone().unwrap_or_default();
    let mut report = |claim_id: Option<&str>, reason: String| {
        invalid.push(InvalidRecord {
            subject: subject.clone(),
            claim_id: claim_id.map(str::to_string),
            reason,
        })
    };

    let mut stage = None;
    let mut status = None;
    let mut anchors = Anchors::default();
    for (key, memory) in live_claims(chain) {
        let predicate = match Predicate::from_qualified(&key) {
            Ok(p) => p,
            Err(e) => {
                report(Some(&memory.id), e.to_string());
                continue;
            }
        };
        let id = memory.id.as_str();
        if pred(memory) != predicate.qualified() {
            report(
                Some(id),
                format!(
                    "predicate '{}' is not the canonical '{}'",
                    pred(memory),
                    predicate.qualified()
                ),
            );
            continue;
        }
        let result = match predicate {
            Predicate::Stage => typed_claim(memory, str::parse).map(|c| stage = Some(c)),
            Predicate::Status => typed_claim(memory, str::parse).map(|c| status = Some(c)),
            Predicate::Repo => typed_claim(memory, RepoUrl::parse).map(|c| anchors.repo = Some(c)),
            Predicate::Path => typed_claim(memory, RepoPath::parse).map(|c| anchors.path = Some(c)),
            Predicate::Tracker => {
                typed_claim(memory, TickListId::parse).map(|c| anchors.tracker = Some(c))
            }
            Predicate::Parent => {
                typed_claim(memory, |s| Ok(s.to_string())).map(|c| anchors.parent = Some(c))
            }
        };
        if let Err(reason) = result {
            report(Some(id), format!("{}: {reason}", predicate.qualified()));
        }
    }

    let (Some(stage), Some(status)) = (stage, status) else {
        report(None, "no valid live stage and status claims".to_string());
        return Ok(None);
    };
    Ok(Some(Project {
        entity: entity_ref(store, &subject)?,
        stage,
        status,
        anchors,
    }))
}

fn entity_ref(store: &Store<'_>, subject: &str) -> Result<EntityRef> {
    let id = entity_id(subject);
    Ok(match get_entity_by_id(store, &id)? {
        Some(e) => EntityRef {
            id: e.id,
            name: e.name,
            aliases: e.aliases,
        },
        // Sync can deliver a claim before its entity; the claim still names it.
        None => EntityRef {
            id,
            name: subject.to_string(),
            aliases: Vec::new(),
        },
    })
}

// --- writes ----------------------------------------------------------------

/// Record `input.claims` about one project, each citing the evidence.
///
/// Everything is validated before anything is written: the claims, that no
/// predicate repeats, that each evidence id is a live memory, and that a new
/// project gets both a stage and a status. A claim equal to the live value is
/// skipped and reported in [`WriteOutcome::unchanged`]. The writes happen in
/// one transaction: a failure part way leaves nothing behind.
pub fn set(store: &Store<'_>, input: &SetInput) -> Result<WriteOutcome> {
    store.transaction(|page| set_in(page, input))
}

fn set_in(store: &Store<'_>, input: &SetInput) -> Result<WriteOutcome> {
    let confirmed_by = required(&input.confirmed_by, "confirmed_by")?;
    if input.claims.is_empty() {
        return Err(StoreError::Invalid("no claims given".into()));
    }
    for claim in &input.claims {
        claim.validated()?;
    }
    let mut seen = HashSet::new();
    if let Some(dup) = input.claims.iter().find(|c| !seen.insert(c.predicate())) {
        return Err(StoreError::Invalid(format!(
            "{} given twice",
            dup.predicate().qualified()
        )));
    }

    let subject = canonical_subject(store, &input.project)?;
    refuse_secret(&subject, "project name")?;
    if let Some(ClaimValue::Parent(parent)) = input
        .claims
        .iter()
        .find(|c| c.predicate() == Predicate::Parent)
    {
        refuse_secret(parent, "parent name")?;
        if normalize_entity_name(parent) == normalize_entity_name(&subject) {
            return Err(StoreError::Invalid(
                "a project cannot be its own parent".into(),
            ));
        }
    }

    let chain = claims_for(store, &subject)?;
    let live = live_claims(&chain);
    let has_stage = live.contains_key(&Predicate::Stage.qualified());
    let gives = |p: Predicate| seen.contains(&p);
    if !has_stage && !(gives(Predicate::Stage) && gives(Predicate::Status)) {
        return Err(StoreError::Invalid(format!(
            "'{subject}' is not a project yet: give both project.stage and project.status"
        )));
    }

    check_live(store, &input.evidence)?;
    let because = input
        .because
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if input.evidence.iter().all(|id| id.trim().is_empty()) && because.is_none() {
        return Err(StoreError::Invalid(
            "give evidence ids or a because sentence".into(),
        ));
    }

    // Unchanged only when the live claim is canonical and holds the same
    // value; a miscased predicate is replaced so it stops being reported.
    let changed: Vec<&ClaimValue> = input
        .claims
        .iter()
        .filter(|c| {
            live.get(&c.predicate().qualified()).is_none_or(|m| {
                pred(m) != c.predicate().qualified()
                    || ClaimValue::of(m).is_none_or(|v| !v.same_as(c))
            })
        })
        .collect();
    let mut outcome = WriteOutcome {
        project: subject.clone(),
        unchanged: input
            .claims
            .iter()
            .map(ClaimValue::predicate)
            .filter(|p| !changed.iter().any(|c| c.predicate() == *p))
            .collect(),
        ..WriteOutcome::default()
    };
    if changed.is_empty() {
        return Ok(outcome);
    }

    let entity = upsert_entity(
        store,
        &EntityInput {
            name: subject.clone(),
            kind: None,
            aliases: Vec::new(),
        },
    )?;
    let mut ids = input.evidence.clone();
    if let Some(sentence) = because {
        let id = write_because(store, &entity.id, sentence)?;
        ids.push(id.clone());
        outcome.because_id = Some(id);
    }
    let evidence = Evidence::new(ids)
        .ok_or_else(|| StoreError::Invalid("give evidence ids or a because sentence".into()))?;

    for claim in changed {
        let meta = ClaimMeta {
            evidence: evidence.ids().to_vec(),
            confirmed_by: confirmed_by.to_string(),
            proposal: input.proposal.clone(),
            reverts: None,
        };
        let (id, superseded) = write_claim(store, &entity.name, claim, &meta, &chain)?;
        outcome.written.push(id);
        outcome.superseded.extend(superseded);
    }
    Ok(outcome)
}

/// Undo the current claim `input.claim_id` with a compensating claim.
///
/// The new claim restores the most recent earlier value that differs, cites
/// that claim's evidence plus the `because` sentence, and names the claim it
/// reverts. Refused when the claim is not current or nothing earlier differs.
/// Like [`set`], it writes in one transaction.
pub fn revert(store: &Store<'_>, input: &RevertInput) -> Result<WriteOutcome> {
    store.transaction(|page| revert_in(page, input))
}

fn revert_in(store: &Store<'_>, input: &RevertInput) -> Result<WriteOutcome> {
    let confirmed_by = required(&input.confirmed_by, "confirmed_by")?;
    let because = required(&input.because, "because")?;
    let subject = canonical_subject(store, &input.project)?;
    let chain = claims_for(store, &subject)?;
    let target = chain
        .iter()
        .find(|m| m.id == input.claim_id)
        .ok_or(StoreError::NotFound)?;
    let key = normalize_entity_name(pred(target));
    let live = live_claims(&chain);
    if live.get(&key).map(|m| m.id.as_str()) != Some(target.id.as_str()) {
        return Err(StoreError::Invalid(format!(
            "claim {} is not the current {key}; revert the current claim",
            target.id
        )));
    }

    // An unparsable current value differs from every valid earlier one.
    let current = ClaimValue::of(target);
    let (earlier, restored) = chain
        .iter()
        .take_while(|m| m.id != target.id)
        .filter(|m| normalize_entity_name(pred(m)) == key)
        .filter_map(|m| ClaimValue::of(m).map(|v| (m, v)))
        .filter(|(_, v)| current.as_ref().is_none_or(|c| !c.same_as(v)))
        .last()
        .ok_or_else(|| StoreError::Invalid(format!("no earlier {key} to restore")))?;
    let earlier_meta = ClaimMeta::from_metadata(&earlier.metadata).map_err(StoreError::Invalid)?;

    let entity = upsert_entity(
        store,
        &EntityInput {
            name: subject.clone(),
            kind: None,
            aliases: Vec::new(),
        },
    )?;
    let because_id = write_because(store, &entity.id, because)?;
    let evidence = Evidence::new(
        earlier_meta
            .evidence
            .into_iter()
            .chain([because_id.clone()]),
    )
    .ok_or_else(|| StoreError::Invalid("revert has no evidence".into()))?;
    let meta = ClaimMeta {
        evidence: evidence.ids().to_vec(),
        confirmed_by: confirmed_by.to_string(),
        proposal: None,
        reverts: Some(target.id.clone()),
    };
    let (id, superseded) = write_claim(store, &entity.name, &restored, &meta, &chain)?;
    Ok(WriteOutcome {
        project: entity.name,
        written: vec![id],
        unchanged: Vec::new(),
        superseded,
        because_id: Some(because_id),
    })
}

fn required<'a>(value: &'a str, field: &str) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Invalid(format!("{field} is required")));
    }
    Ok(trimmed)
}

/// Refuse a name the secret scrubber would rewrite. A name is stored in the
/// claim's subject or object, which are not redacted, so it is refused
/// rather than saved with the secret in it.
fn refuse_secret(name: &str, what: &str) -> Result<()> {
    let mut tags = Vec::new();
    let mut metadata = Value::Null;
    if crate::boundary::scrub(name, &mut tags, &mut metadata) != name {
        return Err(StoreError::Invalid(format!("{what} looks like a secret")));
    }
    Ok(())
}

fn check_live(store: &Store<'_>, ids: &[String]) -> Result<()> {
    let memories = Memories::new(store);
    for id in ids.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
        if memories.get_live(id)?.is_none() {
            return Err(StoreError::Invalid(format!(
                "evidence {id} is not a live memory"
            )));
        }
    }
    Ok(())
}

/// A fact memory written by this module, stamped like any local write, with
/// its content passed through the same secret scrubber as every other write.
fn new_fact(content: &str, mut tags: Vec<String>, mut metadata: Value) -> NewMemory {
    let content = crate::boundary::scrub(content, &mut tags, &mut metadata);
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let decay_rate = get_decay_rate(FACT_CATEGORY);
    let base_weight = get_type_prior(FACT_CATEGORY) * get_source_prior(SOURCE);
    NewMemory {
        category: FACT_CATEGORY.to_string(),
        memory_type: FACT_CATEGORY.to_string(),
        tags,
        source: SOURCE.to_string(),
        metadata,
        decay_rate,
        vitality: calculate_vitality(base_weight, 0, decay_rate, &now_iso, now),
        base_weight,
        accessed_at: Some(now_iso.clone()),
        ..crate::context::default_provenance().stamp(NewMemory::new(
            format!("mem_{}", uuid::Uuid::new_v4().simple()),
            content,
            &now_iso,
        ))
    }
}

fn write_because(store: &Store<'_>, entity_id: &str, sentence: &str) -> Result<String> {
    let row = new_fact(
        sentence,
        vec![EVIDENCE_TAG.to_string()],
        Value::Object(Default::default()),
    );
    Memories::new(store).insert(&row)?;
    link_memory_entity(store, &row.id, entity_id)?;
    Ok(row.id)
}

#[cfg(test)]
thread_local! {
    /// Fail the n-th claim write on this thread (1-based), to prove `set`
    /// and `revert` leave nothing behind when a write fails part way.
    static FAIL_CLAIM_WRITE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Write one claim memory and supersede every other live claim of its
/// predicate in `chain`, the project's claims as read before this write.
fn write_claim(
    store: &Store<'_>,
    subject: &str,
    claim: &ClaimValue,
    meta: &ClaimMeta,
    chain: &[Memory],
) -> Result<(String, Vec<String>)> {
    #[cfg(test)]
    if FAIL_CLAIM_WRITE.with(|n| {
        let left = n.get();
        n.set(left.saturating_sub(1));
        left == 1
    }) {
        return Err(StoreError::Engine("injected claim write failure".into()));
    }
    let predicate = claim.predicate().qualified();
    let object = claim.object();
    let metadata = serde_json::json!({ META_KEY: meta });
    let mut row = new_fact(
        &format!("Project {subject}: {} is {object}", claim.predicate()),
        vec![CLAIM_TAG.to_string()],
        metadata,
    );
    row.subject = Some(subject.to_string());
    row.predicate = Some(predicate.clone());
    row.object = Some(object);
    let memories = Memories::new(store);
    memories.insert(&row)?;

    let mut superseded = Vec::new();
    for old in chain
        .iter()
        .filter(|m| m.superseded_by.is_none())
        .filter(|m| normalize_entity_name(pred(m)) == predicate)
    {
        memories.set_superseded_by(&old.id, &row.id, Some(&row.created_at))?;
        superseded.push(old.id.clone());
    }
    Ok((row.id, superseded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn seed(store: &Store<'_>, content: &str) -> String {
        let row = new_fact(content, Vec::new(), Value::Object(Default::default()));
        Memories::new(store).insert(&row).unwrap();
        row.id
    }

    fn create(store: &Store<'_>, name: &str, stage: Stage, evidence: &str) -> WriteOutcome {
        set(
            store,
            &SetInput {
                project: name.into(),
                claims: vec![ClaimValue::Stage(stage), ClaimValue::Status(Status::Active)],
                evidence: vec![evidence.into()],
                because: None,
                confirmed_by: "human".into(),
                proposal: None,
            },
        )
        .unwrap()
    }

    fn set_one(
        store: &Store<'_>,
        name: &str,
        claim: ClaimValue,
        because: &str,
    ) -> Result<WriteOutcome> {
        set(
            store,
            &SetInput {
                project: name.into(),
                claims: vec![claim],
                evidence: Vec::new(),
                because: Some(because.into()),
                confirmed_by: "human".into(),
                proposal: None,
            },
        )
    }

    // --- pure parsing ------------------------------------------------------

    #[test]
    fn closed_enums_round_trip_and_reject_unknowns() {
        for s in Stage::ALL {
            assert_eq!(s.as_str().parse::<Stage>().unwrap(), *s);
        }
        assert_eq!(" Built ".parse::<Stage>().unwrap(), Stage::Built);
        assert!("shipped".parse::<Stage>().is_err());
        assert!("paused".parse::<Status>().is_err());
        assert_eq!(
            Predicate::from_qualified("Project.Stage").unwrap(),
            Predicate::Stage
        );
        assert!(Predicate::from_qualified("project.owner").is_err());
        assert!(Predicate::from_qualified("stage").is_err());
    }

    #[test]
    fn repo_urls_normalise_to_https() {
        let want = "https://github.com/rusty-mill/rusty_mill";
        for raw in [
            "https://github.com/Rusty-Mill/rusty_mill.git",
            "git@github.com:Rusty-Mill/rusty_mill.git",
            "ssh://git@github.com/Rusty-Mill/rusty_mill",
            "http://GitHub.com/Rusty-Mill/rusty_mill/",
        ] {
            assert_eq!(
                RepoUrl::parse(raw).unwrap().as_str().to_lowercase(),
                want,
                "{raw}"
            );
        }
        assert_eq!(
            RepoUrl::parse("https://GITHUB.com/Rusty-Mill/x")
                .unwrap()
                .as_str(),
            "https://github.com/Rusty-Mill/x"
        );
        for bad in [
            "",
            "rusty_mill",
            "https://github.com/only",
            "https://h/a/../b",
            "https://h/a b/c",
        ] {
            assert!(RepoUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_paths_stay_inside_the_repo() {
        assert_eq!(
            RepoPath::parse("crates/apps/rusty_mux/").unwrap().as_str(),
            "crates/apps/rusty_mux"
        );
        for bad in [
            "",
            "/etc",
            "C:/x",
            "crates\\apps",
            "crates/../x",
            "a//b",
            "./a",
        ] {
            assert!(RepoPath::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tick_lists_use_the_tick_scheme() {
        let id = "tick:list:6F9619FF-8B86-D011-B42D-00C04FC964FF";
        assert_eq!(
            TickListId::parse(id).unwrap().as_str(),
            "tick:list:6f9619ff-8b86-d011-b42d-00c04fc964ff"
        );
        assert!(TickListId::parse("tick:6f9619ff-8b86-d011-b42d-00c04fc964ff").is_err());
        assert!(TickListId::parse("tick:list:nope").is_err());
    }

    #[test]
    fn evidence_is_never_empty() {
        assert!(Evidence::new(Vec::<String>::new()).is_none());
        assert!(Evidence::new(vec![" ".to_string()]).is_none());
        let e = Evidence::new(vec!["a".into(), "b".into(), "a".into()]).unwrap();
        assert_eq!(e.ids(), ["a", "b"]);
    }

    #[test]
    fn reserved_predicates_are_refused_case_insensitively() {
        assert!(refuse_reserved_predicate(Some(" Project.stage")).is_err());
        assert!(refuse_reserved_predicate(Some("lives_in")).is_ok());
        assert!(refuse_reserved_predicate(None).is_ok());
    }

    // --- store -------------------------------------------------------------

    #[test]
    fn a_new_project_needs_stage_status_and_evidence() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let only_stage = set_one(
            &store,
            "rusty_mux",
            ClaimValue::Stage(Stage::Idea),
            "said so",
        );
        assert!(only_stage.is_err());

        let no_evidence = set(
            &store,
            &SetInput {
                project: "rusty_mux".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Idea),
                    ClaimValue::Status(Status::Active),
                ],
                evidence: Vec::new(),
                because: Some("  ".into()),
                confirmed_by: "human".into(),
                proposal: None,
            },
        );
        assert!(no_evidence.is_err());

        let missing = set(
            &store,
            &SetInput {
                project: "rusty_mux".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Idea),
                    ClaimValue::Status(Status::Active),
                ],
                evidence: vec!["mem_missing".into()],
                because: None,
                confirmed_by: "human".into(),
                proposal: None,
            },
        );
        assert!(missing.is_err());
        assert!(
            list(&store).unwrap().projects.is_empty(),
            "nothing written on refusal"
        );
    }

    #[test]
    fn set_projects_a_typed_record_with_evidence() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "we keep talking about a terminal mux");
        let created = create(&store, "rusty_mux", Stage::Idea, &fact);
        assert_eq!(created.written.len(), 2);

        set_one(
            &store,
            "rusty_mux",
            ClaimValue::Repo(RepoUrl::parse("git@github.com:Rusty-Mill/rusty_mill.git").unwrap()),
            "the mux lives in the monorepo",
        )
        .unwrap();

        let project = get(&store, "Rusty_Mux").unwrap().unwrap();
        assert_eq!(project.stage.value, Stage::Idea);
        assert_eq!(project.status.value, Status::Active);
        assert_eq!(project.stage.evidence.ids(), [fact.as_str()]);
        assert_eq!(
            project.anchors.repo.unwrap().value.as_str(),
            "https://github.com/Rusty-Mill/rusty_mill"
        );
        assert!(get(&store, "nothing").unwrap().is_none());
    }

    #[test]
    fn a_new_stage_supersedes_and_history_keeps_the_old_one() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "idea");
        let created = create(&store, "mill", Stage::Idea, &fact);
        let built = set_one(
            &store,
            "mill",
            ClaimValue::Stage(Stage::Built),
            "repo exists",
        )
        .unwrap();
        assert_eq!(built.superseded, [created.written[0].clone()]);

        let same = set_one(&store, "mill", ClaimValue::Stage(Stage::Built), "again").unwrap();
        assert!(same.written.is_empty());
        assert_eq!(same.unchanged, [Predicate::Stage]);

        let stages = history(&store, "mill", Some(Predicate::Stage)).unwrap();
        let values: Vec<&str> = stages.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["idea", "built"]);
        assert_eq!(
            stages[0].superseded_by.as_deref(),
            Some(built.written[0].as_str())
        );
        assert_eq!(
            get(&store, "mill").unwrap().unwrap().stage.value,
            Stage::Built
        );
    }

    #[test]
    fn revert_writes_a_compensating_claim() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "idea");
        create(&store, "mill", Stage::Idea, &fact);
        let built = set_one(&store, "mill", ClaimValue::Stage(Stage::Built), "oops").unwrap();

        let reverted = revert(
            &store,
            &RevertInput {
                project: "mill".into(),
                claim_id: built.written[0].clone(),
                because: "it was not built yet".into(),
                confirmed_by: "human".into(),
            },
        )
        .unwrap();

        let project = get(&store, "mill").unwrap().unwrap();
        assert_eq!(project.stage.value, Stage::Idea);
        assert_eq!(
            project.stage.reverts.as_deref(),
            Some(built.written[0].as_str())
        );
        assert!(project.stage.evidence.ids().contains(&fact));
        assert!(project
            .stage
            .evidence
            .ids()
            .contains(reverted.because_id.as_ref().unwrap()));
        assert_eq!(
            history(&store, "mill", Some(Predicate::Stage))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn revert_refuses_old_claims_and_first_claims() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "idea");
        let created = create(&store, "mill", Stage::Idea, &fact);
        let input = |claim_id: &str| RevertInput {
            project: "mill".into(),
            claim_id: claim_id.into(),
            because: "undo".into(),
            confirmed_by: "human".into(),
        };
        assert!(
            revert(&store, &input(&created.written[0])).is_err(),
            "nothing earlier"
        );

        set_one(&store, "mill", ClaimValue::Stage(Stage::Built), "built").unwrap();
        assert!(
            revert(&store, &input(&created.written[0])).is_err(),
            "not current"
        );
        assert!(matches!(
            revert(&store, &input("mem_nope")),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn a_malformed_synced_claim_is_reported_not_dropped() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let mut row = new_fact("bad", vec![CLAIM_TAG.into()], serde_json::json!({}));
        row.subject = Some("ghost".into());
        row.predicate = Some("project.stage".into());
        row.object = Some("shipped".into());
        Memories::new(&store).insert(&row).unwrap();

        let listed = list(&store).unwrap();
        assert!(listed.projects.is_empty());
        assert!(listed
            .invalid
            .iter()
            .any(|r| r.claim_id.as_deref() == Some(row.id.as_str())));
        assert!(listed
            .invalid
            .iter()
            .any(|r| r.claim_id.is_none() && r.subject == "ghost"));
        assert!(get(&store, "ghost").is_err());
    }

    #[test]
    fn list_orders_by_stage_then_name() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "zeta", Stage::Idea, &fact);
        create(&store, "alpha", Stage::Built, &fact);
        create(&store, "beta", Stage::Idea, &fact);
        let names: Vec<String> = list(&store)
            .unwrap()
            .projects
            .into_iter()
            .map(|p| p.entity.name)
            .collect();
        assert_eq!(names, ["beta", "zeta", "alpha"]);
    }

    #[test]
    fn generic_write_paths_cannot_forge_or_edit_claims() {
        use crate::db::queries::{add_memory, annotate_memories, update_memory};
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        let claim_id = create(&store, "mill", Stage::Idea, &fact).written[0].clone();

        let forged = serde_json::from_value(serde_json::json!({
            "content": "mill is built", "category": "fact",
            "subject": "mill", "predicate": "project.stage", "object": "built",
        }))
        .unwrap();
        assert!(add_memory(&store, forged).is_err());

        let edit = serde_json::from_value(serde_json::json!({
            "memory_id": claim_id, "clear_superseded": true,
        }))
        .unwrap();
        assert!(update_memory(&store, &edit).is_err());

        let annotate = serde_json::from_value(serde_json::json!({ "annotations": [
            { "memory_id": fact, "subject": "mill", "predicate": "Project.Stage", "object": "built" },
            { "memory_id": claim_id, "object": "built" },
        ]}))
        .unwrap();
        let result = annotate_memories(&store, &annotate).unwrap();
        assert_eq!(result.errors.len(), 2);
        assert_eq!(
            get(&store, "mill").unwrap().unwrap().stage.value,
            Stage::Idea
        );
    }

    #[test]
    fn a_project_cannot_parent_itself() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);
        assert!(set_one(&store, "mill", ClaimValue::Parent("Mill".into()), "x").is_err());
    }

    // --- review round 2 ----------------------------------------------------

    /// A synthetic AWS key id: the redaction rules' own fixture shape.
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

    #[test]
    fn a_secret_in_because_is_redacted_and_one_in_a_name_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);

        let because = format!("deploy key {SECRET} proves it");
        let built = set_one(&store, "mill", ClaimValue::Stage(Stage::Built), &because).unwrap();
        let saved = Memories::new(&store)
            .get_live(built.because_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert!(!saved.content.contains(SECRET), "{}", saved.content);
        assert!(saved.content.contains("[REDACTED:"));

        let named = set_one(&store, SECRET, ClaimValue::Stage(Stage::Idea), "x");
        assert!(named.is_err());
        let parent = set_one(&store, "mill", ClaimValue::Parent(SECRET.into()), "x");
        assert!(parent.is_err());
        let all = Memories::new(&store).all_live().unwrap();
        assert!(all.iter().all(|m| !m.content.contains(SECRET)
            && m.subject.as_deref().is_none_or(|s| !s.contains(SECRET))
            && m.object.as_deref().is_none_or(|o| !o.contains(SECRET))));
    }

    #[test]
    fn a_failed_write_part_way_leaves_nothing_behind() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);
        let before = Memories::new(&store).all_live().unwrap().len();

        // The second of two claims fails: the first claim, its supersession
        // and the because sentence must all roll back.
        FAIL_CLAIM_WRITE.with(|n| n.set(2));
        let failed = set(
            &store,
            &SetInput {
                project: "mill".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Built),
                    ClaimValue::Status(Status::Dormant),
                ],
                evidence: Vec::new(),
                because: Some("both changed".into()),
                confirmed_by: "human".into(),
                proposal: None,
            },
        );
        assert!(failed.is_err());
        assert_eq!(Memories::new(&store).all_live().unwrap().len(), before);
        let project = get(&store, "mill").unwrap().unwrap();
        assert_eq!(project.stage.value, Stage::Idea);
        assert_eq!(project.status.value, Status::Active);

        let built = set_one(&store, "mill", ClaimValue::Stage(Stage::Built), "b").unwrap();
        let count = Memories::new(&store).all_live().unwrap().len();
        FAIL_CLAIM_WRITE.with(|n| n.set(1));
        let reverted = revert(
            &store,
            &RevertInput {
                project: "mill".into(),
                claim_id: built.written[0].clone(),
                because: "undo".into(),
                confirmed_by: "human".into(),
            },
        );
        assert!(reverted.is_err());
        assert_eq!(Memories::new(&store).all_live().unwrap().len(), count);
        assert_eq!(
            get(&store, "mill").unwrap().unwrap().stage.value,
            Stage::Built
        );
    }

    #[test]
    fn delete_tagging_and_consolidation_leave_claims_alone() {
        use crate::db::queries::{bulk_delete, bulk_tag, delete_memory};
        use crate::db::vectors::Vectors;
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        let claims = create(&store, "mill", Stage::Idea, &fact).written;

        assert!(delete_memory(&store, &claims[0]).is_err());
        assert!(bulk_delete(&store, &[fact.clone(), claims[1].clone()]).is_err());
        assert!(
            Memories::new(&store).get_live(&fact).unwrap().is_some(),
            "a refused batch deletes nothing"
        );
        let tag = serde_json::from_value(serde_json::json!({
            "ids": [claims[0]], "tags": ["x"], "mode": "set",
        }))
        .unwrap();
        assert!(bulk_tag(&store, &tag).is_err());

        let vectors = Vectors::new(&store);
        for id in claims.iter().chain([&fact]) {
            vectors.put(id, 0, &[1, 0, 0, 0]).unwrap();
        }
        let candidates: Vec<String> = vectors
            .consolidation_candidates(None, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(candidates, [fact.as_str()]);

        assert!(
            delete_memory(&store, &fact).unwrap(),
            "evidence still deletes"
        );
        let project = get(&store, "mill").unwrap().unwrap();
        assert_eq!(project.stage.value, Stage::Idea);
        assert_eq!(project.status.value, Status::Active);
    }

    #[test]
    fn paths_compare_case_sensitively_and_parents_do_not() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);
        let path = |p: &str| ClaimValue::Path(RepoPath::parse(p).unwrap());

        set_one(&store, "mill", path("crates/Foo"), "a").unwrap();
        let lower = set_one(&store, "mill", path("crates/foo"), "b").unwrap();
        assert_eq!(lower.written.len(), 1, "a case change is a change");
        assert_eq!(lower.superseded.len(), 1);

        let undone = revert(
            &store,
            &RevertInput {
                project: "mill".into(),
                claim_id: lower.written[0].clone(),
                because: "it was Foo".into(),
                confirmed_by: "human".into(),
            },
        )
        .unwrap();
        assert_eq!(undone.superseded, lower.written);
        let anchors = get(&store, "mill").unwrap().unwrap().anchors;
        assert_eq!(anchors.path.unwrap().value.as_str(), "crates/Foo");

        set_one(&store, "mill", ClaimValue::Parent("rusty_mill".into()), "c").unwrap();
        let same = set_one(&store, "mill", ClaimValue::Parent("Rusty_Mill".into()), "d").unwrap();
        assert_eq!(same.unchanged, [Predicate::Parent]);
    }

    #[test]
    fn hand_built_values_are_revalidated_before_any_write() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);
        let before = Memories::new(&store).all_live().unwrap().len();
        for bad in [
            ClaimValue::Parent(String::new()),
            ClaimValue::Parent("  two   spaces ".into()),
            ClaimValue::Path(RepoPath("../escape".into())),
            ClaimValue::Repo(RepoUrl("not a url".into())),
            ClaimValue::Tracker(TickListId("tick:list:nope".into())),
        ] {
            assert!(
                set_one(&store, "mill", bad.clone(), "x").is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(Memories::new(&store).all_live().unwrap().len(), before);
    }

    #[test]
    fn subjectless_and_miscased_claims_are_reported() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = seed(&store, "seen");
        create(&store, "mill", Stage::Idea, &fact);
        let synced = |subject: Option<&str>, predicate: &str, object: &str| {
            let meta =
                serde_json::json!({ META_KEY: { "evidence": [fact], "confirmed_by": "peer" } });
            let mut row = new_fact("synced", vec![CLAIM_TAG.into()], meta);
            row.subject = subject.map(str::to_string);
            row.predicate = Some(predicate.into());
            row.object = Some(object.into());
            Memories::new(&store).insert(&row).unwrap();
            row.id
        };
        let orphan = synced(None, "project.stage", "built");
        let blank = synced(Some("  "), "project.stage", "built");
        let miscased = synced(Some("mill"), "Project.Status", "dormant");

        let listed = list(&store).unwrap();
        let reported = |id: &str| {
            listed
                .invalid
                .iter()
                .any(|r| r.claim_id.as_deref() == Some(id))
        };
        assert!(reported(&orphan) && reported(&blank) && reported(&miscased));
        assert!(listed.projects.is_empty(), "mill has no valid live status");

        // Confirming a status supersedes every live status claim, the
        // miscased one included, and the report clears.
        let fixed = set_one(&store, "mill", ClaimValue::Status(Status::Dormant), "ok").unwrap();
        assert_eq!(fixed.superseded.len(), 2);
        assert!(fixed.superseded.contains(&miscased));
        let listed = list(&store).unwrap();
        assert_eq!(listed.projects.len(), 1);
        assert!(!listed
            .invalid
            .iter()
            .any(|r| r.claim_id.as_deref() == Some(miscased.as_str())));
    }
}

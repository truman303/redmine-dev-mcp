use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::client::Client;
use crate::config::{FieldRef, REDMINE_PAGE_MAX, RESERVED_FIELD_KEYS};
use crate::error::RedmineError;
use crate::scan::ScanCache;

/// Stop paging rather than silently drop the tail. 50 pages, same count as the
/// default scan cap.
const LIST_CAP: u64 = 5_000;

/// A failed refresh should not retry on every filter call that arrives during a blip.
const REFRESH_BACKOFF: Duration = Duration::from_secs(30);

/// A custom field added during the TTL is invisible until the next load. A miss
/// forces one refresh, then waits out this interval before trying again.
const FIELD_MISS_REFRESH: Duration = Duration::from_secs(60);

/// A custom field before it has a key. `multiple` stays unknown when the source
/// doesn't say: the project include never does, and an issue only sets it on a
/// multi-value field that currently has a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawField {
    pub id: u64,
    pub label: String,
    pub multiple: Option<bool>,
    /// Whether Redmine accepts this field as an issue-list filter: `is_filter`
    /// and `visible` from `/custom_fields.json`. `None` from the other sources,
    /// which don't say. Redmine silently drops a filter on a field that fails
    /// either, so a `Some(false)` field is refused rather than sent.
    pub filterable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub id: u64,
    pub name: String,
    pub is_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub id: u64,
    pub name: String,
    pub project_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub id: u64,
    pub key: String,
    pub label: String,
    pub multiple: Option<bool>,
    pub filterable: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource {
    CustomFieldsEndpoint,
    ProjectInclude,
    TrackerSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    User,
    Group,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: u64,
    pub name: String,
    pub kind: MemberKind,
}

/// What a status filter becomes. `open` and `closed` stay as Redmine sends them
/// until they are combined with another part, which Redmine cannot express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Closed,
    Any,
    Ids(Vec<u64>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    /// The config file names something this Redmine doesn't have. Cached for the TTL.
    #[error("{0}")]
    Bind(String),
    /// A page was larger than [`LIST_CAP`], or a body was the wrong shape.
    #[error("{0}")]
    Message(String),
    /// A filter named nothing, or more than one thing. The message lists the valid names.
    #[error("{0}")]
    Name(String),
    #[error(transparent)]
    Redmine(#[from] RedmineError),
}

impl CatalogError {
    fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

/// Statuses, trackers, priorities, queries and custom fields for one Redmine.
/// Categories and members are not here; they are loaded per project.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub statuses: Vec<Status>,
    pub trackers: Vec<Named>,
    pub priorities: Vec<Named>,
    pub queries: Vec<Query>,
    pub fields: Vec<Field>,
    pub field_source: FieldSource,
    pub unsampled_trackers: Vec<Named>,
    /// Configured status groups, resolved to ids.
    pub groups: BTreeMap<String, Vec<u64>>,
    pub loaded_at: Instant,
}

/// The names `get_issue` should use. `assigned` is missing when the catalog
/// could not be loaded or bound; configured keys still apply, and every other
/// field is shown as `cf_<id>`.
#[derive(Debug, Clone)]
pub struct IssueNames {
    pub assigned: Option<BTreeMap<u64, String>>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub catalog: Catalog,
    pub(crate) scans: ScanCache,
}

impl Ctx {
    pub fn new(client: Client) -> Self {
        let catalog = Catalog::new(client.clone());
        Self {
            client,
            catalog,
            scans: ScanCache::new(),
        }
    }
}

/// Shared catalog. `Client` stays a transport; this owns the cache.
///
/// One load runs at a time. A failed refresh keeps serving the previous good
/// snapshot, and a bind failure is remembered for the TTL because the config
/// file does not change until the process restarts.
#[derive(Clone)]
pub struct Catalog {
    client: Client,
    inner: Arc<Mutex<Inner>>,
    flight: Arc<Mutex<()>>,
}

struct Inner {
    ready: Option<Arc<Snapshot>>,
    bind_error: Option<(String, Instant)>,
    retry_after: Option<Instant>,
    /// The last load that failed with no snapshot to fall back on. `get` ignores
    /// it, since a filter call that would fail anyway may as well try again.
    /// `issue_names` honours it, so a down catalog doesn't add a full load to
    /// every issue read.
    cold_failure: Option<(String, Instant)>,
    /// When a custom-field miss last forced a reload. At most once a minute.
    early_refresh: Option<Instant>,
    /// Whether Redmine applies a filter on (field id, project). Learned by
    /// probing; see `filters::check_applied`.
    applied: HashMap<(u64, String), (Instant, bool)>,
    categories: HashMap<String, (Instant, Vec<Named>)>,
    members: HashMap<String, (Instant, Vec<Member>)>,
}

impl Catalog {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            inner: Arc::new(Mutex::new(Inner {
                ready: None,
                bind_error: None,
                retry_after: None,
                cold_failure: None,
                early_refresh: None,
                applied: HashMap::new(),
                categories: HashMap::new(),
                members: HashMap::new(),
            })),
            flight: Arc::new(Mutex::new(())),
        }
    }

    /// The bound snapshot. Filter tools call the resolvers on it, which do not fetch.
    pub async fn get(&self) -> Result<Arc<Snapshot>, CatalogError> {
        if let Some(hit) = self.fresh().await {
            return hit;
        }
        let _flight = self.flight.lock().await;
        if let Some(hit) = self.fresh().await {
            return hit;
        }
        self.commit(self.load().await).await
    }

    /// The field a filter named, refreshing once a minute when the name is new.
    ///
    /// An empty name or an ambiguous one fails immediately. A name that matches
    /// nothing reloads the catalog, in case the field was added during the TTL,
    /// and then resolves again.
    pub async fn resolve_custom_field(&self, input: &str) -> Result<Field, CatalogError> {
        let snapshot = self.get().await?;
        match snapshot.find_custom_field(input) {
            Ok(field) => Ok(field.clone()),
            Err(FieldMiss::Rejected(message)) => Err(CatalogError::Name(message)),
            Err(FieldMiss::Unknown(_)) => {
                let snapshot = self.refresh_after_field_miss().await?;
                snapshot.resolve_custom_field(input).cloned()
            }
        }
    }

    async fn commit(
        &self,
        loaded: Result<Snapshot, CatalogError>,
    ) -> Result<Arc<Snapshot>, CatalogError> {
        match loaded {
            Ok(snapshot) => {
                let snapshot = Arc::new(snapshot);
                let mut inner = self.inner.lock().await;
                inner.ready = Some(Arc::clone(&snapshot));
                inner.bind_error = None;
                inner.retry_after = None;
                inner.cold_failure = None;
                Ok(snapshot)
            }
            Err(CatalogError::Bind(message)) => {
                tracing::warn!("REDMINE_CONFIG does not match this Redmine: {message}");
                let mut inner = self.inner.lock().await;
                // A field that disappeared should not keep answering under the old snapshot.
                inner.ready = None;
                inner.bind_error = Some((message.clone(), Instant::now()));
                inner.retry_after = None;
                // A transport failure just before this would otherwise keep issue
                // reads on the old reason for the rest of the backoff.
                inner.cold_failure = None;
                Err(CatalogError::Bind(message))
            }
            Err(err) => {
                let mut inner = self.inner.lock().await;
                if let Some(snapshot) = inner.ready.clone() {
                    tracing::warn!("catalog refresh failed, serving the previous snapshot: {err}");
                    inner.retry_after = Some(Instant::now() + REFRESH_BACKOFF);
                    return Ok(snapshot);
                }
                tracing::warn!("catalog load failed: {err}");
                inner.cold_failure = Some((err.to_string(), Instant::now()));
                Err(err)
            }
        }
    }

    /// One reload per minute after a custom-field miss. Callers that arrive
    /// while that reload is in flight share it, then see the same snapshot.
    async fn refresh_after_field_miss(&self) -> Result<Arc<Snapshot>, CatalogError> {
        let _flight = self.flight.lock().await;
        if let Some(hit) = self.cached_field_refresh().await {
            return hit;
        }
        let loaded = self.load().await;
        let result = self.commit(loaded).await;
        self.inner.lock().await.early_refresh = Some(Instant::now());
        result
    }

    async fn cached_field_refresh(&self) -> Option<Result<Arc<Snapshot>, CatalogError>> {
        let inner = self.inner.lock().await;
        let at = inner.early_refresh?;
        if at.elapsed() >= FIELD_MISS_REFRESH {
            return None;
        }
        if let Some(snapshot) = &inner.ready {
            return Some(Ok(Arc::clone(snapshot)));
        }
        if let Some((message, _)) = &inner.bind_error {
            return Some(Err(CatalogError::Bind(message.clone())));
        }
        if let Some((message, _)) = &inner.cold_failure {
            return Some(Err(CatalogError::Message(message.clone())));
        }
        None
    }

    /// Never fails the caller. `get_issue` uses this so a down catalog still returns the issue.
    ///
    /// The reason is kept short. A bind error lists every field label, which on a
    /// big instance would be kilobytes added to each issue read. The full text is
    /// in the log, and the filter tools return it.
    pub async fn issue_names(&self) -> IssueNames {
        if let Some(reason) = self.recent_cold_failure().await {
            return IssueNames {
                assigned: None,
                error: Some(short_reason(&reason)),
            };
        }
        match self.get().await {
            Ok(snapshot) => IssueNames {
                assigned: Some(
                    snapshot
                        .fields
                        .iter()
                        .map(|field| (field.id, field.key.clone()))
                        .collect(),
                ),
                error: None,
            },
            Err(CatalogError::Bind(_)) => IssueNames {
                assigned: None,
                error: Some(
                    "REDMINE_CONFIG does not match this Redmine. The server log has the details."
                        .to_string(),
                ),
            },
            Err(err) => IssueNames {
                assigned: None,
                error: Some(short_reason(&err.to_string())),
            },
        }
    }

    /// A cached answer to "does Redmine apply a filter on this field in this
    /// project?", or `None` when it hasn't been asked within the TTL.
    pub async fn filter_applied(&self, field_id: u64, project: &str) -> Option<bool> {
        let inner = self.inner.lock().await;
        let (at, applied) = inner.applied.get(&(field_id, project.to_string()))?;
        (at.elapsed() < self.client.config().catalog_ttl).then_some(*applied)
    }

    pub async fn remember_filter_applied(&self, field_id: u64, project: &str, applied: bool) {
        self.inner
            .lock()
            .await
            .applied
            .insert((field_id, project.to_string()), (Instant::now(), applied));
    }

    async fn recent_cold_failure(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        let (message, at) = inner.cold_failure.as_ref()?;
        (at.elapsed() < REFRESH_BACKOFF).then(|| message.clone())
    }

    pub async fn categories(&self, project_id: &str) -> Result<Vec<Named>, CatalogError> {
        if let Some(hit) = self.cached_categories(project_id).await {
            return Ok(hit);
        }
        match fetch_categories(&self.client, project_id).await {
            Ok(items) => {
                self.store_categories(project_id, items.clone()).await;
                Ok(items)
            }
            Err(err) if is_absent(&err) => {
                self.store_categories(project_id, Vec::new()).await;
                Ok(Vec::new())
            }
            Err(err) => Err(err),
        }
    }

    /// Membership errors are not cached. An empty list would make every name look like a real miss.
    pub async fn members(&self, project_id: &str) -> Result<Vec<Member>, CatalogError> {
        if let Some(hit) = self.cached_members(project_id).await {
            return Ok(hit);
        }
        let items = fetch_members(&self.client, project_id).await?;
        self.store_members(project_id, items.clone()).await;
        Ok(items)
    }

    pub async fn resolve_category(
        &self,
        project_id: &str,
        input: &str,
    ) -> Result<Vec<u64>, CatalogError> {
        let items = self.categories(project_id).await?;
        resolve_parts(input, &items, "category", |item| &item.name, |item| item.id)
    }

    pub async fn resolve_member(
        &self,
        project_id: &str,
        input: &str,
    ) -> Result<Vec<u64>, CatalogError> {
        let items = self.members(project_id).await?;
        resolve_parts(input, &items, "member", |item| &item.name, |item| item.id)
    }

    async fn fresh(&self) -> Option<Result<Arc<Snapshot>, CatalogError>> {
        let inner = self.inner.lock().await;
        let ttl = self.client.config().catalog_ttl;
        let now = Instant::now();
        if let Some((message, at)) = &inner.bind_error
            && now.duration_since(*at) < ttl
        {
            return Some(Err(CatalogError::Bind(message.clone())));
        }
        let Some(snapshot) = &inner.ready else {
            return None;
        };
        if now.duration_since(snapshot.loaded_at) < ttl {
            return Some(Ok(Arc::clone(snapshot)));
        }
        if inner.retry_after.is_some_and(|until| now < until) {
            return Some(Ok(Arc::clone(snapshot)));
        }
        None
    }

    async fn cached_categories(&self, project_id: &str) -> Option<Vec<Named>> {
        let inner = self.inner.lock().await;
        let (at, items) = inner.categories.get(project_id)?;
        if Instant::now().duration_since(*at) < self.client.config().catalog_ttl {
            Some(items.clone())
        } else {
            None
        }
    }

    async fn cached_members(&self, project_id: &str) -> Option<Vec<Member>> {
        let inner = self.inner.lock().await;
        let (at, items) = inner.members.get(project_id)?;
        if Instant::now().duration_since(*at) < self.client.config().catalog_ttl {
            Some(items.clone())
        } else {
            None
        }
    }

    async fn store_categories(&self, project_id: &str, items: Vec<Named>) {
        self.inner
            .lock()
            .await
            .categories
            .insert(project_id.to_string(), (Instant::now(), items));
    }

    async fn store_members(&self, project_id: &str, items: Vec<Member>) {
        self.inner
            .lock()
            .await
            .members
            .insert(project_id.to_string(), (Instant::now(), items));
    }

    async fn load(&self) -> Result<Snapshot, CatalogError> {
        let client = &self.client;
        let (statuses, trackers, priorities, queries) = tokio::join!(
            load_statuses(client),
            load_trackers(client),
            load_priorities(client),
            load_queries(client),
        );
        let statuses = statuses?;
        let trackers = trackers?;
        let priorities = priorities?;
        let queries = queries?;
        let fields = load_fields(client, &trackers).await?;
        let assigned = assign_keys(&fields.raw, &client.config().fields);
        let groups = bind(
            &assigned,
            &client.config().fields,
            &statuses,
            &client.config().status_groups,
            fields.source,
            &fields.unsampled,
        )?;
        Ok(Snapshot {
            statuses,
            trackers,
            priorities,
            queries,
            fields: assigned,
            field_source: fields.source,
            unsampled_trackers: fields.unsampled,
            groups,
            loaded_at: Instant::now(),
        })
    }
}

struct FieldList {
    raw: Vec<RawField>,
    source: FieldSource,
    unsampled: Vec<Named>,
}

/// The key a field is shown under, and the key a filter has to use.
///
/// Configured keys are claimed first. Every other field, in id order, gets a
/// slug of its label unless that slug is empty, reserved, configured, or already
/// taken by a lower id. The lower id keeps the slug, so a field created later
/// cannot take a key from an older one.
pub fn assign_keys(fields: &[RawField], config: &BTreeMap<String, FieldRef>) -> Vec<Field> {
    let fields = dedupe_fields(fields);
    let mut key_of: BTreeMap<u64, String> = BTreeMap::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for (key, field_ref) in config {
        let hits: Vec<&RawField> = fields
            .iter()
            .filter(|field| field_matches(field, field_ref))
            .collect();
        if let [field] = hits.as_slice()
            && !key_of.contains_key(&field.id)
        {
            key_of.insert(field.id, key.clone());
            taken.insert(key.clone());
        }
    }
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        let key = if let Some(key) = key_of.get(&field.id) {
            key.clone()
        } else {
            let slug = slug_key(&field.label);
            if slug.is_empty()
                || taken.contains(&slug)
                || RESERVED_FIELD_KEYS.contains(&slug.as_str())
            {
                format!("cf_{}", field.id)
            } else {
                slug
            }
        };
        taken.insert(key.clone());
        out.push(Field {
            id: field.id,
            key,
            label: field.label,
            multiple: field.multiple,
            filterable: field.filterable,
        });
    }
    out
}

/// The key to print for one custom field on an issue.
///
/// A bound catalog wins, including `cf_<id>` for an id it has not seen yet.
/// With no catalog, a configured id or exact label still uses its configured
/// key and everything else is `cf_<id>`. Slugs are not invented here.
pub fn display_key(
    id: u64,
    label: &str,
    configured: &BTreeMap<String, FieldRef>,
    assigned: Option<&BTreeMap<u64, String>>,
) -> String {
    if let Some(assigned) = assigned {
        return assigned
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("cf_{id}"));
    }
    configured
        .iter()
        .find(|(_, field_ref)| match field_ref {
            FieldRef::Id(configured_id) => *configured_id == id,
            FieldRef::Label(configured) => configured == label,
        })
        .map(|(key, _)| key.clone())
        .unwrap_or_else(|| format!("cf_{id}"))
}

fn slug_key(name: &str) -> String {
    let mut out = String::new();
    let mut underscore = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            underscore = false;
        } else if !underscore {
            out.push('_');
            underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn field_matches(field: &RawField, field_ref: &FieldRef) -> bool {
    match field_ref {
        FieldRef::Id(id) => field.id == *id,
        FieldRef::Label(label) => field.label == *label,
    }
}

fn dedupe_fields(fields: &[RawField]) -> Vec<RawField> {
    let mut by_id: BTreeMap<u64, RawField> = BTreeMap::new();
    for field in fields {
        match by_id.get_mut(&field.id) {
            Some(existing) => {
                if existing.label.is_empty() {
                    existing.label.clone_from(&field.label);
                }
                if existing.multiple.is_none() {
                    existing.multiple = field.multiple;
                }
                if existing.filterable.is_none() {
                    existing.filterable = field.filterable;
                }
            }
            None => {
                by_id.insert(field.id, field.clone());
            }
        }
    }
    by_id.into_values().collect()
}

fn bind(
    fields: &[Field],
    config: &BTreeMap<String, FieldRef>,
    statuses: &[Status],
    groups: &BTreeMap<String, Vec<String>>,
    source: FieldSource,
    unsampled: &[Named],
) -> Result<BTreeMap<String, Vec<u64>>, CatalogError> {
    let mut targets: BTreeMap<u64, &str> = BTreeMap::new();
    for (key, field_ref) in config {
        let id = field_target(fields, key, field_ref, source, unsampled)?;
        if let Some(other) = targets.insert(id, key.as_str()) {
            let label = fields
                .iter()
                .find(|field| field.id == id)
                .map(|field| field.label.as_str())
                .unwrap_or("");
            return Err(CatalogError::Bind(format!(
                "fields \"{other}\" and \"{key}\" both point at {label} ({id}). {}",
                source_line(source, unsampled)
            )));
        }
    }
    let mut bound = BTreeMap::new();
    for (group, members) in groups {
        let shadows: Vec<&str> = statuses
            .iter()
            .filter(|status| same_name(&status.name, group))
            .map(|status| status.name.as_str())
            .collect();
        if !shadows.is_empty() {
            return Err(CatalogError::Bind(format!(
                "status group \"{group}\" matches the status {}, so the group would hide it. Known statuses: {}.",
                shadows.join(", "),
                known_statuses(statuses)
            )));
        }
        let mut ids = Vec::new();
        for member in members {
            ids.push(status_id_by_name(statuses, member).map_err(|err| {
                CatalogError::Bind(format!(
                    "status group \"{group}\" lists \"{member}\". {err}"
                ))
            })?);
        }
        bound.insert(group.clone(), dedupe_ids(ids));
    }
    Ok(bound)
}

fn field_target(
    fields: &[Field],
    key: &str,
    field_ref: &FieldRef,
    source: FieldSource,
    unsampled: &[Named],
) -> Result<u64, CatalogError> {
    let hits: Vec<&Field> = fields
        .iter()
        .filter(|field| match field_ref {
            FieldRef::Id(id) => field.id == *id,
            FieldRef::Label(label) => field.label == *label,
        })
        .collect();
    let where_from = source_line(source, unsampled);
    match hits.as_slice() {
        [field] => Ok(field.id),
        [] => {
            let detail = match field_ref {
                FieldRef::Id(id) => {
                    format!("field \"{key}\" has id {id}, which is not in the catalog")
                }
                FieldRef::Label(label) => {
                    format!("field \"{key}\" has label \"{label}\", which matches no field")
                }
            };
            Err(CatalogError::Bind(format!(
                "{detail}. {where_from} Known fields: {}.",
                known_fields(fields)
            )))
        }
        many => {
            let listed = many
                .iter()
                .map(|field| format!("{} ({})", field.label, field.id))
                .collect::<Vec<_>>()
                .join(", ");
            let label = match field_ref {
                FieldRef::Label(label) => label.as_str(),
                FieldRef::Id(id) => {
                    return Err(CatalogError::Bind(format!(
                        "field \"{key}\" has id {id}, which matches more than one field: {listed}."
                    )));
                }
            };
            Err(CatalogError::Bind(format!(
                "field \"{key}\" has label \"{label}\", which matches more than one field: {listed}. {where_from}"
            )))
        }
    }
}

fn source_line(source: FieldSource, unsampled: &[Named]) -> String {
    match source {
        FieldSource::CustomFieldsEndpoint => "Fields came from /custom_fields.json.".to_string(),
        FieldSource::ProjectInclude => {
            "Fields came from /projects.json?include=issue_custom_fields.".to_string()
        }
        FieldSource::TrackerSample => {
            let mut line = "Fields came from one issue per tracker, because /custom_fields.json was refused and /projects.json did not include issue_custom_fields.".to_string();
            if !unsampled.is_empty() {
                let names = unsampled
                    .iter()
                    .map(|tracker| format!("{} ({})", tracker.name, tracker.id))
                    .collect::<Vec<_>>()
                    .join(", ");
                line.push_str(&format!(" Trackers with no visible issue: {names}."));
            }
            line
        }
    }
}

fn known_fields(fields: &[Field]) -> String {
    if fields.is_empty() {
        return "(none)".to_string();
    }
    fields
        .iter()
        .map(|field| format!("{} ({})", field.label, field.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a custom-field name did not resolve. `Unknown` is the only kind a
/// refresh might fix, because the field may have been added during the TTL.
enum FieldMiss {
    Unknown(String),
    Rejected(String),
}

fn known_statuses(statuses: &[Status]) -> String {
    if statuses.is_empty() {
        "(none)".to_string()
    } else {
        statuses
            .iter()
            .map(|status| status.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Snapshot {
    pub fn resolve_status(&self, input: &str) -> Result<StatusFilter, CatalogError> {
        let parts: Vec<&str> = input
            .split('|')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(CatalogError::Name("Status is empty.".to_string()));
        }
        if parts.iter().any(|part| same_name(part, "*")) {
            return Ok(StatusFilter::Any);
        }
        let mut saw_open = false;
        let mut saw_closed = false;
        let mut ids = Vec::new();
        for part in parts {
            if same_name(part, "open") {
                saw_open = true;
            } else if same_name(part, "closed") {
                saw_closed = true;
            } else {
                ids.extend(
                    resolve_status_part(part, &self.statuses, &self.groups)
                        .map_err(CatalogError::Name)?,
                );
            }
        }
        if saw_open && !saw_closed && ids.is_empty() {
            return Ok(StatusFilter::Open);
        }
        if saw_closed && !saw_open && ids.is_empty() {
            return Ok(StatusFilter::Closed);
        }
        // Open and closed together is every status, whatever else is listed.
        if saw_open && saw_closed {
            return Ok(StatusFilter::Any);
        }
        if saw_open {
            ids.extend(
                self.statuses
                    .iter()
                    .filter(|status| !status.is_closed)
                    .map(|status| status.id),
            );
        }
        if saw_closed {
            ids.extend(
                self.statuses
                    .iter()
                    .filter(|status| status.is_closed)
                    .map(|status| status.id),
            );
        }
        Ok(StatusFilter::Ids(dedupe_ids(ids)))
    }

    /// Assigned key, then `cf_<id>` for every field, then an exact label, then
    /// one partial key. `cf_<id>` works even when the field also has a slug.
    ///
    /// Keys come before labels, so with `severity: 36` configured, "Severity"
    /// finds field 36 even when another field is labelled "Severity". A caller
    /// should echo which field it used, such as `severity -> Severity Rating (36)`,
    /// so a surprising pick is visible.
    pub fn resolve_custom_field(&self, input: &str) -> Result<&Field, CatalogError> {
        self.find_custom_field(input).map_err(|miss| match miss {
            FieldMiss::Unknown(message) | FieldMiss::Rejected(message) => {
                CatalogError::Name(message)
            }
        })
    }

    fn find_custom_field(&self, input: &str) -> Result<&Field, FieldMiss> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(FieldMiss::Rejected("Custom field is empty.".to_string()));
        }
        let slug = slug_key(raw);
        if let Some(field) = self
            .fields
            .iter()
            .find(|field| field.key == slug || field.key.eq_ignore_ascii_case(raw))
        {
            return Ok(field);
        }
        if let Some(id) = cf_id(raw)
            && let Some(field) = self.fields.iter().find(|field| field.id == id)
        {
            return Ok(field);
        }
        let label_hits: Vec<&Field> = self
            .fields
            .iter()
            .filter(|field| same_name(&field.label, raw))
            .collect();
        match label_hits.as_slice() {
            [field] => return Ok(field),
            [_, ..] => {
                return Err(FieldMiss::Rejected(format!(
                    "Ambiguous custom field \"{raw}\". Matches: {}.",
                    label_hits
                        .iter()
                        .map(|field| field.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            [] => {}
        }
        if !slug.is_empty() {
            let partial: Vec<&Field> = self
                .fields
                .iter()
                .filter(|field| field.key.contains(&slug))
                .collect();
            match partial.as_slice() {
                [field] => return Ok(field),
                [_, ..] => {
                    return Err(FieldMiss::Rejected(format!(
                        "Ambiguous custom field \"{raw}\". Matches: {}.",
                        partial
                            .iter()
                            .map(|field| field.key.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                [] => {}
            }
        }
        Err(FieldMiss::Unknown(format!(
            "Unknown custom field \"{raw}\". Known fields: {}.",
            if self.fields.is_empty() {
                "(none)".to_string()
            } else {
                self.fields
                    .iter()
                    .map(|field| field.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )))
    }

    pub fn resolve_tracker(&self, input: &str) -> Result<Vec<u64>, CatalogError> {
        resolve_parts(
            input,
            &self.trackers,
            "tracker",
            |item| &item.name,
            |item| item.id,
        )
    }

    pub fn resolve_priority(&self, input: &str) -> Result<Vec<u64>, CatalogError> {
        resolve_parts(
            input,
            &self.priorities,
            "priority",
            |item| &item.name,
            |item| item.id,
        )
    }
}

fn resolve_status_part(
    part: &str,
    statuses: &[Status],
    groups: &BTreeMap<String, Vec<u64>>,
) -> Result<Vec<u64>, String> {
    if let Some(id) = parse_id(part) {
        if statuses.iter().any(|status| status.id == id) {
            return Ok(vec![id]);
        }
        return Err(format!(
            "Unknown status \"{part}\". Known statuses: {}.",
            known_statuses(statuses)
        ));
    }
    let lower = part.to_lowercase();
    let exact: Vec<&Status> = statuses
        .iter()
        .filter(|status| status.name.to_lowercase() == lower)
        .collect();
    if !exact.is_empty() {
        return exact_ids(
            part,
            exact.as_slice(),
            |status| status.name.as_str(),
            |status| status.id,
        );
    }
    let group_hits: Vec<&Vec<u64>> = groups
        .iter()
        .filter(|(name, _)| same_name(name, part))
        .map(|(_, ids)| ids)
        .collect();
    match group_hits.as_slice() {
        [ids] => return Ok((*ids).clone()),
        [_, ..] => {
            return Err(format!(
                "Ambiguous status group \"{part}\". It matches more than one configured group."
            ));
        }
        [] => {}
    }
    let prefixed = prefix_ids(statuses, part);
    if !prefixed.is_empty() {
        return Ok(prefixed);
    }
    let partial = substring_ids(statuses, part);
    match partial.len() {
        1 => Ok(partial),
        0 => Err(format!(
            "Unknown status \"{part}\". Known statuses: {}.",
            known_statuses(statuses)
        )),
        _ => Err(format!(
            "Ambiguous status \"{part}\". Matches: {}.",
            statuses
                .iter()
                .filter(|status| partial.contains(&status.id))
                .map(|status| status.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn exact_ids<T>(
    part: &str,
    hits: &[&T],
    name_of: impl Fn(&T) -> &str,
    id_of: impl Fn(&T) -> u64,
) -> Result<Vec<u64>, String> {
    let precise: Vec<u64> = hits
        .iter()
        .copied()
        .filter(|item| name_of(item) == part)
        .map(&id_of)
        .collect();
    if precise.len() == 1 {
        return Ok(precise);
    }
    if hits.len() == 1 {
        return Ok(vec![id_of(hits[0])]);
    }
    // Same spelling twice is two real rows. A case-only clash with no exact hit is a typo.
    if !precise.is_empty() && precise.len() == hits.len() {
        return Ok(precise);
    }
    Err(format!(
        "Ambiguous status \"{part}\". Matches: {}.",
        hits.iter()
            .map(|item| name_of(item))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn prefix_ids(statuses: &[Status], input: &str) -> Vec<u64> {
    // "QA: Testing" is a name, not a request for every QA: status.
    if input.contains(':') {
        return Vec::new();
    }
    let prefix = format!("{}:", input.to_lowercase());
    statuses
        .iter()
        .filter(|status| status.name.to_lowercase().starts_with(&prefix))
        .map(|status| status.id)
        .collect()
}

fn substring_ids(statuses: &[Status], input: &str) -> Vec<u64> {
    let needle = input.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    statuses
        .iter()
        .filter(|status| status.name.to_lowercase().contains(&needle))
        .map(|status| status.id)
        .collect()
}

/// A status named in the config file. Exact names only, ignoring case: the file
/// promises exact names, so a typo fails the bind instead of landing on
/// whatever status happens to contain it.
fn status_id_by_name(statuses: &[Status], name: &str) -> Result<u64, String> {
    let hits: Vec<&Status> = statuses
        .iter()
        .filter(|status| same_name(&status.name, name))
        .collect();
    if hits.is_empty() {
        return Err(format!(
            "No status has that exact name. Known statuses: {}.",
            known_statuses(statuses)
        ));
    }
    let ids = exact_ids(
        name,
        &hits,
        |status| status.name.as_str(),
        |status| status.id,
    )?;
    match ids.as_slice() {
        [id] => Ok(*id),
        _ => Err(format!(
            "More than one status has that name: {}.",
            hits.iter()
                .map(|status| status.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn resolve_parts<T>(
    input: &str,
    items: &[T],
    what: &str,
    name_of: impl Fn(&T) -> &str,
    id_of: impl Fn(&T) -> u64,
) -> Result<Vec<u64>, CatalogError> {
    let parts: Vec<&str> = input
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(CatalogError::Name(format!("{what} is empty.")));
    }
    let mut ids = Vec::new();
    for part in parts {
        ids.extend(resolve_one(part, items, what, &name_of, &id_of).map_err(CatalogError::Name)?);
    }
    Ok(dedupe_ids(ids))
}

fn resolve_one<T>(
    part: &str,
    items: &[T],
    what: &str,
    name_of: &impl Fn(&T) -> &str,
    id_of: &impl Fn(&T) -> u64,
) -> Result<Vec<u64>, String> {
    if let Some(id) = parse_id(part) {
        if items.iter().any(|item| id_of(item) == id) {
            return Ok(vec![id]);
        }
        return Err(unknown_name(what, part, items, name_of));
    }
    let lower = part.to_lowercase();
    let exact: Vec<&T> = items
        .iter()
        .filter(|item| name_of(item).to_lowercase() == lower)
        .collect();
    if !exact.is_empty() {
        return exact_ids(part, exact.as_slice(), name_of, id_of)
            .map_err(|err| err.replacen("Ambiguous status", &format!("Ambiguous {what}"), 1));
    }
    let partial: Vec<&T> = items
        .iter()
        .filter(|item| name_of(item).to_lowercase().contains(&lower))
        .collect();
    match partial.as_slice() {
        [item] => Ok(vec![id_of(item)]),
        [] => Err(unknown_name(what, part, items, name_of)),
        many => Err(ambiguous(
            what,
            part,
            &many.iter().map(|item| name_of(item)).collect::<Vec<_>>(),
        )),
    }
}

fn unknown_name<T>(what: &str, part: &str, items: &[T], name_of: impl Fn(&T) -> &str) -> String {
    let known = if items.is_empty() {
        "(none)".to_string()
    } else {
        items.iter().map(name_of).collect::<Vec<_>>().join(", ")
    };
    format!("Unknown {what} \"{part}\". Known: {known}.")
}

fn ambiguous(what: &str, part: &str, names: &[&str]) -> String {
    format!(
        "Ambiguous {what} \"{part}\". Matches: {}.",
        names.join(", ")
    )
}

fn cf_id(input: &str) -> Option<u64> {
    let lower = input.to_ascii_lowercase();
    let rest = lower.strip_prefix("cf_")?;
    parse_id(rest)
}

fn parse_id(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn same_name(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn dedupe_ids(ids: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let mut out = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// One line of at most 200 characters, for a note inside an issue response.
fn short_reason(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::render::clip(&format!("catalog unavailable: {line}"), 200)
}

fn is_absent(err: &CatalogError) -> bool {
    matches!(err, CatalogError::Redmine(inner) if matches!(inner.status(), Some(401 | 403 | 404)))
}

async fn load_statuses(client: &Client) -> Result<Vec<Status>, CatalogError> {
    let body: StatusBody = client.get_json("/issue_statuses.json", &[]).await?;
    Ok(body
        .issue_statuses
        .into_iter()
        .map(|row| Status {
            id: row.id,
            name: row.name,
            is_closed: row.is_closed,
        })
        .collect())
}

async fn load_trackers(client: &Client) -> Result<Vec<Named>, CatalogError> {
    let body: NamedBody<TrackerRow> = client.get_json("/trackers.json", &[]).await?;
    Ok(body
        .trackers
        .into_iter()
        .map(|row| Named {
            id: row.id,
            name: row.name,
        })
        .collect())
}

async fn load_priorities(client: &Client) -> Result<Vec<Named>, CatalogError> {
    let body: PriorityBody = client
        .get_json("/enumerations/issue_priorities.json", &[])
        .await?;
    Ok(body
        .issue_priorities
        .into_iter()
        .map(|row| Named {
            id: row.id,
            name: row.name,
        })
        .collect())
}

async fn load_queries(client: &Client) -> Result<Vec<Query>, CatalogError> {
    match pages::<QueryRow>(client, "/queries.json", "queries", &[]).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|row| Query {
                id: row.id,
                name: row.name,
                project_id: row.project_id,
            })
            .collect()),
        Err(err) if is_absent(&err) => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

async fn load_fields(client: &Client, trackers: &[Named]) -> Result<FieldList, CatalogError> {
    match custom_fields_from_endpoint(client).await {
        Ok(raw) => {
            return Ok(FieldList {
                raw,
                source: FieldSource::CustomFieldsEndpoint,
                unsampled: Vec::new(),
            });
        }
        Err(err) if is_absent(&err) => {}
        Err(err) => return Err(err),
    }
    match project_fields(client).await {
        Ok(Some(raw)) => {
            return Ok(FieldList {
                raw,
                source: FieldSource::ProjectInclude,
                unsampled: Vec::new(),
            });
        }
        Ok(None) => {}
        Err(err) => return Err(err),
    }
    let mut merged: BTreeMap<u64, RawField> = BTreeMap::new();
    let mut unsampled = Vec::new();
    for tracker in trackers {
        let tracker_id = tracker.id.to_string();
        let body: IssuesBody = client
            .get_json(
                "/issues.json",
                &[
                    ("tracker_id", tracker_id.as_str()),
                    ("status_id", "*"),
                    ("limit", "1"),
                ],
            )
            .await?;
        let Some(issue) = body.issues.into_iter().next() else {
            unsampled.push(tracker.clone());
            continue;
        };
        for field in issue.custom_fields {
            merge_raw(
                &mut merged,
                RawField {
                    id: field.id,
                    label: field.name,
                    multiple: field.multiple,
                    filterable: None,
                },
            );
        }
    }
    Ok(FieldList {
        raw: merged.into_values().collect(),
        source: FieldSource::TrackerSample,
        unsampled,
    })
}

async fn custom_fields_from_endpoint(client: &Client) -> Result<Vec<RawField>, CatalogError> {
    let body: CustomFieldsBody = client.get_json("/custom_fields.json", &[]).await?;
    Ok(body
        .custom_fields
        .into_iter()
        .filter(|row| row.customized_type.as_deref() == Some("issue"))
        .map(|row| RawField {
            id: row.id,
            label: row.name,
            multiple: row.multiple,
            filterable: match (row.is_filter, row.visible) {
                (Some(is_filter), Some(visible)) => Some(is_filter && visible),
                (Some(false), _) | (_, Some(false)) => Some(false),
                _ => None,
            },
        })
        .collect())
}

/// `Ok(None)` means this Redmine ignored `include=issue_custom_fields` (older than 4.2).
async fn project_fields(client: &Client) -> Result<Option<Vec<RawField>>, CatalogError> {
    let projects = pages::<ProjectRow>(
        client,
        "/projects.json",
        "projects",
        &[("include", "issue_custom_fields")],
    )
    .await?;
    if !projects
        .iter()
        .any(|project| project.issue_custom_fields.is_some())
    {
        return Ok(None);
    }
    let mut merged = BTreeMap::new();
    for project in projects {
        if let Some(fields) = project.issue_custom_fields {
            for field in fields {
                merge_raw(
                    &mut merged,
                    RawField {
                        id: field.id,
                        label: field.name,
                        multiple: None,
                        filterable: None,
                    },
                );
            }
        }
    }
    Ok(Some(merged.into_values().collect()))
}

fn merge_raw(into: &mut BTreeMap<u64, RawField>, field: RawField) {
    match into.get_mut(&field.id) {
        Some(existing) => {
            if existing.label.is_empty() {
                existing.label = field.label;
            }
            if existing.multiple.is_none() {
                existing.multiple = field.multiple;
            }
            if existing.filterable.is_none() {
                existing.filterable = field.filterable;
            }
        }
        None => {
            into.insert(field.id, field);
        }
    }
}

async fn fetch_categories(client: &Client, project_id: &str) -> Result<Vec<Named>, CatalogError> {
    let path = format!("/projects/{project_id}/issue_categories.json");
    let body: CategoriesBody = client.get_json(&path, &[]).await?;
    Ok(body
        .issue_categories
        .into_iter()
        .map(|row| Named {
            id: row.id,
            name: row.name,
        })
        .collect())
}

async fn fetch_members(client: &Client, project_id: &str) -> Result<Vec<Member>, CatalogError> {
    let path = format!("/projects/{project_id}/memberships.json");
    let rows = pages::<MembershipRow>(client, &path, "memberships", &[]).await?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(user) = row.user {
            out.push(Member {
                id: user.id,
                name: user.name,
                kind: MemberKind::User,
            });
        }
        if let Some(group) = row.group {
            out.push(Member {
                id: group.id,
                name: group.name,
                kind: MemberKind::Group,
            });
        }
    }
    Ok(out)
}

async fn pages<T: DeserializeOwned>(
    client: &Client,
    path: &str,
    list_key: &str,
    extra: &[(&str, &str)],
) -> Result<Vec<T>, CatalogError> {
    let mut offset = 0u64;
    let mut out = Vec::new();
    loop {
        let offset_s = offset.to_string();
        let limit_s = REDMINE_PAGE_MAX.to_string();
        let mut query: Vec<(&str, &str)> = extra.to_vec();
        query.push(("offset", &offset_s));
        query.push(("limit", &limit_s));
        let value: serde_json::Value = client.get_json(path, &query).await?;
        let total = value.get("total_count").and_then(serde_json::Value::as_u64);
        if let Some(total) = total
            && total > LIST_CAP
        {
            return Err(CatalogError::message(format!(
                "{path} lists {total} rows, over the catalog cap of {LIST_CAP}"
            )));
        }
        let Some(items) = value.get(list_key).and_then(serde_json::Value::as_array) else {
            return Err(CatalogError::message(format!(
                "Unexpected response from {path}: missing {list_key}"
            )));
        };
        let count = items.len() as u64;
        for item in items {
            out.push(serde_json::from_value(item.clone()).map_err(|err| {
                CatalogError::message(format!("Unexpected response from {path}: {err}"))
            })?);
        }
        if count == 0
            || count < REDMINE_PAGE_MAX
            || total.is_some_and(|total| offset + count >= total)
        {
            break;
        }
        offset += count;
        if offset >= LIST_CAP {
            return Err(CatalogError::message(format!(
                "{path} lists more than {LIST_CAP} rows, over the catalog cap"
            )));
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct StatusBody {
    issue_statuses: Vec<StatusRow>,
}

#[derive(Deserialize)]
struct StatusRow {
    id: u64,
    name: String,
    #[serde(default)]
    is_closed: bool,
}

#[derive(Deserialize)]
struct NamedBody<T> {
    trackers: Vec<T>,
}

#[derive(Deserialize)]
struct TrackerRow {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct PriorityBody {
    issue_priorities: Vec<TrackerRow>,
}

#[derive(Deserialize)]
struct QueryRow {
    id: u64,
    name: String,
    #[serde(default)]
    project_id: Option<u64>,
}

#[derive(Deserialize)]
struct CustomFieldsBody {
    custom_fields: Vec<CustomFieldRow>,
}

#[derive(Deserialize)]
struct CustomFieldRow {
    id: u64,
    name: String,
    #[serde(default)]
    customized_type: Option<String>,
    #[serde(default)]
    multiple: Option<bool>,
    #[serde(default)]
    is_filter: Option<bool>,
    #[serde(default)]
    visible: Option<bool>,
}

#[derive(Deserialize)]
struct ProjectRow {
    #[serde(default)]
    issue_custom_fields: Option<Vec<IdName>>,
}

#[derive(Deserialize)]
struct IdName {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct IssuesBody {
    #[serde(default)]
    issues: Vec<IssueSample>,
}

#[derive(Deserialize)]
struct IssueSample {
    #[serde(default)]
    custom_fields: Vec<SampleField>,
}

#[derive(Deserialize)]
struct SampleField {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    multiple: Option<bool>,
}

#[derive(Deserialize)]
struct CategoriesBody {
    #[serde(default)]
    issue_categories: Vec<IdName>,
}

#[derive(Deserialize)]
struct MembershipRow {
    #[serde(default)]
    user: Option<IdName>,
    #[serde(default)]
    group: Option<IdName>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::load_from;

    fn raw(id: u64, label: &str) -> RawField {
        RawField {
            id,
            label: label.to_string(),
            multiple: None,
            filterable: None,
        }
    }

    fn config(entries: &[(&str, FieldRef)]) -> BTreeMap<String, FieldRef> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn key_of(fields: &[Field], id: u64) -> &str {
        fields
            .iter()
            .find(|field| field.id == id)
            .map(|field| field.key.as_str())
            .unwrap_or("")
    }

    fn garden() -> Vec<RawField> {
        vec![
            raw(40, "Severity"),
            raw(36, "Severity Rating"),
            raw(41, "Status"),
            raw(42, "Région"),
            raw(43, "Region"),
            raw(7, "Приоритет"),
            raw(8, "顧客"),
        ]
    }

    #[test]
    fn configured_keys_win_and_collisions_become_cf_ids() {
        let fields = assign_keys(&garden(), &config(&[("severity", FieldRef::Id(36))]));
        assert_eq!(key_of(&fields, 36), "severity");
        assert_eq!(key_of(&fields, 40), "cf_40");
        assert_eq!(key_of(&fields, 41), "cf_41");
        assert_eq!(key_of(&fields, 42), "r_gion");
        assert_eq!(key_of(&fields, 43), "region");
    }

    #[test]
    fn slugs_lowercase_labels_and_turn_other_characters_into_underscores() {
        assert_eq!(slug_key("Company (Primary)"), "company_primary");
        assert_eq!(slug_key("Rollout/plan notes"), "rollout_plan_notes");
    }

    #[test]
    fn a_label_with_no_ascii_letters_keeps_a_cf_id() {
        let fields = assign_keys(&garden(), &BTreeMap::new());
        assert_eq!(key_of(&fields, 7), "cf_7");
        assert_eq!(key_of(&fields, 8), "cf_8");
    }

    #[test]
    fn the_lower_id_keeps_the_slug_whatever_order_the_fields_arrive() {
        let mut fields = vec![raw(43, "Region"), raw(10, "Region")];
        let assigned = assign_keys(&fields, &BTreeMap::new());
        assert_eq!(key_of(&assigned, 10), "region");
        assert_eq!(key_of(&assigned, 43), "cf_43");
        fields.reverse();
        let assigned = assign_keys(&fields, &BTreeMap::new());
        assert_eq!(key_of(&assigned, 10), "region");
        assert_eq!(key_of(&assigned, 43), "cf_43");
    }

    #[test]
    fn a_user_custom_field_does_not_take_a_key() {
        let body: CustomFieldsBody = serde_json::from_value(json!({
            "custom_fields": [
                { "id": 8, "name": "Department", "customized_type": "user" },
                { "id": 2, "name": "Product", "customized_type": "issue", "multiple": false }
            ]
        }))
        .unwrap();
        let raw_fields: Vec<RawField> = body
            .custom_fields
            .into_iter()
            .filter(|row| row.customized_type.as_deref() == Some("issue"))
            .map(|row| RawField {
                id: row.id,
                label: row.name,
                multiple: row.multiple,
                filterable: None,
            })
            .collect();
        let assigned = assign_keys(&raw_fields, &BTreeMap::new());
        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].id, 2);
        assert_eq!(assigned[0].key, "product");
        assert_eq!(assigned[0].multiple, Some(false));
    }

    #[test]
    fn every_field_resolves_by_cf_id_including_one_with_a_slug() {
        let snapshot = sample_snapshot(assign_keys(
            &[raw(2, "Product"), raw(40, "Severity")],
            &config(&[("severity", FieldRef::Id(40))]),
        ));
        let by_slug = snapshot.resolve_custom_field("product").unwrap();
        assert_eq!(by_slug.id, 2);
        let by_cf = snapshot.resolve_custom_field("cf_2").unwrap();
        assert_eq!(by_cf.id, 2);
        assert_eq!(by_cf.key, "product");
        assert_eq!(
            snapshot.resolve_custom_field("CF_40").unwrap().key,
            "severity"
        );
        assert_eq!(snapshot.resolve_custom_field("Product").unwrap().id, 2);
    }

    #[test]
    fn a_configured_key_beats_another_fields_label() {
        let snapshot = sample_snapshot(assign_keys(
            &[raw(36, "Severity Rating"), raw(40, "Severity")],
            &config(&[("severity", FieldRef::Id(36))]),
        ));
        let field = snapshot.resolve_custom_field("Severity").unwrap();
        assert_eq!(field.id, 36);
        assert_eq!(field.key, "severity");
        assert_eq!(key_of(&snapshot.fields, 40), "cf_40");
        assert_eq!(snapshot.resolve_custom_field("cf_40").unwrap().id, 40);
    }

    fn sample_statuses() -> Vec<Status> {
        vec![
            status(1, "New", false),
            status(2, "In Progress", false),
            status(3, "Review", false),
            status(4, "QA: Testing", false),
            status(5, "QA: Failed", false),
            status(6, "Rejected", true),
            status(7, "Closed", true),
            status(8, "Waiting on vendor", false),
            status(9, "Waiting on customer", false),
        ]
    }

    fn status(id: u64, name: &str, is_closed: bool) -> Status {
        Status {
            id,
            name: name.to_string(),
            is_closed,
        }
    }

    fn sample_snapshot(fields: Vec<Field>) -> Snapshot {
        let mut groups = BTreeMap::new();
        groups.insert("qa".to_string(), vec![4]);
        groups.insert("blocked".to_string(), vec![6]);
        Snapshot {
            statuses: sample_statuses(),
            trackers: vec![
                Named {
                    id: 1,
                    name: "Bug".to_string(),
                },
                Named {
                    id: 2,
                    name: "Feature".to_string(),
                },
            ],
            priorities: vec![Named {
                id: 3,
                name: "Normal".to_string(),
            }],
            queries: Vec::new(),
            fields,
            field_source: FieldSource::ProjectInclude,
            unsampled_trackers: Vec::new(),
            groups,
            loaded_at: Instant::now(),
        }
    }

    #[test]
    fn status_prefix_exact_group_and_pipes() {
        let snapshot = sample_snapshot(Vec::new());
        assert_eq!(
            snapshot.resolve_status("qa").unwrap(),
            StatusFilter::Ids(vec![4])
        );
        assert_eq!(
            snapshot.resolve_status("QA: Testing").unwrap(),
            StatusFilter::Ids(vec![4])
        );
        assert_eq!(
            snapshot.resolve_status("blocked").unwrap(),
            StatusFilter::Ids(vec![6])
        );
        assert_eq!(
            snapshot.resolve_status("QA: Testing|QA: Failed").unwrap(),
            StatusFilter::Ids(vec![4, 5])
        );
        assert_eq!(
            snapshot.resolve_status("4|QA: Testing").unwrap(),
            StatusFilter::Ids(vec![4])
        );
        assert_eq!(snapshot.resolve_status("open").unwrap(), StatusFilter::Open);
        assert_eq!(
            snapshot.resolve_status("CLOSED").unwrap(),
            StatusFilter::Closed
        );
        assert_eq!(snapshot.resolve_status("*|New").unwrap(), StatusFilter::Any);
        assert_eq!(
            snapshot.resolve_status("open|closed").unwrap(),
            StatusFilter::Any
        );
        assert_eq!(
            snapshot.resolve_status("Closed|open|Review").unwrap(),
            StatusFilter::Any
        );
        assert_eq!(
            snapshot.resolve_status("open|Review").unwrap(),
            StatusFilter::Ids(vec![3, 1, 2, 4, 5, 8, 9])
        );
        assert_eq!(
            snapshot.resolve_status("closed|Review").unwrap(),
            StatusFilter::Ids(vec![3, 6, 7])
        );
        assert_eq!(
            snapshot.resolve_status("in progress").unwrap(),
            StatusFilter::Ids(vec![2])
        );
    }

    #[test]
    fn the_prefix_rule_selects_every_status_with_that_prefix() {
        let mut snapshot = sample_snapshot(Vec::new());
        // Without the group, "qa" would be every QA: status.
        snapshot.groups.remove("qa");
        assert_eq!(
            snapshot.resolve_status("qa").unwrap(),
            StatusFilter::Ids(vec![4, 5])
        );
    }

    #[test]
    fn two_substring_matches_list_both_and_a_miss_lists_the_real_statuses() {
        let snapshot = sample_snapshot(Vec::new());
        let ambiguous = snapshot.resolve_status("waiting").unwrap_err().to_string();
        assert!(ambiguous.contains("Waiting on vendor"), "{ambiguous}");
        assert!(ambiguous.contains("Waiting on customer"), "{ambiguous}");
        let unknown = snapshot.resolve_status("Nope").unwrap_err().to_string();
        assert!(unknown.contains("Known statuses"), "{unknown}");
        assert!(unknown.contains("QA: Testing"), "{unknown}");
        let missing_id = snapshot.resolve_status("99999").unwrap_err().to_string();
        assert!(missing_id.contains("99999"), "{missing_id}");
        assert!(missing_id.contains("Known statuses"), "{missing_id}");
    }

    #[test]
    fn when_two_statuses_differ_only_by_case_the_exact_one_wins() {
        let mut snapshot = sample_snapshot(Vec::new());
        snapshot.statuses.push(status(10, "qa: testing", false));
        assert_eq!(
            snapshot.resolve_status("QA: Testing").unwrap(),
            StatusFilter::Ids(vec![4])
        );
        let err = snapshot
            .resolve_status("QA: TESTING")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ambiguous"), "{err}");
    }

    fn try_bind(
        fields: &[Field],
        configured: BTreeMap<String, FieldRef>,
        groups: BTreeMap<String, Vec<String>>,
        source: FieldSource,
        unsampled: &[Named],
    ) -> Result<BTreeMap<String, Vec<u64>>, CatalogError> {
        bind(
            fields,
            &configured,
            &sample_statuses(),
            &groups,
            source,
            unsampled,
        )
    }

    #[test]
    fn bind_rejects_a_missing_id_a_missing_label_and_an_ambiguous_label() {
        let fields = assign_keys(
            &[raw(2, "Product"), raw(3, "Region"), raw(4, "Region")],
            &BTreeMap::new(),
        );
        let missing_id = try_bind(
            &fields,
            config(&[("widget", FieldRef::Id(99))]),
            BTreeMap::new(),
            FieldSource::ProjectInclude,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(missing_id.contains("99"), "{missing_id}");
        assert!(missing_id.contains("Product"), "{missing_id}");
        assert!(missing_id.contains("issue_custom_fields"), "{missing_id}");

        let missing_label = try_bind(
            &fields,
            config(&[("customer", FieldRef::Label("Customer".to_string()))]),
            BTreeMap::new(),
            FieldSource::CustomFieldsEndpoint,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(missing_label.contains("Customer"), "{missing_label}");
        assert!(
            missing_label.contains("/custom_fields.json"),
            "{missing_label}"
        );

        let ambiguous = try_bind(
            &fields,
            config(&[("region", FieldRef::Label("Region".to_string()))]),
            BTreeMap::new(),
            FieldSource::ProjectInclude,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(ambiguous.contains("more than one"), "{ambiguous}");
        assert!(ambiguous.contains("3"), "{ambiguous}");
        assert!(ambiguous.contains("4"), "{ambiguous}");
    }

    #[test]
    fn bind_rejects_two_keys_on_one_field_a_shadowing_group_and_an_unknown_member() {
        let fields = assign_keys(&[raw(2, "Product")], &BTreeMap::new());
        let two = try_bind(
            &fields,
            config(&[
                ("product", FieldRef::Id(2)),
                ("widget", FieldRef::Label("Product".to_string())),
            ]),
            BTreeMap::new(),
            FieldSource::CustomFieldsEndpoint,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(two.contains("product"), "{two}");
        assert!(two.contains("widget"), "{two}");

        let mut groups = BTreeMap::new();
        groups.insert("review".to_string(), vec!["New".to_string()]);
        let shadow = try_bind(
            &fields,
            BTreeMap::new(),
            groups,
            FieldSource::ProjectInclude,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(shadow.contains("hide"), "{shadow}");

        let mut groups = BTreeMap::new();
        groups.insert("in-review".to_string(), vec!["Nope".to_string()]);
        let missing = try_bind(
            &fields,
            BTreeMap::new(),
            groups,
            FieldSource::TrackerSample,
            &[Named {
                id: 2,
                name: "Feature".to_string(),
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("Nope"), "{missing}");
        assert!(missing.contains("Known statuses"), "{missing}");
    }

    #[test]
    fn a_group_member_must_be_an_exact_status_name() {
        let fields = assign_keys(&[raw(2, "Product")], &BTreeMap::new());
        // "Test" is inside "QA: Testing". A filter may match it; the config may not.
        let mut groups = BTreeMap::new();
        groups.insert("testing".to_string(), vec!["Test".to_string()]);
        let err = try_bind(
            &fields,
            BTreeMap::new(),
            groups,
            FieldSource::ProjectInclude,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"Test\""), "{err}");
        assert!(err.contains("exact name"), "{err}");

        let mut groups = BTreeMap::new();
        groups.insert("testing".to_string(), vec!["qa: testing".to_string()]);
        let bound = try_bind(
            &fields,
            BTreeMap::new(),
            groups,
            FieldSource::ProjectInclude,
            &[],
        )
        .unwrap();
        assert_eq!(bound["testing"], vec![4]);
    }

    #[test]
    fn a_tracker_partial_has_to_be_unique_and_a_pipe_dedupes() {
        let snapshot = sample_snapshot(Vec::new());
        assert_eq!(snapshot.resolve_tracker("bug").unwrap(), vec![1]);
        assert_eq!(snapshot.resolve_tracker("Bug|1").unwrap(), vec![1]);
        let err = snapshot.resolve_tracker("Nope").unwrap_err().to_string();
        assert!(err.contains("Feature"), "{err}");
    }

    fn catalog_for(base: &str, file: Option<&str>, ttl_ms: Option<&str>) -> Catalog {
        let mut pairs = vec![
            ("REDMINE_URL".to_string(), base.to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
        ];
        if file.is_some() {
            pairs.push((
                "REDMINE_CONFIG".to_string(),
                "/etc/redmine.json".to_string(),
            ));
        }
        if let Some(ttl) = ttl_ms {
            pairs.push(("REDMINE_CATALOG_TTL_MS".to_string(), ttl.to_string()));
        }
        let env: HashMap<String, String> = pairs.into_iter().collect();
        let file = file.unwrap_or("").to_string();
        let config = load_from(&env, move |_| Ok(file.clone())).unwrap();
        Catalog::new(Client::new(config).unwrap())
    }

    fn vocab() -> serde_json::Value {
        json!({
            "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }],
            "trackers": [
                { "id": 1, "name": "Bug" },
                { "id": 2, "name": "Feature" }
            ],
            "issue_priorities": [{ "id": 3, "name": "Normal" }]
        })
    }

    async fn mount_json(server: &MockServer, path_name: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(path_name))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_vocab(server: &MockServer) {
        let body = vocab();
        mount_json(
            server,
            "/issue_statuses.json",
            json!({ "issue_statuses": body["issue_statuses"] }),
        )
        .await;
        mount_json(
            server,
            "/trackers.json",
            json!({ "trackers": body["trackers"] }),
        )
        .await;
        mount_json(
            server,
            "/enumerations/issue_priorities.json",
            json!({ "issue_priorities": body["issue_priorities"] }),
        )
        .await;
    }

    #[tokio::test]
    async fn a_refused_custom_fields_endpoint_uses_the_project_include_once() {
        let server = MockServer::start().await;
        mount_vocab(&server).await;
        Mock::given(method("GET"))
            .and(path("/queries.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projects": [
                    {
                        "id": 1,
                        "name": "Garden",
                        "issue_custom_fields": [
                            { "id": 2, "name": "Product" },
                            { "id": 36, "name": "Severity Rating" }
                        ]
                    },
                    {
                        "id": 2,
                        "name": "Shed",
                        "issue_custom_fields": [
                            { "id": 2, "name": "Product" },
                            { "id": 9, "name": "Project reference" }
                        ]
                    }
                ],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;

        let catalog = catalog_for(&server.uri(), None, None);
        let snapshot = catalog.get().await.unwrap();
        assert_eq!(snapshot.field_source, FieldSource::ProjectInclude);
        assert!(snapshot.queries.is_empty());
        assert_eq!(key_of(&snapshot.fields, 2), "product");
        assert_eq!(key_of(&snapshot.fields, 36), "severity_rating");
        assert_eq!(key_of(&snapshot.fields, 9), "project_reference");
        assert!(snapshot.fields.iter().all(|field| field.multiple.is_none()));
        let again = catalog.get().await.unwrap();
        assert_eq!(again.fields.len(), snapshot.fields.len());
    }

    #[tokio::test]
    async fn without_the_project_include_it_samples_one_issue_per_tracker() {
        let server = MockServer::start().await;
        mount_vocab(&server).await;
        Mock::given(method("GET"))
            .and(path("/queries.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [],
                "total_count": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projects": [{ "id": 1, "name": "Garden" }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("tracker_id", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{
                    "id": 10,
                    "custom_fields": [
                        { "id": 2, "name": "Product", "value": "Hosepipe" }
                    ]
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("tracker_id", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(1)
            .mount(&server)
            .await;

        let catalog = catalog_for(&server.uri(), Some(r#"{"fields":{"widget":99}}"#), None);
        let err = catalog.get().await.unwrap_err().to_string();
        assert!(err.contains("widget"), "{err}");
        assert!(err.contains("99"), "{err}");
        assert!(err.contains("Feature"), "{err}");
        assert!(err.contains("no visible issue"), "{err}");
        let again = catalog.get().await.unwrap_err().to_string();
        assert_eq!(again, err);
    }

    #[tokio::test]
    async fn a_status_failure_is_not_cached() {
        let server = MockServer::start().await;
        // The first mounted mock wins, so the 500 is used once and the retry gets the 200.
        Mock::given(method("GET"))
            .and(path("/issue_statuses.json"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issue_statuses.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        // The failed load and the retry both request these. Statuses fail before fields.
        Mock::given(method("GET"))
            .and(path("/trackers.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "trackers": [{ "id": 1, "name": "Bug" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/enumerations/issue_priorities.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue_priorities": [{ "id": 3, "name": "Normal" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/queries.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [],
                "total_count": 0
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "custom_fields": []
            })))
            .mount(&server)
            .await;

        let catalog = catalog_for(&server.uri(), None, None);
        assert!(catalog.get().await.is_err());
        let snapshot = catalog.get().await.unwrap();
        assert_eq!(snapshot.statuses[0].name, "New");
    }

    #[tokio::test]
    async fn two_callers_share_one_load() {
        let server = MockServer::start().await;
        let delayed = |body: serde_json::Value| {
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_json(body)
        };
        for (path_name, body) in [
            (
                "/issue_statuses.json",
                json!({ "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }] }),
            ),
            (
                "/trackers.json",
                json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
            ),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [{ "id": 3, "name": "Normal" }] }),
            ),
            ("/queries.json", json!({ "queries": [], "total_count": 0 })),
            (
                "/custom_fields.json",
                json!({ "custom_fields": [{ "id": 2, "name": "Product", "customized_type": "issue" }] }),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(path_name))
                .respond_with(delayed(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        let catalog = catalog_for(&server.uri(), None, None);
        let (first, second) = tokio::join!(catalog.get(), catalog.get());
        assert_eq!(first.unwrap().fields[0].key, "product");
        assert_eq!(second.unwrap().fields[0].key, "product");
    }

    #[tokio::test]
    async fn an_expired_snapshot_survives_a_failed_refresh() {
        let server = MockServer::start().await;
        // First mounted mock wins. The 200 answers the first load, then the refresh gets the 500.
        Mock::given(method("GET"))
            .and(path("/issue_statuses.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }]
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issue_statuses.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        for (path_name, body) in [
            (
                "/trackers.json",
                json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
            ),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [{ "id": 3, "name": "Normal" }] }),
            ),
            ("/queries.json", json!({ "queries": [], "total_count": 0 })),
            ("/custom_fields.json", json!({ "custom_fields": [] })),
        ] {
            Mock::given(method("GET"))
                .and(path(path_name))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        let catalog = catalog_for(&server.uri(), None, Some("40"));
        let first = catalog.get().await.unwrap();
        let loaded_at = first.loaded_at;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let second = catalog.get().await.unwrap();
        assert_eq!(second.loaded_at, loaded_at);
        let third = catalog.get().await.unwrap();
        assert_eq!(third.loaded_at, loaded_at);
    }

    #[tokio::test]
    async fn a_project_list_over_the_cap_is_an_error() {
        let server = MockServer::start().await;
        mount_vocab(&server).await;
        Mock::given(method("GET"))
            .and(path("/queries.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [],
                "total_count": 0
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projects": [{ "id": 1, "name": "Garden", "issue_custom_fields": [] }],
                "total_count": 5001
            })))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = catalog_for(&server.uri(), None, None);
        let err = catalog.get().await.unwrap_err().to_string();
        assert!(err.contains("5001"), "{err}");
        assert!(err.contains("5000"), "{err}");
    }

    #[tokio::test]
    async fn categories_cache_a_404_and_members_do_not_cache_a_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/garden/issue_categories.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/garden/memberships.json"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/garden/memberships.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "memberships": [
                    { "user": { "id": 4, "name": "Robin Example" } },
                    { "group": { "id": 8, "name": "Grounds Crew" } }
                ],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        let catalog = catalog_for(&server.uri(), None, None);
        assert!(catalog.categories("garden").await.unwrap().is_empty());
        assert!(catalog.categories("garden").await.unwrap().is_empty());
        assert!(catalog.members("garden").await.is_err());
        let members = catalog.members("garden").await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(
            catalog.resolve_member("garden", "grounds").await.unwrap(),
            vec![8]
        );
        let missing = catalog
            .resolve_category("garden", "Irrigation")
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("Known"), "{missing}");
    }

    #[tokio::test]
    async fn a_bind_failure_replaces_a_cold_failure_for_issue_reads() {
        let server = MockServer::start().await;
        for (at, body) in [
            (
                "/issue_statuses.json",
                json!({ "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }] }),
            ),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [{ "id": 3, "name": "Normal" }] }),
            ),
            ("/queries.json", json!({ "queries": [], "total_count": 0 })),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/trackers.json"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/trackers.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "trackers": [{ "id": 1, "name": "Bug" }]
            })))
            .expect(1)
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "custom_fields": [
                    { "id": 2, "name": "Product", "customized_type": "issue" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let catalog = catalog_for(&server.uri(), Some(r#"{"fields":{"widget":99}}"#), None);
        let first = catalog.issue_names().await.error.unwrap();
        assert!(first.contains("catalog unavailable"), "{first}");
        let err = catalog.get().await.unwrap_err();
        assert!(matches!(err, CatalogError::Bind(_)), "{err}");
        let second = catalog.issue_names().await;
        assert_eq!(
            second.error.as_deref(),
            Some("REDMINE_CONFIG does not match this Redmine. The server log has the details.")
        );
    }

    #[tokio::test]
    async fn a_missing_custom_field_refreshes_once_a_minute() {
        let server = MockServer::start().await;
        for (at, body) in [
            (
                "/issue_statuses.json",
                json!({ "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }] }),
            ),
            (
                "/trackers.json",
                json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
            ),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [{ "id": 3, "name": "Normal" }] }),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(2)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/queries.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [],
                "total_count": 0
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "custom_fields": [
                    { "id": 2, "name": "Product", "customized_type": "issue" }
                ]
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "custom_fields": [
                    { "id": 2, "name": "Product", "customized_type": "issue" },
                    { "id": 9, "name": "Widget", "customized_type": "issue" }
                ]
            })))
            .expect(1)
            .with_priority(2)
            .mount(&server)
            .await;

        let catalog = catalog_for(&server.uri(), None, None);
        let found = catalog.resolve_custom_field("widget").await.unwrap();
        assert_eq!(found.id, 9);
        let missing = catalog
            .resolve_custom_field("nope")
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("Unknown custom field"), "{missing}");
        assert!(missing.contains("widget"), "{missing}");
    }
}

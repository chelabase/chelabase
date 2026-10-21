//! `project:` channels (spec §5.3, R17 narrowing, R21, W34, S28): a join is allowed when the
//! project is in the person's `/internal/realtime/projects` list; events come in on
//! `chelabase.rt.project.<project_id>` and reach a socket only for databases its cached list
//! has. An event for a database the list doesn't have refreshes the list first and is
//! delivered only if the fresh list has it (R21: never by default). Within one 1 s window each
//! distinct event goes out once, in arrival order (W34, S28).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use chelabase_nats::rt::ProjectEvent;
use serde::Deserialize;
use uuid::Uuid;

/// The dedup window (W34).
pub const DEDUP_WINDOW: Duration = Duration::from_secs(1);

/// What a person may see: each visible project's visible database ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectView {
    pub tenant_id: Uuid,
    pub projects: BTreeMap<Uuid, BTreeSet<Uuid>>,
}

#[derive(Deserialize)]
struct ProjectsBody {
    tenant_id: Uuid,
    projects: Vec<ProjectEntry>,
}

#[derive(Deserialize)]
struct ProjectEntry {
    project_id: Uuid,
    database_ids: Vec<Uuid>,
}

impl ProjectView {
    /// From `/projects`' 200 body `{tenant_id, projects: [{project_id, database_ids}]}`.
    pub fn from_json(body: &[u8]) -> Result<Self, String> {
        let parsed: ProjectsBody =
            serde_json::from_slice(body).map_err(|e| format!("the projects answer: {e}"))?;
        let mut projects: BTreeMap<Uuid, BTreeSet<Uuid>> = BTreeMap::new();
        for p in parsed.projects {
            projects
                .entry(p.project_id)
                .or_default()
                .extend(p.database_ids);
        }
        Ok(Self {
            tenant_id: parsed.tenant_id,
            projects,
        })
    }
    pub fn has_project(&self, project_id: Uuid) -> bool {
        self.projects.contains_key(&project_id)
    }
}

/// The database an event is about.
pub fn event_database(event: &ProjectEvent) -> Uuid {
    match event {
        ProjectEvent::BranchStatus { database_id, .. }
        | ProjectEvent::BranchCreated { database_id, .. }
        | ProjectEvent::BranchDeleted { database_id, .. }
        | ProjectEvent::DatabaseCreated { database_id }
        | ProjectEvent::DatabaseDeleted { database_id }
        | ProjectEvent::PgDatabaseCreated { database_id, .. }
        | ProjectEvent::PgDatabaseDropped { database_id, .. } => *database_id,
    }
}

/// What to do with an event for one socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Deliver,
    /// The database isn't in the cached list: refresh it, then route again.
    Refresh,
    Drop,
}

/// R17/R21 for one socket's view. `refreshed` says the view was just fetched for this event, so
/// a database still missing is dropped instead of refreshed again.
pub fn route(
    view: &ProjectView,
    project_id: Uuid,
    event: &ProjectEvent,
    refreshed: bool,
) -> Delivery {
    let Some(databases) = view.projects.get(&project_id) else {
        return Delivery::Drop;
    };
    if databases.contains(&event_database(event)) {
        Delivery::Deliver
    } else if refreshed {
        Delivery::Drop
    } else {
        Delivery::Refresh
    }
}

/// W34/S28: drops an event identical to one admitted for the same project less than
/// [`DEDUP_WINDOW`] ago.
#[derive(Debug)]
pub struct ProjectDedup {
    window: Duration,
    admitted: HashMap<(Uuid, String), Instant>,
}

impl Default for ProjectDedup {
    fn default() -> Self {
        Self::with_window(DEDUP_WINDOW)
    }
}

impl ProjectDedup {
    pub fn new() -> Self {
        Self::default()
    }
    /// Another window than [`DEDUP_WINDOW`] (`Timings::project_dedup`).
    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            admitted: HashMap::new(),
        }
    }
    pub fn admit(&mut self, project_id: Uuid, event: &ProjectEvent, now: Instant) -> bool {
        // Entries past the window can't drop anything any more.
        let window = self.window;
        self.admitted
            .retain(|_, at| now.saturating_duration_since(*at) < window);
        let key = (
            project_id,
            serde_json::to_string(event).expect("an event serializes"),
        );
        if self.admitted.contains_key(&key) {
            return false;
        }
        self.admitted.insert(key, now);
        true
    }
}

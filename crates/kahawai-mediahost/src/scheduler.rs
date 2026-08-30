//! One process-wide admission authority for media filesystem and analysis work.
//!
//! Priority is semantic and never ages: waiting cannot turn optional loudness
//! into work more valuable than matching or subtitles. Resources are acquired
//! as one bundle, so a job never holds one disk while waiting for another.
//!
//! The two cost axes are explicit. Rebuild cost orders durable discovery
//! benefits (catalogue truth, exact AniDB identity, OCR inputs, skip markers,
//! then optional loudness); latency at point of use elevates viewer, admin and
//! up-next demand above that order. CPU capacity defaults to one heavy job.
//! Storage capacity defaults to one operation per filesystem device, with
//! configuration overrides for several paths sharing one backing store or an
//! array known to sustain parallel I/O. Thus an I/O-only header read on one
//! device may overlap a CPU decode on another without allowing two jobs to
//! contend accidentally for the same default resource.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Result, bail};
use kahawai_core::media::{CollectionConfig, MediahostSchedulerConfig};

const FALLBACK_DOMAIN: &str = "auto:unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    Demand = 0,
    CatalogFreshness = 1,
    Attachments = 2,
    Keyframes = 3,
    Geometry = 4,
    LocalMetadata = 5,
    Ed2k = 6,
    SubtitlePrewarm = 7,
    Segments = 8,
    Loudness = 9,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resources {
    cpu: usize,
    io: BTreeSet<String>,
    exclusive: BTreeSet<String>,
}

impl Resources {
    fn conflicts(&self, other: &Self) -> bool {
        (self.cpu != 0 && other.cpu != 0)
            || !self.io.is_disjoint(&other.io)
            || !self.exclusive.is_disjoint(&other.exclusive)
    }

    pub fn exclusive(mut self, resource: impl Into<String>) -> Self {
        self.exclusive.insert(resource.into());
        self
    }
}

struct Control {
    granted: AtomicBool,
    pause: AtomicBool,
    cancelled: AtomicBool,
    mutex: Mutex<()>,
    condvar: Condvar,
    notify: tokio::sync::Notify,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            granted: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl Control {
    fn wake(&self) {
        self.condvar.notify_all();
        self.notify.notify_waiters();
    }

    async fn wait_async(&self) -> Result<()> {
        loop {
            let notified = self.notify.notified();
            if self.cancelled.load(Ordering::Acquire) {
                bail!("scheduler job cancelled");
            }
            if self.granted.load(Ordering::Acquire) {
                return Ok(());
            }
            notified.await;
        }
    }

    fn wait_blocking(&self) -> Result<()> {
        let mut guard = self.mutex.lock().unwrap();
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                bail!("scheduler job cancelled");
            }
            if self.granted.load(Ordering::Acquire) {
                return Ok(());
            }
            guard = self.condvar.wait(guard).unwrap();
        }
    }
}

struct Pending {
    id: u64,
    sequence: u64,
    priority: Priority,
    base_priority: Priority,
    resources: Resources,
    owner: Option<String>,
    label: String,
    control: Arc<Control>,
    demand_sources: BTreeSet<(String, String, String)>,
}

struct Active {
    priority: Priority,
    base_priority: Priority,
    resources: Resources,
    owner: Option<String>,
    label: String,
    control: Arc<Control>,
    sequence: u64,
    demand_sources: BTreeSet<(String, String, String)>,
}

struct State {
    next_id: u64,
    next_sequence: u64,
    cpu_capacity: usize,
    io_capacity: BTreeMap<String, usize>,
    pending: Vec<Pending>,
    active: HashMap<u64, Active>,
    interactive: HashMap<u64, Resources>,
    hints: HashMap<(String, String, String, String), std::time::Instant>,
}

impl State {
    fn used(&self) -> (usize, BTreeMap<String, usize>) {
        let mut cpu = 0;
        let mut io = BTreeMap::new();
        for active in self.active.values() {
            cpu += active.resources.cpu;
            for domain in &active.resources.io {
                *io.entry(domain.clone()).or_insert(0) += 1;
            }
        }
        (cpu, io)
    }

    fn available(&self, resources: &Resources) -> bool {
        if self
            .interactive
            .values()
            .any(|interactive| interactive.conflicts(resources))
        {
            return false;
        }
        let (used_cpu, used_io) = self.used();
        if used_cpu + resources.cpu > self.cpu_capacity {
            return false;
        }
        resources.io.iter().all(|domain| {
            used_io.get(domain.as_str()).copied().unwrap_or(0)
                < self.io_capacity.get(domain).copied().unwrap_or(1)
        }) && self
            .active
            .values()
            .all(|active| active.resources.exclusive.is_disjoint(&resources.exclusive))
    }
}

struct Inner {
    state: Mutex<State>,
    roots: HashMap<String, String>,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    pub fn new(
        collections: &[CollectionConfig],
        config: &MediahostSchedulerConfig,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.cpu_slots > 0,
            "scheduler cpu_slots must be at least 1"
        );
        let mut override_roots = HashMap::new();
        let mut io_capacity = BTreeMap::new();
        for domain in &config.io_domains {
            let key = format!("config:{}", domain.name);
            io_capacity.insert(key.clone(), domain.max_concurrent);
            for root in &domain.roots {
                override_roots.insert(root.clone(), key.clone());
            }
        }

        let mut roots = HashMap::new();
        for root in collections
            .iter()
            .flat_map(CollectionConfig::resolved_roots)
        {
            let domain = override_roots
                .get(&root.path)
                .cloned()
                .unwrap_or_else(|| automatic_domain(&root.path));
            io_capacity.entry(domain.clone()).or_insert(1);
            roots.insert(root.token, domain);
        }
        io_capacity.entry(FALLBACK_DOMAIN.into()).or_insert(1);
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    next_id: 1,
                    next_sequence: 1,
                    cpu_capacity: config.cpu_slots,
                    io_capacity,
                    pending: Vec::new(),
                    active: HashMap::new(),
                    interactive: HashMap::new(),
                    hints: HashMap::new(),
                }),
                roots,
            }),
        })
    }

    pub fn resources<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a str>,
        cpu_heavy: bool,
    ) -> Resources {
        Resources {
            cpu: usize::from(cpu_heavy),
            io: roots
                .into_iter()
                .map(|root| {
                    self.inner
                        .roots
                        .get(root)
                        .cloned()
                        .unwrap_or_else(|| FALLBACK_DOMAIN.into())
                })
                .collect(),
            exclusive: BTreeSet::new(),
        }
    }

    pub async fn acquire(
        &self,
        priority: Priority,
        resources: Resources,
        owner: Option<String>,
        label: impl Into<String>,
    ) -> Result<JobPermit> {
        self.acquire_with_sources(priority, resources, owner, label, BTreeSet::new())
            .await
    }

    pub async fn acquire_segments(
        &self,
        resources: Resources,
        owner: Option<String>,
        label: impl Into<String>,
        job: &kahawai_proto::v1::DetectSegments,
    ) -> Result<JobPermit> {
        let priority = self.segment_priority(job);
        let demand_sources = job
            .episodes
            .iter()
            .filter_map(|episode| episode.source.as_ref())
            .map(|source| {
                (
                    job.collection_id.clone(),
                    source.root_token.clone(),
                    source.path_rel.clone(),
                )
            })
            .collect();
        self.acquire_with_sources(priority, resources, owner, label, demand_sources)
            .await
    }

    async fn acquire_with_sources(
        &self,
        priority: Priority,
        resources: Resources,
        owner: Option<String>,
        label: impl Into<String>,
        demand_sources: BTreeSet<(String, String, String)>,
    ) -> Result<JobPermit> {
        let label = label.into();
        let control = Arc::new(Control::default());
        let id = {
            let mut state = self.inner.state.lock().unwrap();
            let id = state.next_id;
            state.next_id += 1;
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            state.pending.push(Pending {
                id,
                sequence,
                priority,
                base_priority: priority,
                resources,
                owner,
                label,
                control: control.clone(),
                demand_sources,
            });
            request_preemption(&mut state);
            dispatch(&mut state);
            id
        };
        let mut pending = PendingGuard {
            inner: self.inner.clone(),
            id,
            armed: true,
        };
        control.wait_async().await?;
        pending.armed = false;
        Ok(JobPermit(Arc::new(PermitLease {
            inner: self.inner.clone(),
            id,
            control,
        })))
    }

    /// Interactive work is never capacity-gated. Its presence pauses only
    /// conflicting scheduled work and prevents a new conflicting admission.
    pub fn enter_interactive(
        &self,
        resources: Resources,
        label: impl Into<String>,
    ) -> InteractiveGuard {
        let label = label.into();
        let id = {
            let mut state = self.inner.state.lock().unwrap();
            let id = state.next_id;
            state.next_id += 1;
            for active in state.active.values() {
                if active.resources.conflicts(&resources) {
                    active.control.pause.store(true, Ordering::Release);
                }
            }
            state.interactive.insert(id, resources);
            id
        };
        tracing::debug!(job = %label, "interactive media work entered scheduler");
        InteractiveGuard {
            inner: self.inner.clone(),
            id,
            label,
        }
    }

    pub fn cancel_owner(&self, owner: &str) {
        let mut state = self.inner.state.lock().unwrap();
        for pending in state
            .pending
            .iter()
            .filter(|job| job.owner.as_deref() == Some(owner))
        {
            pending.control.cancelled.store(true, Ordering::Release);
            pending.control.wake();
        }
        state
            .pending
            .retain(|job| job.owner.as_deref() != Some(owner));
        for active in state
            .active
            .values()
            .filter(|job| job.owner.as_deref() == Some(owner))
        {
            active.control.cancelled.store(true, Ordering::Release);
            active.control.pause.store(true, Ordering::Release);
            active.control.wake();
        }
        state
            .hints
            .retain(|(hint_owner, _, _, _), _| hint_owner != owner);
        request_preemption(&mut state);
        dispatch(&mut state);
    }

    pub fn hint_segment(
        &self,
        owner: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        ttl: std::time::Duration,
    ) {
        let mut state = self.inner.state.lock().unwrap();
        let now = std::time::Instant::now();
        state.hints.retain(|_, expires| *expires > now);
        state.hints.insert(
            (
                owner.to_string(),
                collection_id.to_string(),
                root_token.to_string(),
                path_rel.to_string(),
            ),
            now + ttl,
        );
        let key = (
            collection_id.to_string(),
            root_token.to_string(),
            path_rel.to_string(),
        );
        for pending in &mut state.pending {
            if pending.demand_sources.contains(&key) {
                pending.priority = Priority::Demand;
            }
        }
        for active in state.active.values_mut() {
            if active.demand_sources.contains(&key) {
                active.priority = Priority::Demand;
            }
        }
        request_preemption(&mut state);
        dispatch(&mut state);
    }

    pub fn segment_priority(&self, job: &kahawai_proto::v1::DetectSegments) -> Priority {
        let mut state = self.inner.state.lock().unwrap();
        let now = std::time::Instant::now();
        state.hints.retain(|_, expires| *expires > now);
        let demanded = job.episodes.iter().any(|episode| {
            let Some(source) = &episode.source else {
                return false;
            };
            state.hints.keys().any(|(_, collection, root, path)| {
                collection == &job.collection_id
                    && root == &source.root_token
                    && path == &source.path_rel
            })
        });
        if demanded {
            Priority::Demand
        } else {
            Priority::Segments
        }
    }
}

#[cfg(unix)]
fn automatic_domain(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .map(|metadata| format!("auto:dev:{}", metadata.dev()))
        .unwrap_or_else(|_| FALLBACK_DOMAIN.into())
}

#[cfg(windows)]
fn automatic_domain(path: &std::path::Path) -> String {
    use std::os::windows::fs::MetadataExt as _;
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.volume_serial_number())
        .map(|serial| format!("auto:volume:{serial}"))
        .unwrap_or_else(|| FALLBACK_DOMAIN.into())
}

#[cfg(not(any(unix, windows)))]
fn automatic_domain(_path: &std::path::Path) -> String {
    FALLBACK_DOMAIN.into()
}

fn refresh_demand_priorities(state: &mut State) {
    let now = std::time::Instant::now();
    state.hints.retain(|_, expires| *expires > now);
    let hinted = state
        .hints
        .keys()
        .map(|(_, collection, root, path)| (collection.clone(), root.clone(), path.clone()))
        .collect::<BTreeSet<_>>();
    let demanded = |sources: &BTreeSet<(String, String, String)>| {
        sources.iter().any(|source| hinted.contains(source))
    };
    for pending in &mut state.pending {
        pending.priority = if demanded(&pending.demand_sources) {
            Priority::Demand
        } else {
            pending.base_priority
        };
    }
    for active in state.active.values_mut() {
        active.priority = if demanded(&active.demand_sources) {
            Priority::Demand
        } else {
            active.base_priority
        };
    }
}

fn request_preemption(state: &mut State) {
    refresh_demand_priorities(state);
    for active in state.active.values() {
        let interactive_conflict = state
            .interactive
            .values()
            .any(|interactive| interactive.conflicts(&active.resources));
        active
            .control
            .pause
            .store(interactive_conflict, Ordering::Release);
    }
    let (used_cpu, used_io) = state.used();
    let requests = state
        .pending
        .iter()
        .map(|pending| {
            let cpu_full =
                pending.resources.cpu != 0 && used_cpu + pending.resources.cpu > state.cpu_capacity;
            let full_io = pending
                .resources
                .io
                .iter()
                .filter(|domain| {
                    used_io.get(domain.as_str()).copied().unwrap_or(0)
                        >= state.io_capacity.get(*domain).copied().unwrap_or(1)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            (
                pending.priority,
                pending.resources.cpu,
                cpu_full,
                full_io,
                pending.resources.exclusive.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (priority, requested_cpu, cpu_full, full_io, exclusive) in requests {
        for active in state.active.values() {
            let blocks_cpu = cpu_full && requested_cpu != 0 && active.resources.cpu != 0;
            let blocks_io = !active.resources.io.is_disjoint(&full_io);
            let blocks_exclusive = !active.resources.exclusive.is_disjoint(&exclusive);
            if priority < active.priority && (blocks_cpu || blocks_io || blocks_exclusive) {
                active.control.pause.store(true, Ordering::Release);
            }
        }
    }
}

fn dispatch(state: &mut State) {
    refresh_demand_priorities(state);
    loop {
        let mut order: Vec<usize> = (0..state.pending.len()).collect();
        order.sort_by_key(|index| {
            let job = &state.pending[*index];
            (job.priority, job.sequence)
        });
        let mut selected = None;
        for index in order {
            let job = &state.pending[index];
            if state.available(&job.resources) {
                selected = Some(index);
                break;
            }
        }
        let Some(index) = selected else { break };
        let pending = state.pending.swap_remove(index);
        pending.control.pause.store(false, Ordering::Release);
        pending.control.granted.store(true, Ordering::Release);
        tracing::debug!(
            job = %pending.label,
            priority = ?pending.priority,
            io_domains = ?pending.resources.io,
            cpu = pending.resources.cpu,
            "scheduled media work admitted"
        );
        pending.control.wake();
        state.active.insert(
            pending.id,
            Active {
                priority: pending.priority,
                base_priority: pending.base_priority,
                resources: pending.resources,
                owner: pending.owner,
                label: pending.label,
                control: pending.control,
                sequence: pending.sequence,
                demand_sources: pending.demand_sources,
            },
        );
    }
}

fn yield_job(inner: &Arc<Inner>, id: u64) {
    let mut state = inner.state.lock().unwrap();
    let Some(active) = state.active.remove(&id) else {
        return;
    };
    active.control.granted.store(false, Ordering::Release);
    tracing::debug!(job = %active.label, "scheduled media work yielded");
    state.pending.push(Pending {
        id,
        sequence: active.sequence,
        priority: active.priority,
        base_priority: active.base_priority,
        resources: active.resources,
        owner: active.owner,
        label: active.label,
        control: active.control,
        demand_sources: active.demand_sources,
    });
    request_preemption(&mut state);
    dispatch(&mut state);
}

fn finish_job(inner: &Arc<Inner>, id: u64) {
    let mut state = inner.state.lock().unwrap();
    if let Some(active) = state.active.remove(&id) {
        tracing::debug!(job = %active.label, "scheduled media work completed");
    }
    if let Some(index) = state.pending.iter().position(|job| job.id == id) {
        state.pending.swap_remove(index);
    }
    request_preemption(&mut state);
    dispatch(&mut state);
}

struct PendingGuard {
    inner: Arc<Inner>,
    id: u64,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            finish_job(&self.inner, self.id);
        }
    }
}

struct PermitLease {
    inner: Arc<Inner>,
    id: u64,
    control: Arc<Control>,
}

impl Drop for PermitLease {
    fn drop(&mut self) {
        finish_job(&self.inner, self.id);
    }
}

#[derive(Clone)]
pub struct JobPermit(Arc<PermitLease>);

impl JobPermit {
    pub async fn checkpoint(&self) -> Result<()> {
        if self.0.control.cancelled.load(Ordering::Acquire) {
            bail!("scheduler job cancelled");
        }
        if !self.0.control.pause.load(Ordering::Acquire) {
            return Ok(());
        }
        yield_job(&self.0.inner, self.0.id);
        self.0.control.wait_async().await
    }

    pub fn checkpoint_blocking(&self) -> Result<()> {
        if self.0.control.cancelled.load(Ordering::Acquire) {
            bail!("scheduler job cancelled");
        }
        if !self.0.control.pause.load(Ordering::Acquire) {
            return Ok(());
        }
        yield_job(&self.0.inner, self.0.id);
        self.0.control.wait_blocking()
    }
}

pub struct InteractiveGuard {
    inner: Arc<Inner>,
    id: u64,
    label: String,
}

impl Drop for InteractiveGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.interactive.remove(&self.id);
        request_preemption(&mut state);
        dispatch(&mut state);
        tracing::debug!(job = %self.label, "interactive media work left scheduler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_core::media::MediahostIoDomainConfig;

    fn scheduler() -> (tempfile::TempDir, Scheduler, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let collections = vec![CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![a.clone(), b.clone()],
        }];
        let config = MediahostSchedulerConfig {
            cpu_slots: 1,
            io_domains: vec![
                MediahostIoDomainConfig {
                    name: "a".into(),
                    roots: vec![a],
                    max_concurrent: 1,
                },
                MediahostIoDomainConfig {
                    name: "b".into(),
                    roots: vec![b],
                    max_concurrent: 1,
                },
            ],
        };
        let scheduler = Scheduler::new(&collections, &config).unwrap();
        let roots: Vec<_> = collections[0]
            .resolved_roots()
            .map(|root| root.token)
            .collect();
        (temp, scheduler, roots[0].clone(), roots[1].clone())
    }

    #[tokio::test]
    async fn independent_storage_runs_together_but_cpu_is_bounded() {
        let (_temp, scheduler, a, b) = scheduler();
        let first = scheduler
            .acquire(
                Priority::Segments,
                scheduler.resources([a.as_str()], true),
                None,
                "first",
            )
            .await
            .unwrap();
        let io_only = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([b.as_str()], false),
                None,
                "io-only",
            )
            .await
            .unwrap();
        let scheduler2 = scheduler.clone();
        let b2 = b.clone();
        let blocked = tokio::spawn(async move {
            scheduler2
                .acquire(
                    Priority::Demand,
                    scheduler2.resources([b2.as_str()], true),
                    None,
                    "cpu-blocked",
                )
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        drop(first);
        drop(io_only);
        tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn higher_priority_pauses_and_then_resumes_the_same_permit() {
        let (_temp, scheduler, a, _) = scheduler();
        let low = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([a.as_str()], false),
                None,
                "loudness",
            )
            .await
            .unwrap();
        let scheduler2 = scheduler.clone();
        let a2 = a.clone();
        let high = tokio::spawn(async move {
            scheduler2
                .acquire(
                    Priority::Segments,
                    scheduler2.resources([a2.as_str()], false),
                    None,
                    "segments",
                )
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        let low2 = low.clone();
        let paused = tokio::task::spawn_blocking(move || low2.checkpoint_blocking());
        let high = tokio::time::timeout(std::time::Duration::from_secs(1), high)
            .await
            .unwrap()
            .unwrap();
        assert!(!paused.is_finished());
        drop(high);
        tokio::time::timeout(std::time::Duration::from_secs(1), paused)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn interactive_work_only_pauses_a_conflicting_domain() {
        let (_temp, scheduler, a, b) = scheduler();
        let on_a = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([a.as_str()], false),
                None,
                "a",
            )
            .await
            .unwrap();
        let on_b = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([b.as_str()], false),
                None,
                "b",
            )
            .await
            .unwrap();
        let foreground =
            scheduler.enter_interactive(scheduler.resources([a.as_str()], false), "viewer");
        assert!(on_a.0.control.pause.load(Ordering::Acquire));
        assert!(!on_b.0.control.pause.load(Ordering::Acquire));
        drop(foreground);
    }

    #[tokio::test]
    async fn older_low_value_work_never_ages_ahead_of_fixed_priority() {
        let (_temp, scheduler, a, _) = scheduler();
        let blocker = scheduler.enter_interactive(scheduler.resources([a.as_str()], false), "hold");
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_high_tx, release_high_rx) = tokio::sync::oneshot::channel();
        let low_scheduler = scheduler.clone();
        let low_root = a.clone();
        let low_started = started_tx.clone();
        let low = tokio::spawn(async move {
            let _permit = low_scheduler
                .acquire(
                    Priority::Loudness,
                    low_scheduler.resources([low_root.as_str()], false),
                    None,
                    "old loudness",
                )
                .await
                .unwrap();
            low_started.send("loudness").unwrap();
        });
        tokio::task::yield_now().await;
        let high_scheduler = scheduler.clone();
        let high_root = a.clone();
        let high = tokio::spawn(async move {
            let _permit = high_scheduler
                .acquire(
                    Priority::Segments,
                    high_scheduler.resources([high_root.as_str()], false),
                    None,
                    "new segments",
                )
                .await
                .unwrap();
            started_tx.send("segments").unwrap();
            let _ = release_high_rx.await;
        });
        tokio::task::yield_now().await;
        drop(blocker);
        assert_eq!(started_rx.recv().await, Some("segments"));
        release_high_tx.send(()).unwrap();
        high.await.unwrap();
        assert_eq!(started_rx.recv().await, Some("loudness"));
        low.await.unwrap();
    }

    #[tokio::test]
    async fn spare_cpu_capacity_does_not_preempt_independent_work() {
        let (_temp, scheduler, a, b) = scheduler();
        scheduler.inner.state.lock().unwrap().cpu_capacity = 2;
        let first = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([a.as_str()], true),
                None,
                "cpu a",
            )
            .await
            .unwrap();
        let second = scheduler
            .acquire(
                Priority::Segments,
                scheduler.resources([b.as_str()], true),
                None,
                "cpu b",
            )
            .await
            .unwrap();
        assert!(!first.0.control.pause.load(Ordering::Acquire));
        drop((first, second));
    }

    #[tokio::test]
    async fn io_blocking_does_not_pause_an_unrelated_cpu_job_when_a_slot_is_spare() {
        let (_temp, scheduler, a, b) = scheduler();
        scheduler.inner.state.lock().unwrap().cpu_capacity = 2;
        let io_a = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([a.as_str()], false),
                None,
                "io a",
            )
            .await
            .unwrap();
        let cpu_b = scheduler
            .acquire(
                Priority::Loudness,
                scheduler.resources([b.as_str()], true),
                None,
                "cpu b",
            )
            .await
            .unwrap();
        let high_scheduler = scheduler.clone();
        let high_root = a.clone();
        let high = tokio::spawn(async move {
            high_scheduler
                .acquire(
                    Priority::Segments,
                    high_scheduler.resources([high_root.as_str()], true),
                    None,
                    "segments a",
                )
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(io_a.0.control.pause.load(Ordering::Acquire));
        assert!(!cpu_b.0.control.pause.load(Ordering::Acquire));
        drop(io_a);
        let high = high.await.unwrap();
        drop((high, cpu_b));
    }

    #[tokio::test]
    async fn named_exclusive_resources_remain_singleton_with_extra_cpu_slots() {
        let (_temp, scheduler, a, b) = scheduler();
        scheduler.inner.state.lock().unwrap().cpu_capacity = 2;
        let first = scheduler
            .acquire(
                Priority::Segments,
                scheduler
                    .resources([a.as_str()], true)
                    .exclusive("segment-analysis"),
                None,
                "season a",
            )
            .await
            .unwrap();
        let second_scheduler = scheduler.clone();
        let second_root = b.clone();
        let second = tokio::spawn(async move {
            second_scheduler
                .acquire(
                    Priority::Segments,
                    second_scheduler
                        .resources([second_root.as_str()], true)
                        .exclusive("segment-analysis"),
                    None,
                    "season b",
                )
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_late_hint_promotes_an_already_queued_segment_job() {
        let (_temp, scheduler, a, _) = scheduler();
        let blocker = scheduler.enter_interactive(scheduler.resources([a.as_str()], false), "hold");
        let job = kahawai_proto::v1::DetectSegments {
            request_id: "season".into(),
            collection_id: "series".into(),
            episodes: vec![kahawai_proto::v1::SegmentEpisode {
                source: Some(kahawai_proto::v1::SourcePath::new(&a, "episode.mkv")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let queued_scheduler = scheduler.clone();
        let queued_job = job.clone();
        let queued_root = a.clone();
        let queued = tokio::spawn(async move {
            queued_scheduler
                .acquire_segments(
                    queued_scheduler.resources([queued_root.as_str()], false),
                    Some("hub:a".into()),
                    "queued season",
                    &queued_job,
                )
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        scheduler.hint_segment(
            "hub:a",
            "series",
            &a,
            "episode.mkv",
            std::time::Duration::from_secs(60),
        );
        assert_eq!(
            scheduler.inner.state.lock().unwrap().pending[0].priority,
            Priority::Demand
        );
        drop(blocker);
        tokio::time::timeout(std::time::Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
    }
}

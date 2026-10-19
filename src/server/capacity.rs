use crate::domain::ModelProfile;
use crate::store::Store;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// A released slot favours the next request of the session that held it for
/// this long, so a multi-turn session keeps the runtime's prompt cache warm.
pub const AFFINITY_WINDOW: Duration = Duration::from_secs(10);
/// A waiter rises one class for each of these it waits, so no class starves.
pub const AGING_STEP: Duration = Duration::from_secs(60);
/// A waiter whose wait ran out keeps its place this long for a retry that
/// names its ticket.
pub const TICKET_GRACE: Duration = Duration::from_secs(60);
/// A job the scheduler stops asking for (cancelled, failed) leaves the line.
/// The scheduler asks every 2 s, but one slow tick (a node probe that times
/// out) can hold it for 10 s or more.
pub const JOB_WAITER_EXPIRY: Duration = Duration::from_secs(30);
pub const MAX_WAITERS_PER_SLOT: usize = 16;
pub const MAX_WAITERS_PER_TOKEN: usize = 4;
/// The longest a request is held waiting: under a typical reverse proxy's two-minute timeout, so
/// the client hears the queue's answer rather than a gateway error.
pub const MAX_WAIT: Duration = Duration::from_secs(110);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SlotKey {
    pub node_id: String,
    pub runtime_profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupant {
    Interactive {
        lease_id: String,
        session: Option<String>,
    },
    Job {
        job_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupied {
    Interactive,
    Job {
        job_id: String,
    },
    /// The slot is free but held for a waiter that outranks this request.
    Reserved,
}

/// Who a waiting request is for, most urgent first. A token may ask for
/// `Owner` or `Planner` only with the `inference:priority` permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaitClass {
    /// A person waiting on the answer.
    Owner,
    /// Planning and management turns that unblock other work.
    Planner,
    /// Implementation work; requests without a class.
    Worker,
    /// Durable jobs and batch work.
    Background,
}

impl WaitClass {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Self::Owner),
            "planner" => Some(Self::Planner),
            "worker" => Some(Self::Worker),
            "background" => Some(Self::Background),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Planner => "planner",
            Self::Worker => "worker",
            Self::Background => "background",
        }
    }
}

/// How a request asks to wait for its slot.
#[derive(Clone, Debug)]
pub struct WaitRequest {
    pub class: WaitClass,
    /// The client's conversation, for cache affinity.
    pub session: Option<String>,
    /// The application token, for the per-token cap.
    pub token_id: String,
    /// A ticket from an earlier wait that ran out: the request keeps its place.
    pub ticket: Option<String>,
    /// How long to hold the request; zero answers at once.
    pub wait: Duration,
}

/// Where a request stands when its wait ran out, or when it did not wait.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueuePlace {
    /// 1 is next.
    pub position: usize,
    /// Present when the request kept a place: send it back to keep it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub holder_kind: Option<&'static str>,
    pub waiting: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitError {
    Busy(QueuePlace),
    /// Too many waiters on the slot, or for this token.
    Full {
        per_token: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WaiterView {
    pub class: &'static str,
    pub kind: &'static str,
    pub waited_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueueView {
    pub node_id: String,
    pub runtime_profile: String,
    pub holder_kind: Option<&'static str>,
    pub waiting: Vec<WaiterView>,
}

#[derive(Clone, Debug)]
struct Waiter {
    ticket: String,
    class: WaitClass,
    session: Option<String>,
    token_id: Option<String>,
    job_id: Option<String>,
    enqueued: Instant,
    /// Set while the waiter is not itself waiting: a ticket's grace, or a job
    /// between scheduler ticks. It leaves the line after this.
    held_until: Option<Instant>,
}

impl Waiter {
    /// May take the slot now: a request that is waiting, or a job still asked for.
    fn present(&self, now: Instant) -> bool {
        match self.held_until {
            None => true,
            Some(until) => self.job_id.is_some() && until > now,
        }
    }

    fn rank(&self, now: Instant) -> u64 {
        rank(self.class, now.saturating_duration_since(self.enqueued))
    }
}

/// A class, raised one step per `AGING_STEP` waited.
fn rank(class: WaitClass, waited: Duration) -> u64 {
    (class as u64).saturating_sub(waited.as_secs() / AGING_STEP.as_secs())
}

#[derive(Default)]
struct SlotQueue {
    waiters: Vec<Waiter>,
    /// The session that last held the slot, and when it let go.
    last_release: Option<(Option<String>, Instant)>,
    notify: Arc<Notify>,
}

impl SlotQueue {
    fn prune(&mut self, now: Instant) {
        let before = self.waiters.len();
        self.waiters
            .retain(|waiter| waiter.held_until.is_none_or(|until| until > now));
        // A waiter that left may have been holding a free slot for itself.
        if self.waiters.len() != before {
            self.notify.notify_waiters();
        }
    }

    /// Requests waiting now: what the caps count. Jobs and kept places are not here.
    fn live_requests(&self) -> impl Iterator<Item = &Waiter> {
        self.waiters
            .iter()
            .filter(|waiter| waiter.job_id.is_none() && waiter.held_until.is_none())
    }

    fn affine(&self, now: Instant) -> Option<&str> {
        self.last_release
            .as_ref()
            .filter(|(_, at)| now.saturating_duration_since(*at) < AFFINITY_WINDOW)
            .and_then(|(session, _)| session.as_deref())
    }

    /// The order the slot is handed out in: aged class, then the session that
    /// just let go of it, then arrival.
    fn key(
        &self,
        now: Instant,
        rank: u64,
        session: Option<&str>,
        enqueued: Instant,
    ) -> (u64, bool, Instant) {
        let affine = self.affine(now).is_some_and(|last| session == Some(last));
        (rank, !affine, enqueued)
    }

    fn waiter_key(&self, now: Instant, waiter: &Waiter) -> (u64, bool, Instant) {
        self.key(
            now,
            waiter.rank(now),
            waiter.session.as_deref(),
            waiter.enqueued,
        )
    }

    /// The present waiter the slot goes to next, other than `except`.
    fn next(&self, now: Instant, except: Option<&str>) -> Option<(u64, bool, Instant)> {
        self.waiters
            .iter()
            .filter(|waiter| waiter.present(now) && Some(waiter.ticket.as_str()) != except)
            .map(|waiter| self.waiter_key(now, waiter))
            .min()
    }

    /// 1 plus every waiter ahead of `key`, present or keeping a place.
    fn position(&self, now: Instant, key: (u64, bool, Instant), except: Option<&str>) -> usize {
        1 + self
            .waiters
            .iter()
            .filter(|waiter| Some(waiter.ticket.as_str()) != except)
            .filter(|waiter| self.waiter_key(now, waiter) < key)
            .count()
    }
}

#[derive(Default)]
struct Slots {
    occupants: HashMap<SlotKey, Occupant>,
    queues: HashMap<SlotKey, SlotQueue>,
}

impl Slots {
    /// Take a free slot for a request ranked `key`, unless a waiter outranks it.
    fn may_take(
        &self,
        slot: &SlotKey,
        now: Instant,
        key: (u64, bool, Instant),
        except: Option<&str>,
    ) -> Result<(), Occupied> {
        if let Some(occupant) = self.occupants.get(slot) {
            return Err(occupied(occupant));
        }
        match self
            .queues
            .get(slot)
            .and_then(|queue| queue.next(now, except))
        {
            Some(best) if best < key => Err(Occupied::Reserved),
            _ => Ok(()),
        }
    }

    fn holder_kind(&self, slot: &SlotKey) -> Option<&'static str> {
        self.occupants.get(slot).map(|occupant| match occupant {
            Occupant::Interactive { .. } => "interactive",
            Occupant::Job { .. } => "job",
        })
    }

    fn release(&mut self, slot: &SlotKey, session: Option<String>) {
        self.occupants.remove(slot);
        let queue = self.queues.entry(slot.clone()).or_default();
        queue.last_release = Some((session, Instant::now()));
        queue.notify.notify_waiters();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OccupancyView {
    pub node_id: String,
    pub runtime_profile: String,
    pub kind: String,
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveLease {
    pub node_id: String,
    pub runtime_profile: String,
    pub lease_id: String,
}

#[derive(Clone)]
pub struct Occupancy {
    inner: Arc<Mutex<Slots>>,
    requests: Arc<Mutex<HashSet<String>>>,
}

pub struct JobRequestGuard {
    requests: Arc<Mutex<HashSet<String>>>,
    job_id: String,
}

impl Drop for JobRequestGuard {
    fn drop(&mut self) {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.job_id);
    }
}

pub struct InteractiveGuard {
    occupancy: Occupancy,
    slot: SlotKey,
    lease_id: String,
}

impl Occupancy {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Slots::default())),
            requests: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn try_job_request(&self, id: &str) -> Option<JobRequestGuard> {
        let mut requests = self.requests.lock().unwrap_or_else(|p| p.into_inner());
        requests.insert(id.into()).then(|| JobRequestGuard {
            requests: self.requests.clone(),
            job_id: id.into(),
        })
    }

    pub fn exclusive_slot(profile: &ModelProfile) -> Option<SlotKey> {
        if profile.runtime_profile.as_deref() != Some("ollama") && !is_coding_profile(profile) {
            return None;
        }
        Some(SlotKey {
            node_id: nonempty(profile.node_id.as_deref())?.to_owned(),
            runtime_profile: nonempty(profile.runtime_profile.as_deref())?.to_owned(),
        })
    }

    pub fn recover(&self, store: &Store) -> Result<()> {
        for job in store.occupying_jobs()? {
            let Some(package) = store.runner_package(&job.id)? else {
                continue;
            };
            let Some(slot) = Self::exclusive_slot(&package.model) else {
                continue;
            };
            let _ = self.try_claim_job(&slot, &job.id);
        }
        Ok(())
    }

    /// Take the slot now or not at all, for a request that does not wait.
    /// It still queues behind nobody: a waiter that outranks it keeps the slot.
    pub fn try_claim_interactive(
        &self,
        slot: SlotKey,
        lease_id: impl Into<String>,
    ) -> Result<InteractiveGuard, Occupied> {
        let lease_id = lease_id.into();
        let now = Instant::now();
        let mut slots = self.lock();
        if let Some(queue) = slots.queues.get_mut(&slot) {
            queue.prune(now);
        }
        let key = slots
            .queues
            .get(&slot)
            .map(|queue| queue.key(now, WaitClass::Worker as u64, None, now))
            .unwrap_or((WaitClass::Worker as u64, true, now));
        slots.may_take(&slot, now, key, None)?;
        slots.occupants.insert(
            slot.clone(),
            Occupant::Interactive {
                lease_id: lease_id.clone(),
                session: None,
            },
        );
        Ok(InteractiveGuard {
            occupancy: self.clone(),
            slot,
            lease_id,
        })
    }

    /// Take the slot for a request, waiting up to `request.wait` in line.
    /// Dropping the future (the client went away) leaves the line.
    pub async fn claim_interactive(
        &self,
        slot: SlotKey,
        lease_id: impl Into<String>,
        request: &WaitRequest,
    ) -> Result<InteractiveGuard, WaitError> {
        let lease_id = lease_id.into();
        let wait = request.wait.min(MAX_WAIT);
        let deadline = Instant::now() + wait;
        let mut place = WaitPlace {
            occupancy: self.clone(),
            slot: slot.clone(),
            ticket: None,
        };
        loop {
            let mut notify = {
                let now = Instant::now();
                let mut slots = self.lock();
                let queue = slots.queues.entry(slot.clone()).or_default();
                queue.prune(now);
                if place.ticket.is_none() {
                    // A retry naming its ticket takes back the place it kept.
                    if let Some(ticket) = request.ticket.as_deref()
                        && let Some(waiter) = queue.waiters.iter_mut().find(|waiter| {
                            // Only a place kept after its wait ran out: a
                            // ticket another request is waiting on is not shared.
                            waiter.ticket == ticket
                                && waiter.job_id.is_none()
                                && waiter.held_until.is_some()
                                && waiter.token_id.as_deref() == Some(request.token_id.as_str())
                        })
                    {
                        waiter.held_until = None;
                        waiter.class = waiter.class.min(request.class);
                        place.ticket = Some(ticket.to_owned());
                    }
                }
                let (rank_now, enqueued, session) =
                    match place.ticket.as_deref().and_then(|ticket| {
                        queue.waiters.iter().find(|waiter| waiter.ticket == ticket)
                    }) {
                        Some(waiter) => (waiter.rank(now), waiter.enqueued, waiter.session.clone()),
                        None => (request.class as u64, now, request.session.clone()),
                    };
                let key = queue.key(now, rank_now, session.as_deref(), enqueued);
                if slots
                    .may_take(&slot, now, key, place.ticket.as_deref())
                    .is_ok()
                {
                    let queue = slots.queues.entry(slot.clone()).or_default();
                    if let Some(ticket) = place.ticket.take() {
                        queue.waiters.retain(|waiter| waiter.ticket != ticket);
                    }
                    slots.occupants.insert(
                        slot.clone(),
                        Occupant::Interactive {
                            lease_id: lease_id.clone(),
                            session: request.session.clone(),
                        },
                    );
                    return Ok(InteractiveGuard {
                        occupancy: self.clone(),
                        slot,
                        lease_id,
                    });
                }
                let holder_kind = slots.holder_kind(&slot);
                let queue = slots.queues.entry(slot.clone()).or_default();
                if now >= deadline {
                    let waiting = queue
                        .waiters
                        .iter()
                        .filter(|waiter| waiter.present(now))
                        .count();
                    let position = queue.position(now, key, place.ticket.as_deref());
                    // A request that waited keeps its place for a retry; one
                    // that asked not to wait never had one.
                    let ticket = place.ticket.take();
                    if let Some(ticket) = ticket.as_deref()
                        && let Some(waiter) = queue
                            .waiters
                            .iter_mut()
                            .find(|waiter| waiter.ticket == ticket)
                    {
                        waiter.held_until = Some(now + TICKET_GRACE);
                    }
                    return Err(WaitError::Busy(QueuePlace {
                        position,
                        ticket,
                        holder_kind,
                        waiting,
                    }));
                }
                if place.ticket.is_none() {
                    if queue.live_requests().count() >= MAX_WAITERS_PER_SLOT {
                        return Err(WaitError::Full { per_token: false });
                    }
                    let mine = queue
                        .live_requests()
                        .filter(|waiter| {
                            waiter.token_id.as_deref() == Some(request.token_id.as_str())
                        })
                        .count();
                    if mine >= MAX_WAITERS_PER_TOKEN {
                        return Err(WaitError::Full { per_token: true });
                    }
                    let ticket = uuid::Uuid::new_v4().to_string();
                    queue.waiters.push(Waiter {
                        ticket: ticket.clone(),
                        class: request.class,
                        session: request.session.clone(),
                        token_id: Some(request.token_id.clone()),
                        job_id: None,
                        enqueued: now,
                        held_until: None,
                    });
                    place.ticket = Some(ticket);
                }
                // Registered while the lock is held, so a release between this
                // look and the wait below still wakes it. A periodic look covers
                // aging and jobs whose turn passed.
                let mut notified = Box::pin(queue.notify.clone().notified_owned());
                notified.as_mut().enable();
                notified
            };
            let until = deadline.min(Instant::now() + Duration::from_secs(1));
            tokio::select! {
                _ = notify.as_mut() => {}
                _ = tokio::time::sleep_until(until.into()) => {}
            }
        }
    }

    /// Take the slot for a job, which the scheduler asks for every tick. A job
    /// that cannot have it waits in line as background work, rising as it waits.
    pub fn try_claim_job(&self, slot: &SlotKey, job_id: &str) -> Result<(), Occupied> {
        let now = Instant::now();
        let ticket = format!("job:{job_id}");
        let mut slots = self.lock();
        if let Some(Occupant::Job { job_id: existing }) = slots.occupants.get(slot)
            && existing == job_id
        {
            return Ok(());
        }
        let queue = slots.queues.entry(slot.clone()).or_default();
        queue.prune(now);
        let waiter = match queue
            .waiters
            .iter_mut()
            .find(|waiter| waiter.ticket == ticket)
        {
            Some(waiter) => {
                waiter.held_until = Some(now + JOB_WAITER_EXPIRY);
                waiter.clone()
            }
            None => {
                let waiter = Waiter {
                    ticket: ticket.clone(),
                    class: WaitClass::Background,
                    session: None,
                    token_id: None,
                    job_id: Some(job_id.to_owned()),
                    enqueued: now,
                    held_until: Some(now + JOB_WAITER_EXPIRY),
                };
                queue.waiters.push(waiter.clone());
                waiter
            }
        };
        let key = queue.waiter_key(now, &waiter);
        slots.may_take(slot, now, key, Some(&ticket))?;
        if let Some(queue) = slots.queues.get_mut(slot) {
            queue.waiters.retain(|waiter| waiter.ticket != ticket);
        }
        slots.occupants.insert(
            slot.clone(),
            Occupant::Job {
                job_id: job_id.to_owned(),
            },
        );
        Ok(())
    }

    pub fn release_job(&self, job_id: &str) {
        let mut slots = self.lock();
        let held: Vec<SlotKey> = slots
            .occupants
            .iter()
            .filter(
                |(_, occupant)| matches!(occupant, Occupant::Job { job_id: id } if id == job_id),
            )
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in held {
            slots.release(&slot, None);
        }
        // A job stopped while it waited leaves the line at once, and whoever
        // it was keeping a free slot from is woken.
        let ticket = format!("job:{job_id}");
        for queue in slots.queues.values_mut() {
            let before = queue.waiters.len();
            queue.waiters.retain(|waiter| waiter.ticket != ticket);
            if queue.waiters.len() != before {
                queue.notify.notify_waiters();
            }
        }
    }

    pub fn occupant(&self, slot: &SlotKey) -> Option<Occupant> {
        self.lock().occupants.get(slot).cloned()
    }

    /// Every slot that is held or has a line, with who waits and for how long.
    pub fn queue_snapshot(&self) -> Vec<QueueView> {
        let now = Instant::now();
        let mut slots = self.lock();
        for queue in slots.queues.values_mut() {
            queue.prune(now);
        }
        let mut keys: Vec<SlotKey> = slots
            .occupants
            .keys()
            .chain(
                slots
                    .queues
                    .iter()
                    .filter(|(_, queue)| !queue.waiters.is_empty())
                    .map(|(slot, _)| slot),
            )
            .cloned()
            .collect();
        keys.sort_by(|a, b| {
            (&a.node_id, &a.runtime_profile).cmp(&(&b.node_id, &b.runtime_profile))
        });
        keys.dedup();
        keys.into_iter()
            .map(|slot| {
                let mut waiting: Vec<(_, WaiterView)> = slots
                    .queues
                    .get(&slot)
                    .map(|queue| {
                        queue
                            .waiters
                            .iter()
                            .map(|waiter| {
                                (
                                    queue.waiter_key(now, waiter),
                                    WaiterView {
                                        class: waiter.class.as_str(),
                                        kind: if waiter.job_id.is_some() {
                                            "job"
                                        } else {
                                            "request"
                                        },
                                        waited_seconds: now
                                            .saturating_duration_since(waiter.enqueued)
                                            .as_secs(),
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                waiting.sort_by_key(|a| a.0);
                QueueView {
                    holder_kind: slots.holder_kind(&slot),
                    node_id: slot.node_id.clone(),
                    runtime_profile: slot.runtime_profile.clone(),
                    waiting: waiting.into_iter().map(|(_, view)| view).collect(),
                }
            })
            .collect()
    }

    pub fn with_idle_node<T>(&self, id: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        // Hold the reservation lock until removal/maintenance is recorded, so a
        // new reservation cannot race the idle check before its ensure is queued.
        let slots = self.lock();
        anyhow::ensure!(
            !slots.occupants.keys().any(|slot| slot.node_id == id),
            "Machine has a current job or inference request; finish or cancel it first"
        );
        operation()
    }

    pub fn snapshot(&self) -> Vec<OccupancyView> {
        self.named_snapshot(&[])
    }

    pub fn named_snapshot(&self, profiles: &[ModelProfile]) -> Vec<OccupancyView> {
        self.lock()
            .occupants
            .iter()
            .map(|(slot, occupant)| OccupancyView {
                node_id: slot.node_id.clone(),
                runtime_profile: slot.runtime_profile.clone(),
                kind: match occupant {
                    Occupant::Interactive { .. } => "interactive".into(),
                    Occupant::Job { .. } => "job".into(),
                },
                job_id: match occupant {
                    Occupant::Job { job_id } => Some(job_id.clone()),
                    Occupant::Interactive { .. } => None,
                },
                profiles: profiles
                    .iter()
                    .filter(|profile| {
                        profile.node_id.as_deref() == Some(slot.node_id.as_str())
                            && profile.runtime_profile.as_deref()
                                == Some(slot.runtime_profile.as_str())
                    })
                    .flat_map(|profile| {
                        std::iter::once(profile.name.clone()).chain(profile.aliases.iter().cloned())
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn interactive_leases(&self) -> Vec<InteractiveLease> {
        self.lock()
            .occupants
            .iter()
            .filter_map(|(slot, occupant)| match occupant {
                Occupant::Interactive { lease_id, .. } => Some(InteractiveLease {
                    node_id: slot.node_id.clone(),
                    runtime_profile: slot.runtime_profile.clone(),
                    lease_id: lease_id.clone(),
                }),
                Occupant::Job { .. } => None,
            })
            .collect()
    }

    fn release_interactive(&self, slot: &SlotKey, lease_id: &str) {
        let mut slots = self.lock();
        if let Some(Occupant::Interactive {
            lease_id: held,
            session,
        }) = slots.occupants.get(slot)
            && held == lease_id
        {
            let session = session.clone();
            slots.release(slot, session);
        }
    }

    fn leave_line(&self, slot: &SlotKey, ticket: &str) {
        let mut slots = self.lock();
        if let Some(queue) = slots.queues.get_mut(slot) {
            queue.waiters.retain(|waiter| waiter.ticket != ticket);
            // The waiter behind may now be first.
            queue.notify.notify_waiters();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Slots> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A request's place in line while it waits; dropped early, it leaves the line.
struct WaitPlace {
    occupancy: Occupancy,
    slot: SlotKey,
    ticket: Option<String>,
}

impl Drop for WaitPlace {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.occupancy.leave_line(&self.slot, &ticket);
        }
    }
}

impl Drop for InteractiveGuard {
    fn drop(&mut self) {
        self.occupancy
            .release_interactive(&self.slot, &self.lease_id);
    }
}

fn occupied(occupant: &Occupant) -> Occupied {
    match occupant {
        Occupant::Interactive { .. } => Occupied::Interactive,
        Occupant::Job { job_id } => Occupied::Job {
            job_id: job_id.clone(),
        },
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

// Interactive and durable coding both hold their node/runtime slot exclusively.
fn is_coding_profile(profile: &ModelProfile) -> bool {
    profile.uses.contains("interactive") || profile.uses.contains("durable")
}

#[cfg(test)]
#[path = "capacity_tests.rs"]
mod tests;

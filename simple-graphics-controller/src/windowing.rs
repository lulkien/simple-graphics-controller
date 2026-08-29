//! Windowing policy: decides who wins when multiple clients want a resource.
//!
//! Mechanism (revoke handshake, waiter queues, timeouts) lives in the engine;
//! policies only decide WHO wins. Closed enum + impl (not a dyn trait):
//! exhaustive match, no vtable.

use std::{
    collections::{HashMap, VecDeque},
    str::FromStr,
    time::Duration,
};

use simple_graphics_protocol::Resource;
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, timeout_at},
};
use tracing::{debug, info, warn};

use crate::types::ClientId;

/// Lifecycle state of a resource slot, as visible to the policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// No owner; next Acquire is granted immediately.
    Free,
    /// Some client owns the resource.
    Granted,
    /// A Revoke was sent to the owner; awaiting its Release before the
    /// resource frees.
    Revoking,
}

/// Outcome of a policy decision for an Acquire request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireDecision {
    /// Slot is free; grant immediately.
    Grant,
    /// Preempt the current owner (send Revoke) and join the waiters.
    RevokeAndQueue,
    /// A revoke is already in flight; join the waiters.
    Queue,
    /// Hard denial, no waiting. The engine builds the reason string.
    Deny,
}

/// Arbitration policy for a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// First acquirer holds until it releases or disconnects; later Acquires
    /// are denied. Non-preemptive — for kiosk/locked-down resources.
    FirstOwner,
    /// Newest acquirer preempts the current owner; waiters are served
    /// newest-first (LIFO stack). "Last app open gets the screen."
    LatestOwner,
    /// Newest acquirer preempts; waiters are served oldest-first (FIFO
    /// queue). Default — newer apps cannot starve older waiters.
    FairQueue,
}

impl Policy {
    /// What happens to an Acquire given the current slot state.
    pub fn decide(&self, state: SlotState) -> AcquireDecision {
        match (self, state) {
            (_, SlotState::Free) => AcquireDecision::Grant,
            (Policy::FirstOwner, _) => AcquireDecision::Deny,
            (_, SlotState::Granted) => AcquireDecision::RevokeAndQueue,
            (_, SlotState::Revoking) => AcquireDecision::Queue,
        }
    }

    /// Which waiter gets the resource when it frees.
    pub fn pop_waiter<T>(&self, waiters: &mut VecDeque<T>) -> Option<T> {
        match self {
            // Never queues, so never serves waiters (defensive invariant).
            Policy::FirstOwner => None,
            // Newest requester first.
            Policy::LatestOwner => waiters.pop_back(),
            // Oldest requester first.
            Policy::FairQueue => waiters.pop_front(),
        }
    }

    /// Requeue a client that was just revoked (it released after a Revoke),
    /// so it gets one more turn after everyone currently waiting.
    ///
    /// Insertion point is "served last": the far end from where
    /// [`Policy::pop_waiter`] pops. A naive push_back would hand the
    /// resource straight back to the revoked owner under LIFO (LatestOwner),
    /// starving the preemptor that triggered the revoke.
    pub fn requeue_waiter<T>(&self, waiters: &mut VecDeque<T>, waiter: T) {
        match self {
            // Never queues, so never requeues.
            Policy::FirstOwner => {}
            // pop_back serves the back; served last = front.
            Policy::LatestOwner => waiters.push_front(waiter),
            // pop_front serves the front; served last = back.
            Policy::FairQueue => waiters.push_back(waiter),
        }
    }
}

impl FromStr for Policy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "first-owner" => Ok(Policy::FirstOwner),
            "latest-owner" => Ok(Policy::LatestOwner),
            "fair-queue" => Ok(Policy::FairQueue),
            other => Err(anyhow::anyhow!(
                "unknown policy {other:?} (expected first-owner | latest-owner | fair-queue)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine: the mechanism that enforces a Policy
// ---------------------------------------------------------------------------

/// How long the engine waits for a revoked owner to Release before forcing
/// the resource free (the owner is NOT requeued in that case).
pub const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages the engine pushes to a connection task (which owns the stream).
#[derive(Debug, Clone)]
pub enum ControlMessage {
    /// Evict the client: write `ServerMessage::Revoke` on the wire.
    Revoke { resources: Vec<Resource> },
    /// The client is granted (after being queued or requeued): write
    /// `ServerMessage::Grant` with the matching fds on the wire.
    Grant { resources: Vec<Resource> },
}

/// Outcome of an `Acquire` request.
#[derive(Debug)]
pub enum AcquireOutcome {
    /// Granted immediately; the caller sends Grant + fds.
    Granted,
    /// Queued; a Grant will arrive later via the control channel.
    Queued,
    /// Denied; never granted. The reason is human-readable for `Deny`.
    Denied { reason: String },
}

/// A client waiting for a resource.
#[derive(Debug, Clone, Copy)]
struct Waiter {
    client: ClientId,
}

/// Per-resource slot: owner, waiters, and revoke bookkeeping.
struct Slot {
    policy: Policy,
    owner: Option<ClientId>,
    waiters: VecDeque<Waiter>,
    /// Set while a Revoke is in flight (awaiting the owner's Release).
    revoke_deadline: Option<Instant>,
}

impl Slot {
    fn state(&self) -> SlotState {
        match (self.owner, self.revoke_deadline) {
            (None, _) => SlotState::Free,
            (Some(_), None) => SlotState::Granted,
            (Some(_), Some(_)) => SlotState::Revoking,
        }
    }

    /// Is this client already waiting? (dedup guard — no double entries)
    fn is_queued(&self, client: ClientId) -> bool {
        self.waiters.iter().any(|w| w.client == client)
    }

    fn pop_waiter(&mut self) -> Option<Waiter> {
        self.policy.pop_waiter(&mut self.waiters)
    }
}

/// Commands from connection tasks to the engine.
enum EngineCommand {
    Register {
        client: ClientId,
        control: mpsc::UnboundedSender<ControlMessage>,
    },
    Acquire {
        client: ClientId,
        resource: Resource,
        reply: oneshot::Sender<AcquireOutcome>,
    },
    Release {
        client: ClientId,
        resource: Resource,
    },
    Disconnected {
        client: ClientId,
    },
}

/// Handle to the policy engine: a single actor task owning all state.
#[derive(Clone)]
pub struct PolicyEngine {
    tx: mpsc::Sender<EngineCommand>,
}

impl PolicyEngine {
    /// Spawn the engine with one slot per resource in `policies`.
    pub fn spawn(policies: HashMap<Resource, Policy>) -> Self {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move { run_engine(rx, policies).await });
        Self { tx }
    }

    pub async fn register(&self, client: ClientId, control: mpsc::UnboundedSender<ControlMessage>) {
        let _ = self
            .tx
            .send(EngineCommand::Register { client, control })
            .await;
    }

    pub async fn acquire(&self, client: ClientId, resource: Resource) -> AcquireOutcome {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(EngineCommand::Acquire {
                client,
                resource,
                reply,
            })
            .await
            .is_err()
        {
            return AcquireOutcome::Denied {
                reason: "policy engine unavailable".into(),
            };
        }
        rx.await.unwrap_or(AcquireOutcome::Denied {
            reason: "policy engine unavailable".into(),
        })
    }

    pub async fn release(&self, client: ClientId, resource: Resource) {
        let _ = self
            .tx
            .send(EngineCommand::Release { client, resource })
            .await;
    }

    pub async fn disconnected(&self, client: ClientId) {
        let _ = self.tx.send(EngineCommand::Disconnected { client }).await;
    }
}

async fn run_engine(mut rx: mpsc::Receiver<EngineCommand>, policies: HashMap<Resource, Policy>) {
    let mut slots: HashMap<Resource, Slot> = policies
        .into_iter()
        .map(|(resource, policy)| {
            (
                resource,
                Slot {
                    policy,
                    owner: None,
                    waiters: VecDeque::new(),
                    revoke_deadline: None,
                },
            )
        })
        .collect();
    let mut control_reg: HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>> = HashMap::new();

    loop {
        // Sleep until the earliest revoke deadline, or forever if none.
        let deadline = slots.values().filter_map(|slot| slot.revoke_deadline).min();
        let cmd = match deadline {
            Some(d) => timeout_at(d, rx.recv()).await,
            None => Ok(rx.recv().await),
        };

        match cmd {
            Ok(Some(cmd)) => handle_command(cmd, &mut slots, &mut control_reg),
            Ok(None) => break, // all connection tasks gone; engine shuts down
            Err(_) => {
                // Revoke deadline(s) elapsed: force-reclaim silent owners.
                for (resource, slot) in slots.iter_mut() {
                    if let Some(deadline) = slot.revoke_deadline
                        && deadline <= Instant::now()
                    {
                        warn!(
                            "Force-reclaiming {resource:?}: owner {} did not \
                                 Release after Revoke",
                            slot.owner.map(|o| o.to_string()).unwrap_or_default()
                        );
                        force_reclaim(resource, slot, &control_reg);
                    }
                }
            }
        }
    }
}

fn handle_command(
    cmd: EngineCommand,
    slots: &mut HashMap<Resource, Slot>,
    control_reg: &mut HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    match cmd {
        EngineCommand::Register { client, control } => {
            control_reg.insert(client, control);
            debug!("[client {client}] registered with policy engine");
        }
        EngineCommand::Disconnected { client } => {
            control_reg.remove(&client);
            for (resource, slot) in slots.iter_mut() {
                let before = slot.waiters.len();
                slot.waiters.retain(|w| w.client != client);
                if slot.waiters.len() != before {
                    debug!("[client {client}] removed from {resource:?} waiters");
                }
                if slot.owner == Some(client) {
                    info!("[client {client}] disconnected, releasing {resource:?}");
                    slot.owner = None;
                    slot.revoke_deadline = None;
                    grant_next(resource, slot, control_reg);
                }
            }
        }
        EngineCommand::Acquire {
            client,
            resource,
            reply,
        } => {
            let Some(slot) = slots.get_mut(&resource) else {
                let _ = reply.send(AcquireOutcome::Denied {
                    reason: format!("{resource:?} is not registered"),
                });
                return;
            };

            if slot.owner == Some(client) {
                let _ = reply.send(AcquireOutcome::Denied {
                    reason: format!("{resource:?} is already owned by this client"),
                });
                return;
            }

            match slot.policy.decide(slot.state()) {
                AcquireDecision::Grant => {
                    slot.owner = Some(client);
                    info!("[client {client}] acquired {resource:?}");
                    let _ = reply.send(AcquireOutcome::Granted);
                }
                AcquireDecision::Deny => {
                    let reason = match slot.owner {
                        Some(owner) => format!("{resource:?} is owned by client {owner}"),
                        None => format!("{resource:?} is not available"),
                    };
                    warn!("[client {client}] denied {resource:?}: {reason}");
                    let _ = reply.send(AcquireOutcome::Denied { reason });
                }
                AcquireDecision::RevokeAndQueue | AcquireDecision::Queue => {
                    if slot.is_queued(client) {
                        debug!(
                            "[client {client}] already queued for {resource:?}; keeping position"
                        );
                        let _ = reply.send(AcquireOutcome::Queued);
                        return;
                    }
                    slot.waiters.push_back(Waiter { client });
                    debug!(
                        "[client {client}] queued for {resource:?} ({} waiting)",
                        slot.waiters.len()
                    );
                    let _ = reply.send(AcquireOutcome::Queued);

                    // First waiter: start the revoke of the current owner.
                    if slot.revoke_deadline.is_none()
                        && let Some(owner) = slot.owner
                    {
                        if let Some(control) = control_reg.get(&owner) {
                            let _ = control.send(ControlMessage::Revoke {
                                resources: vec![resource.clone()],
                            });
                            slot.revoke_deadline = Some(Instant::now() + REVOKE_TIMEOUT);
                            info!(
                                "[client {client}] preempting client {owner} on \
                                 {resource:?}; Revoke sent"
                            );
                        } else {
                            // Owner vanished without a Disconnected; reclaim.
                            slot.owner = None;
                            grant_next(&resource, slot, control_reg);
                        }
                    }
                }
            }
        }
        EngineCommand::Release { client, resource } => {
            let Some(slot) = slots.get_mut(&resource) else {
                warn!("[client {client}] release of unregistered {resource:?} ignored");
                return;
            };
            if slot.owner != Some(client) {
                warn!("[client {client}] cannot release {resource:?}: not the owner");
                return;
            }

            let revoking = slot.revoke_deadline.is_some();
            slot.owner = None;
            slot.revoke_deadline = None;
            if revoking && !slot.waiters.is_empty() {
                // Clean revoke-ack: the preempted owner gets one more turn.
                slot.policy
                    .requeue_waiter(&mut slot.waiters, Waiter { client });
                info!("[client {client}] released {resource:?} after Revoke; requeued");
            } else {
                // Spontaneous release, or the revoke's preemptor(s) are gone:
                // no requeue — requeueing with an empty queue would regrant
                // the resource straight back to the releaser.
                info!("[client {client}] released {resource:?}");
            }
            grant_next(&resource, slot, control_reg);
        }
    }
}

/// Hand the resource to the next waiter (per the slot's policy).
///
/// The engine does not own streams: it pushes a `Grant` into the winner's
/// control channel; that connection's task writes it (with fds) on the wire.
/// A waiter whose channel is gone (raced disconnect) is dropped and the next
/// one is tried.
fn grant_next(
    resource: &Resource,
    slot: &mut Slot,
    control_reg: &HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    while let Some(waiter) = slot.pop_waiter() {
        match control_reg.get(&waiter.client) {
            Some(control)
                if control
                    .send(ControlMessage::Grant {
                        resources: vec![resource.clone()],
                    })
                    .is_ok() =>
            {
                slot.owner = Some(waiter.client);
                info!("[client {}] granted {resource:?} from queue", waiter.client);
                return;
            }
            Some(_) => {
                warn!(
                    "[client {}] grant delivery failed; dropping waiter",
                    waiter.client
                );
            }
            None => {
                warn!(
                    "[client {}] no control channel; dropping waiter",
                    waiter.client
                );
            }
        }
    }
}

/// Force-free a slot whose owner never confirmed the Revoke. No requeue: a
/// wedged client does not get a queue spot.
fn force_reclaim(
    resource: &Resource,
    slot: &mut Slot,
    control_reg: &HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    slot.owner = None;
    slot.revoke_deadline = None;
    grant_next(resource, slot, control_reg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_slot_grants_for_all_policies() {
        for policy in [Policy::FirstOwner, Policy::LatestOwner, Policy::FairQueue] {
            assert_eq!(policy.decide(SlotState::Free), AcquireDecision::Grant);
        }
    }

    #[test]
    fn first_owner_denies_when_held() {
        let policy = Policy::FirstOwner;
        assert_eq!(policy.decide(SlotState::Granted), AcquireDecision::Deny);
        assert_eq!(policy.decide(SlotState::Revoking), AcquireDecision::Deny);
    }

    #[test]
    fn preemptive_policies_revoke_or_queue() {
        for policy in [Policy::LatestOwner, Policy::FairQueue] {
            assert_eq!(
                policy.decide(SlotState::Granted),
                AcquireDecision::RevokeAndQueue
            );
            assert_eq!(policy.decide(SlotState::Revoking), AcquireDecision::Queue);
        }
    }

    #[test]
    fn latest_owner_serves_newest_waiter_first() {
        let mut waiters: VecDeque<u32> = [10, 20, 30].into_iter().collect();
        assert_eq!(Policy::LatestOwner.pop_waiter(&mut waiters), Some(30));
        assert_eq!(Policy::LatestOwner.pop_waiter(&mut waiters), Some(20));
        assert_eq!(Policy::LatestOwner.pop_waiter(&mut waiters), Some(10));
        assert_eq!(Policy::LatestOwner.pop_waiter(&mut waiters), None);
    }

    #[test]
    fn fair_queue_serves_oldest_waiter_first() {
        let mut waiters: VecDeque<u32> = [10, 20, 30].into_iter().collect();
        assert_eq!(Policy::FairQueue.pop_waiter(&mut waiters), Some(10));
        assert_eq!(Policy::FairQueue.pop_waiter(&mut waiters), Some(20));
        assert_eq!(Policy::FairQueue.pop_waiter(&mut waiters), Some(30));
        assert_eq!(Policy::FairQueue.pop_waiter(&mut waiters), None);
    }

    #[test]
    fn first_owner_never_serves_waiters() {
        let mut waiters: VecDeque<u32> = [10, 20].into_iter().collect();
        assert_eq!(Policy::FirstOwner.pop_waiter(&mut waiters), None);
        assert_eq!(waiters.len(), 2, "waiter list must stay untouched");
    }

    #[test]
    fn requeued_owner_gets_one_more_turn_after_current_waiters() {
        // Both preemptive policies: B (10) queued, then A (99) revoked and
        // requeued -> B served first, then A gets the resource back.
        for policy in [Policy::LatestOwner, Policy::FairQueue] {
            let mut waiters: VecDeque<u32> = [10].into_iter().collect();
            policy.requeue_waiter(&mut waiters, 99);
            assert_eq!(policy.pop_waiter(&mut waiters), Some(10));
            assert_eq!(policy.pop_waiter(&mut waiters), Some(99));
            assert_eq!(policy.pop_waiter(&mut waiters), None);
        }
    }

    #[test]
    fn latest_owner_requeue_does_not_regrant_immediately() {
        // The trap the served-last placement avoids: a naive push_back under
        // LIFO would serve the revoked owner (99) before the preemptor (10).
        let mut waiters: VecDeque<u32> = [10].into_iter().collect();
        Policy::LatestOwner.requeue_waiter(&mut waiters, 99);
        assert_eq!(Policy::LatestOwner.pop_waiter(&mut waiters), Some(10));
    }

    #[test]
    fn first_owner_requeue_is_noop() {
        let mut waiters: VecDeque<u32> = [10].into_iter().collect();
        Policy::FirstOwner.requeue_waiter(&mut waiters, 99);
        assert_eq!(waiters.len(), 1, "FirstOwner never queues");
    }

    #[test]
    fn parse_policy_names() {
        assert_eq!("first-owner".parse::<Policy>().unwrap(), Policy::FirstOwner);
        assert_eq!(
            "latest-owner".parse::<Policy>().unwrap(),
            Policy::LatestOwner
        );
        assert_eq!("fair-queue".parse::<Policy>().unwrap(), Policy::FairQueue);
        assert!("kebab".parse::<Policy>().is_err());
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn fbdev() -> Resource {
        Resource::Fbdev
    }

    fn cid(n: u64) -> ClientId {
        ClientId::new(n)
    }

    fn engine(policy: Policy) -> PolicyEngine {
        PolicyEngine::spawn(HashMap::from([(fbdev(), policy)]))
    }

    /// Register a fake connection and return its control receiver.
    async fn connect(engine: &PolicyEngine, id: u64) -> mpsc::UnboundedReceiver<ControlMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        engine.register(cid(id), tx).await;
        rx
    }

    /// Wait (real time) for the next control message.
    async fn next_control(rx: &mut mpsc::UnboundedReceiver<ControlMessage>) -> ControlMessage {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected a control message")
            .expect("control channel closed")
    }

    /// Poll for a control message without real-time timeout (paused time).
    async fn poll_control(rx: &mut mpsc::UnboundedReceiver<ControlMessage>) -> ControlMessage {
        for _ in 0..100 {
            if let Ok(msg) = rx.try_recv() {
                return msg;
            }
            tokio::task::yield_now().await;
        }
        panic!("no control message after 100 polls");
    }

    #[tokio::test]
    async fn immediate_grant_when_free() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        // Immediate grants are sent by the task itself — nothing on control.
        assert!(a.try_recv().is_err());
    }

    #[tokio::test]
    async fn preempt_revokes_owner_then_grants_after_release() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Queued
        ));
        // A is told to leave.
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resources } if resources == vec![fbdev()]
        ));
        // A's revoke-ack Release hands the resource to B.
        engine.release(cid(1), fbdev()).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { resources } if resources == vec![fbdev()]
        ));
    }

    #[tokio::test]
    async fn revoked_owner_is_requeued_and_gets_one_more_turn() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // B queued, revoke sent to A
        let _ = next_control(&mut a).await; // Revoke
        engine.release(cid(1), fbdev()).await; // revoke-ack -> A requeued
        let _ = next_control(&mut b).await; // B granted
        engine.release(cid(2), fbdev()).await; // B done -> A gets one more turn
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Grant { .. }
        ));
    }

    #[tokio::test]
    async fn fair_queue_serves_oldest_waiter() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued
        engine.acquire(cid(3), fbdev()).await; // queued
        let _ = next_control(&mut a).await; // Revoke A
        engine.release(cid(1), fbdev()).await;
        // B arrived before C -> B is granted first.
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { .. }
        ));
        assert!(c.try_recv().is_err());
    }

    #[tokio::test]
    async fn latest_owner_serves_newest_waiter() {
        let engine = engine(Policy::LatestOwner);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued
        engine.acquire(cid(3), fbdev()).await; // queued
        let _ = next_control(&mut a).await; // Revoke A
        engine.release(cid(1), fbdev()).await;
        // C arrived after B -> C is granted first (LIFO).
        assert!(matches!(
            next_control(&mut c).await,
            ControlMessage::Grant { .. }
        ));
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn first_owner_denies_new_acquires() {
        let engine = engine(Policy::FirstOwner);
        let _a = connect(&engine, 1).await;
        let _b = connect(&engine, 2).await;

        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn double_acquire_by_owner_is_denied() {
        let engine = engine(Policy::FairQueue);
        let _a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Denied { reason } if reason.contains("already owned")
        ));
    }

    #[tokio::test]
    async fn unregistered_resource_is_denied() {
        let engine = PolicyEngine::spawn(HashMap::new());
        let _a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Denied { reason } if reason.contains("not registered")
        ));
    }

    #[tokio::test]
    async fn disconnect_while_queued_removes_waiter() {
        let engine = engine(Policy::FairQueue);
        let _a = connect(&engine, 1).await;
        let _b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Queued
        ));
        // B gives up and disconnects while queued.
        engine.disconnected(cid(2)).await;
        engine.release(cid(1), fbdev()).await;
        // No waiter left: a fresh Acquire is granted immediately.
        assert!(matches!(
            engine.acquire(cid(3), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(c.try_recv().is_err());
    }

    #[tokio::test]
    async fn owner_disconnect_grants_queued_waiter() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued, revoke sent
        let _ = next_control(&mut a).await; // Revoke
        // A dies instead of releasing.
        engine.disconnected(cid(1)).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_owner_is_force_reclaimed_after_timeout() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued, revoke sent
        let _ = poll_control(&mut a).await; // Revoke

        // A never releases. Advance past the revoke deadline.
        tokio::time::advance(REVOKE_TIMEOUT + Duration::from_secs(1)).await;
        let msg = poll_control(&mut b).await;
        assert!(matches!(msg, ControlMessage::Grant { .. }));
    }
}

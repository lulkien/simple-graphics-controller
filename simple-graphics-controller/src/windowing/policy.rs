//! The arbitration policy: given a slot's state, decide what an Acquire
//! does — grant / preempt-and-queue / queue / deny — and how waiters are
//! served when the resource frees. No mechanism here; the engine enforces.

use std::{
    collections::VecDeque,
    str::FromStr,
};

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

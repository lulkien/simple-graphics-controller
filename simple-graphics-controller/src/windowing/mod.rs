//! Windowing policy: decides who wins when multiple clients want a resource.
//!
//! Mechanism (revoke handshake, waiter queues, timeouts) lives in the engine;
//! policies only decide WHO wins. Closed enum + impl (not a dyn trait):
//! exhaustive match, no vtable.
//!
//! Submodules: [`policy`] (the arbitration policy and its decisions),
//! [`engine`] (the actor that enforces a policy per resource slot).

mod engine;
mod policy;

pub use engine::{AcquireOutcome, ControlMessage, PolicyEngine};
pub use policy::Policy;

// Test-only surface (windowing's engine tests + integration tests).
#[allow(unused_imports)]
pub use engine::REVOKE_TIMEOUT;
#[allow(unused_imports)]
pub use policy::{AcquireDecision, SlotState};

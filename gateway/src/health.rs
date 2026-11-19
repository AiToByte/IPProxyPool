//! Runtime health facts and readiness decision model.
//!
//! The evaluator is pure and has no dependency I/O. Runtime facts are updated by
//! startup/shutdown wiring and bounded local health observers, then consumed by
//! HTTP handlers to return stable, low-cardinality probe responses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared runtime facts for health handlers and startup/shutdown wiring.
///
/// `config_valid` and `required_listener_ready` start false: process creation or
/// listener registration alone must never imply readiness. Dependency policy is
/// provided by `RuntimeFacts` snapshots so this state stays free of I/O.
pub(crate) struct RuntimeHealthState {
    config_valid: AtomicBool,
    required_listener_ready: AtomicBool,
    required_dependency_ready: AtomicBool,
    optional_dependencies_ready: AtomicBool,
    drain_requested: AtomicBool,
    stopped: AtomicBool,
}

impl RuntimeHealthState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            config_valid: AtomicBool::new(false),
            required_listener_ready: AtomicBool::new(false),
            required_dependency_ready: AtomicBool::new(false),
            optional_dependencies_ready: AtomicBool::new(true),
            drain_requested: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        })
    }

    pub(crate) fn set_config_valid(&self, ready: bool) {
        self.config_valid.store(ready, Ordering::Release);
    }

    #[allow(dead_code)]
    pub(crate) fn set_required_listener_ready(&self, ready: bool) {
        self.required_listener_ready.store(ready, Ordering::Release);
    }

    #[allow(dead_code)]
    pub(crate) fn set_required_dependency_ready(&self, ready: bool) {
        self.required_dependency_ready
            .store(ready, Ordering::Release);
    }

    #[allow(dead_code)]
    pub(crate) fn set_optional_dependencies_ready(&self, ready: bool) {
        self.optional_dependencies_ready
            .store(ready, Ordering::Release);
    }

    #[allow(dead_code)]
    pub(crate) fn request_drain(&self) {
        self.drain_requested.store(true, Ordering::Release);
    }

    #[allow(dead_code)]
    pub(crate) fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    pub(crate) fn accepts_new_requests(&self) -> bool {
        !self.drain_requested.load(Ordering::Acquire) && !self.stopped.load(Ordering::Acquire)
    }

    pub(crate) fn evaluate(&self) -> ReadinessDecision {
        evaluate_readiness(RuntimeFacts {
            config_valid: self.config_valid.load(Ordering::Acquire),
            required_listener_ready: self.required_listener_ready.load(Ordering::Acquire),
            required_dependency_ready: self.required_dependency_ready.load(Ordering::Acquire),
            optional_dependencies_ready: self.optional_dependencies_ready.load(Ordering::Acquire),
            drain_requested: self.drain_requested.load(Ordering::Acquire),
            stopped: self.stopped.load(Ordering::Acquire),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimePhase {
    Starting,
    Ready,
    Degraded,
    Draining,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadinessReason {
    Starting,
    Ready,
    OptionalDependencyDegraded,
    RequiredDependencyUnavailable,
    ListenerUnavailable,
    Draining,
    Stopped,
}

impl RuntimePhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        }
    }
}

impl ReadinessReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::OptionalDependencyDegraded => "optional_dependency_degraded",
            Self::RequiredDependencyUnavailable => "required_dependency_unavailable",
            Self::ListenerUnavailable => "listener_unavailable",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeFacts {
    pub(crate) config_valid: bool,
    pub(crate) required_listener_ready: bool,
    pub(crate) required_dependency_ready: bool,
    pub(crate) optional_dependencies_ready: bool,
    pub(crate) drain_requested: bool,
    pub(crate) stopped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadinessDecision {
    pub(crate) phase: RuntimePhase,
    pub(crate) ready: bool,
    pub(crate) reason: ReadinessReason,
}

pub(crate) fn redis_dependency_facts(
    tenant_store_requires_redis: bool,
    redis_available: bool,
) -> (bool, bool) {
    (
        !tenant_store_requires_redis || redis_available,
        redis_available,
    )
}

pub(crate) fn evaluate_readiness(facts: RuntimeFacts) -> ReadinessDecision {
    if facts.stopped {
        return ReadinessDecision {
            phase: RuntimePhase::Stopped,
            ready: false,
            reason: ReadinessReason::Stopped,
        };
    }
    if facts.drain_requested {
        return ReadinessDecision {
            phase: RuntimePhase::Draining,
            ready: false,
            reason: ReadinessReason::Draining,
        };
    }
    if !facts.config_valid {
        return ReadinessDecision {
            phase: RuntimePhase::Starting,
            ready: false,
            reason: ReadinessReason::Starting,
        };
    }
    if !facts.required_listener_ready {
        return ReadinessDecision {
            phase: RuntimePhase::Starting,
            ready: false,
            reason: ReadinessReason::ListenerUnavailable,
        };
    }
    if !facts.required_dependency_ready {
        return ReadinessDecision {
            phase: RuntimePhase::Degraded,
            ready: false,
            reason: ReadinessReason::RequiredDependencyUnavailable,
        };
    }
    if !facts.optional_dependencies_ready {
        return ReadinessDecision {
            phase: RuntimePhase::Degraded,
            ready: true,
            reason: ReadinessReason::OptionalDependencyDegraded,
        };
    }
    ReadinessDecision {
        phase: RuntimePhase::Ready,
        ready: true,
        reason: ReadinessReason::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_facts() -> RuntimeFacts {
        RuntimeFacts {
            config_valid: true,
            required_listener_ready: true,
            required_dependency_ready: true,
            optional_dependencies_ready: true,
            drain_requested: false,
            stopped: false,
        }
    }

    #[test]
    fn starting_is_not_ready_before_config_or_listener() {
        let mut facts = ready_facts();
        facts.config_valid = false;
        assert_eq!(evaluate_readiness(facts).reason, ReadinessReason::Starting);
        facts.config_valid = true;
        facts.required_listener_ready = false;
        assert!(!evaluate_readiness(facts).ready);
        assert_eq!(
            evaluate_readiness(facts).reason,
            ReadinessReason::ListenerUnavailable
        );
    }

    #[test]
    fn required_dependency_failure_is_not_ready() {
        let mut facts = ready_facts();
        facts.required_dependency_ready = false;
        let decision = evaluate_readiness(facts);
        assert_eq!(decision.phase, RuntimePhase::Degraded);
        assert!(!decision.ready);
        assert_eq!(
            decision.reason,
            ReadinessReason::RequiredDependencyUnavailable
        );
    }

    #[test]
    fn optional_dependency_failure_is_degraded_but_ready() {
        let mut facts = ready_facts();
        facts.optional_dependencies_ready = false;
        let decision = evaluate_readiness(facts);
        assert_eq!(decision.phase, RuntimePhase::Degraded);
        assert!(decision.ready);
        assert_eq!(decision.reason, ReadinessReason::OptionalDependencyDegraded);
    }

    #[test]
    fn drain_and_stop_override_all_other_facts() {
        let mut facts = ready_facts();
        facts.drain_requested = true;
        assert_eq!(evaluate_readiness(facts).phase, RuntimePhase::Draining);
        assert!(!evaluate_readiness(facts).ready);
        facts.stopped = true;
        assert_eq!(evaluate_readiness(facts).phase, RuntimePhase::Stopped);
        assert_eq!(evaluate_readiness(facts).reason, ReadinessReason::Stopped);
    }

    #[test]
    fn state_precedence_is_stopped_then_draining_then_startup_then_dependencies() {
        let facts = RuntimeFacts {
            config_valid: false,
            required_listener_ready: false,
            required_dependency_ready: false,
            optional_dependencies_ready: false,
            drain_requested: true,
            stopped: true,
        };
        let stopped = evaluate_readiness(facts);
        assert_eq!(stopped.phase, RuntimePhase::Stopped);
        assert_eq!(stopped.reason, ReadinessReason::Stopped);
        assert!(!stopped.ready);

        let draining = evaluate_readiness(RuntimeFacts {
            stopped: false,
            ..facts
        });
        assert_eq!(draining.phase, RuntimePhase::Draining);
        assert_eq!(draining.reason, ReadinessReason::Draining);
        assert!(!draining.ready);

        let listener_blocked = evaluate_readiness(RuntimeFacts {
            stopped: false,
            drain_requested: false,
            config_valid: true,
            required_listener_ready: false,
            required_dependency_ready: false,
            optional_dependencies_ready: false,
        });
        assert_eq!(
            listener_blocked.reason,
            ReadinessReason::ListenerUnavailable
        );
        assert!(!listener_blocked.ready);

        let required_dependency_blocked = evaluate_readiness(RuntimeFacts {
            stopped: false,
            drain_requested: false,
            config_valid: true,
            required_listener_ready: true,
            required_dependency_ready: false,
            optional_dependencies_ready: false,
        });
        assert_eq!(
            required_dependency_blocked.reason,
            ReadinessReason::RequiredDependencyUnavailable
        );
        assert!(!required_dependency_blocked.ready);
    }

    #[test]
    fn shared_state_starts_unready_and_changes_with_lifecycle_facts() {
        let state = RuntimeHealthState::new();
        let starting = state.evaluate();
        assert_eq!(starting.phase, RuntimePhase::Starting);
        assert!(!starting.ready);

        state.set_config_valid(true);
        state.set_required_listener_ready(true);
        state.set_required_dependency_ready(true);
        let ready = state.evaluate();
        assert_eq!(ready.phase, RuntimePhase::Ready);
        assert!(ready.ready);

        state.request_drain();
        let draining = state.evaluate();
        assert_eq!(draining.phase, RuntimePhase::Draining);
        assert!(!draining.ready);
    }

    #[test]
    fn draining_state_closes_new_admission_without_affecting_readiness_facts() {
        let state = RuntimeHealthState::new();
        state.set_config_valid(true);
        state.set_required_listener_ready(true);
        state.set_required_dependency_ready(true);

        assert!(state.accepts_new_requests());
        assert!(state.evaluate().ready);

        state.request_drain();

        assert!(!state.accepts_new_requests());
        assert_eq!(state.evaluate().phase, RuntimePhase::Draining);
        assert!(!state.evaluate().ready);
    }

    #[test]
    fn redis_dependency_policy_separates_required_and_optional_modes() {
        assert_eq!(redis_dependency_facts(true, true), (true, true));
        assert_eq!(redis_dependency_facts(true, false), (false, false));
        assert_eq!(redis_dependency_facts(false, true), (true, true));
        assert_eq!(redis_dependency_facts(false, false), (true, false));
    }

    #[test]
    fn all_ready_facts_produce_ready() {
        let decision = evaluate_readiness(ready_facts());
        assert_eq!(decision.phase, RuntimePhase::Ready);
        assert!(decision.ready);
        assert_eq!(decision.reason, ReadinessReason::Ready);
    }
}

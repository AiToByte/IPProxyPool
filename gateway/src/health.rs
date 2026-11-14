//! Runtime health state and readiness decision model.
//!
//! This module intentionally contains only pure state/decision logic. Listener
//! ownership, dependency probes, and HTTP routing are wired in a later slice so
//! readiness cannot accidentally become a synonym for process startup.

#![cfg_attr(not(test), allow(dead_code))]

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
    fn all_ready_facts_produce_ready() {
        let decision = evaluate_readiness(ready_facts());
        assert_eq!(decision.phase, RuntimePhase::Ready);
        assert!(decision.ready);
        assert_eq!(decision.reason, ReadinessReason::Ready);
    }
}

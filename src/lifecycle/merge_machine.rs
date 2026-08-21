use super::{MergeExecutionFailureKind, MergePhase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SquashResumeState {
    NotStarted,
    Prepared,
    Skipped,
    Completed,
    LegacyCompleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IntegrationRecovery {
    Unvalidated,
    Pending,
    Completed,
    StaleSource,
    StaleValidationAncestry,
    StaleTarget,
}

pub(super) fn integration_recovery(
    validated_source: Option<&str>,
    validated_target: Option<&str>,
    source_commit: &str,
    target_commit: &str,
    cleanup_pending: bool,
    validation_ancestry_observed: bool,
) -> IntegrationRecovery {
    let (Some(validated_source), Some(validated_target)) = (validated_source, validated_target)
    else {
        return IntegrationRecovery::Unvalidated;
    };
    if source_commit != validated_source {
        return IntegrationRecovery::StaleSource;
    }
    if target_commit == validated_source {
        return if validation_ancestry_observed {
            IntegrationRecovery::Completed
        } else {
            IntegrationRecovery::StaleValidationAncestry
        };
    }
    if cleanup_pending || target_commit != validated_target {
        return IntegrationRecovery::StaleTarget;
    }
    IntegrationRecovery::Pending
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // Snapshot pins independent journal and policy facts.
pub(super) struct Snapshot {
    pub(super) journaled: bool,
    pub(super) needs_rebase: bool,
    pub(super) squash: SquashResumeState,
    pub(super) cleanup_pending: bool,
    pub(super) integration: IntegrationRecovery,
    pub(super) removes_topic: bool,
    pub(super) stage_all: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Step {
    PersistJournal,
    StageBeforeRebase,
    Rebase,
    StageAfterRebase,
    Squash,
    Validation,
    Integration,
    RecordObservedIntegration,
    Cleanup,
    FinishRetained,
}

impl Step {
    pub(super) const fn phase(self) -> MergePhase {
        match self {
            Self::PersistJournal | Self::StageBeforeRebase => MergePhase::Planned,
            Self::Rebase | Self::StageAfterRebase => MergePhase::Rebase,
            Self::Squash => MergePhase::Squash,
            Self::Validation => MergePhase::Validation,
            Self::Integration | Self::RecordObservedIntegration => MergePhase::Integration,
            Self::Cleanup => MergePhase::Cleanup,
            Self::FinishRetained => MergePhase::Complete,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Failure {
    pub(super) phase: MergePhase,
    pub(super) kind: MergeExecutionFailureKind,
    pub(super) message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StepResult {
    Start,
    Completed(Step),
    Failed(Failure),
}

impl StepResult {
    pub(super) fn failed(
        step: Step,
        kind: MergeExecutionFailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self::Failed(Failure {
            phase: step.phase(),
            kind,
            message: message.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Instruction {
    Run(Step),
    ExitSuccess,
    ExitFailure(Failure),
}

pub(super) fn transition(snapshot: Snapshot, result: StepResult) -> Instruction {
    match result {
        StepResult::Failed(failure) => Instruction::ExitFailure(failure),
        StepResult::Start => initial_instruction(snapshot),
        StepResult::Completed(step) => match step {
            Step::PersistJournal => before_rebase(snapshot),
            Step::StageBeforeRebase => {
                if snapshot.needs_rebase {
                    Instruction::Run(Step::Rebase)
                } else {
                    after_rebase(snapshot)
                }
            }
            Step::Rebase => {
                if should_stage(snapshot) {
                    Instruction::Run(Step::StageAfterRebase)
                } else {
                    after_rebase(snapshot)
                }
            }
            Step::StageAfterRebase => after_rebase(snapshot),
            Step::Squash => Instruction::Run(Step::Validation),
            Step::Validation => Instruction::Run(Step::Integration),
            Step::Integration | Step::RecordObservedIntegration => after_integration(snapshot),
            Step::Cleanup | Step::FinishRetained => Instruction::ExitSuccess,
        },
    }
}

fn initial_instruction(snapshot: Snapshot) -> Instruction {
    match snapshot.integration {
        IntegrationRecovery::Completed => {
            return Instruction::Run(Step::RecordObservedIntegration);
        }
        IntegrationRecovery::StaleSource => {
            return stale_recovery(
                "the journaled source changed after validation; restore it or reconcile the lifecycle journal",
            );
        }
        IntegrationRecovery::StaleValidationAncestry => {
            return stale_recovery("the journaled validation ancestry is no longer observable");
        }
        IntegrationRecovery::StaleTarget => {
            return stale_recovery(
                "the target changed after journaled validation; reconcile it before retrying",
            );
        }
        IntegrationRecovery::Unvalidated | IntegrationRecovery::Pending => {}
    }
    if snapshot.cleanup_pending {
        return after_integration(snapshot);
    }
    if !snapshot.journaled {
        return Instruction::Run(Step::PersistJournal);
    }
    before_rebase(snapshot)
}

fn stale_recovery(message: &str) -> Instruction {
    Instruction::ExitFailure(Failure {
        phase: MergePhase::Planned,
        kind: MergeExecutionFailureKind::StalePlan,
        message: message.into(),
    })
}

fn before_rebase(snapshot: Snapshot) -> Instruction {
    if should_stage(snapshot) {
        Instruction::Run(Step::StageBeforeRebase)
    } else if snapshot.needs_rebase {
        Instruction::Run(Step::Rebase)
    } else {
        after_rebase(snapshot)
    }
}

fn after_rebase(snapshot: Snapshot) -> Instruction {
    match snapshot.squash {
        SquashResumeState::NotStarted | SquashResumeState::Prepared => {
            Instruction::Run(Step::Squash)
        }
        SquashResumeState::Skipped
        | SquashResumeState::Completed
        | SquashResumeState::LegacyCompleted => Instruction::Run(Step::Validation),
    }
}

fn after_integration(snapshot: Snapshot) -> Instruction {
    if snapshot.removes_topic {
        Instruction::Run(Step::Cleanup)
    } else {
        Instruction::Run(Step::FinishRetained)
    }
}

fn should_stage(snapshot: Snapshot) -> bool {
    snapshot.stage_all && matches!(snapshot.squash, SquashResumeState::NotStarted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            journaled: false,
            needs_rebase: true,
            squash: SquashResumeState::NotStarted,
            cleanup_pending: false,
            integration: IntegrationRecovery::Unvalidated,
            removes_topic: true,
            stage_all: false,
        }
    }

    #[test]
    fn fresh_happy_path_crosses_every_merge_phase() {
        let state = snapshot();
        let expected = [
            Step::PersistJournal,
            Step::Rebase,
            Step::Squash,
            Step::Validation,
            Step::Integration,
            Step::Cleanup,
        ];
        let mut result = StepResult::Start;
        for step in expected {
            assert_eq!(transition(state, result), Instruction::Run(step));
            result = StepResult::Completed(step);
        }
        assert_eq!(transition(state, result), Instruction::ExitSuccess);
    }

    #[test]
    fn every_persisted_squash_state_has_a_resume_instruction() {
        let expected = [
            (SquashResumeState::NotStarted, Step::Squash),
            (SquashResumeState::Prepared, Step::Squash),
            (SquashResumeState::Skipped, Step::Validation),
            (SquashResumeState::Completed, Step::Validation),
            (SquashResumeState::LegacyCompleted, Step::Validation),
        ];
        for (squash, step) in expected {
            let state = Snapshot {
                journaled: true,
                needs_rebase: false,
                squash,
                ..snapshot()
            };
            assert_eq!(transition(state, StepResult::Start), Instruction::Run(step));
        }
    }

    #[test]
    fn interrupted_rebase_and_cleanup_resume_from_their_persisted_phase() {
        let rebase = Snapshot {
            journaled: true,
            needs_rebase: true,
            squash: SquashResumeState::NotStarted,
            ..snapshot()
        };
        assert_eq!(
            transition(rebase, StepResult::Start),
            Instruction::Run(Step::Rebase)
        );

        let cleanup = Snapshot {
            journaled: true,
            needs_rebase: false,
            squash: SquashResumeState::Completed,
            cleanup_pending: true,
            ..snapshot()
        };
        assert_eq!(
            transition(cleanup, StepResult::Start),
            Instruction::Run(Step::Cleanup)
        );
    }

    #[test]
    fn completed_integration_requires_exact_target_equality_and_ancestry() {
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "candidate",
                "candidate",
                false,
                true,
            ),
            IntegrationRecovery::Completed
        );
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "candidate",
                "candidate",
                false,
                false,
            ),
            IntegrationRecovery::StaleValidationAncestry
        );
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "different-source",
                "candidate",
                false,
                true,
            ),
            IntegrationRecovery::StaleSource
        );
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "candidate",
                "different-target",
                false,
                true,
            ),
            IntegrationRecovery::StaleTarget
        );
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "candidate",
                "target-before",
                false,
                true,
            ),
            IntegrationRecovery::Pending
        );
        assert_eq!(
            integration_recovery(
                Some("candidate"),
                Some("target-before"),
                "candidate",
                "target-before",
                true,
                true,
            ),
            IntegrationRecovery::StaleTarget
        );
    }

    #[test]
    fn integration_recovery_states_are_transition_instructions() {
        let completed = Snapshot {
            journaled: true,
            needs_rebase: false,
            squash: SquashResumeState::Completed,
            integration: IntegrationRecovery::Completed,
            ..snapshot()
        };
        assert_eq!(
            transition(completed, StepResult::Start),
            Instruction::Run(Step::RecordObservedIntegration)
        );

        for (integration, message) in [
            (
                IntegrationRecovery::StaleSource,
                "the journaled source changed after validation; restore it or reconcile the lifecycle journal",
            ),
            (
                IntegrationRecovery::StaleValidationAncestry,
                "the journaled validation ancestry is no longer observable",
            ),
            (
                IntegrationRecovery::StaleTarget,
                "the target changed after journaled validation; reconcile it before retrying",
            ),
        ] {
            let stale = Snapshot {
                journaled: true,
                integration,
                ..snapshot()
            };
            assert_eq!(
                transition(stale, StepResult::Start),
                Instruction::ExitFailure(Failure {
                    phase: MergePhase::Planned,
                    kind: MergeExecutionFailureKind::StalePlan,
                    message: message.into(),
                })
            );
        }
    }

    #[test]
    fn phase_failures_exit_without_advancing() {
        for step in [
            Step::PersistJournal,
            Step::Rebase,
            Step::Squash,
            Step::Validation,
            Step::Integration,
            Step::Cleanup,
            Step::FinishRetained,
        ] {
            let result = StepResult::failed(
                step,
                MergeExecutionFailureKind::StalePlan,
                "reported failure",
            );
            assert_eq!(
                transition(snapshot(), result.clone()),
                Instruction::ExitFailure(Failure {
                    phase: step.phase(),
                    kind: MergeExecutionFailureKind::StalePlan,
                    message: "reported failure".into(),
                })
            );
        }
    }
}

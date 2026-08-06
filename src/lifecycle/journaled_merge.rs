use anyhow::{Result, bail};

use crate::{config::HookPhase, git::LifecycleOutput, hook_approval, render, ui};

use super::{
    MergeError, MergeInput, MergeOutcome, MergeOutcomeContext, MergePlan, MergeResult,
    merge_approval_context, merge_preflight_outcome, merge_trust_recovery, plan_merge,
    run_prepared_merge,
};

/// Policy for local changes at the journaled merge boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChangePolicy {
    RequireClean,
    IncludeAll,
}

/// Read-only request accepted by the journaled merge preparation seam.
pub(crate) struct MergeRequest {
    input: MergeInput,
    changes: ChangePolicy,
}

impl MergeRequest {
    pub(crate) fn ordinary(input: &MergeInput) -> Self {
        Self {
            input: input.clone(),
            changes: ChangePolicy::RequireClean,
        }
    }

    pub(crate) fn include_all(input: &MergeInput) -> Self {
        Self {
            input: input.clone(),
            changes: ChangePolicy::IncludeAll,
        }
    }
}

/// The one ordered requirement currently suspending merge execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalRequirement {
    SquashGenerator,
    Hook(HookPhase),
}

enum PendingRequirement {
    SquashGenerator,
    Hook(hook_approval::Candidate),
}

impl PendingRequirement {
    fn exposed(&self) -> ApprovalRequirement {
        match self {
            Self::SquashGenerator => ApprovalRequirement::SquashGenerator,
            Self::Hook(candidate) => ApprovalRequirement::Hook(candidate.phase()),
        }
    }
}

/// Result of read-only merge preparation.
pub(crate) enum Preparation {
    Ready(PreparedMerge),
    ApprovalRequired(PendingApproval),
    Complete(MergeOutcome),
}

/// Opaque approval suspension that can only be resumed through fresh preparation.
pub(crate) struct PendingApproval {
    request: MergeRequest,
    plan: MergePlan,
    requirement: PendingRequirement,
}

impl PendingApproval {
    pub(crate) fn requirement(&self) -> ApprovalRequirement {
        self.requirement.exposed()
    }

    /// Persists a human hook approval without granting mutation authority.
    ///
    /// # Errors
    /// Returns an error when generator approval must use its separate trust command,
    /// the person declines or cancels, or trust persistence fails.
    pub(crate) fn approve_interactively(&self) -> Result<()> {
        match &self.requirement {
            PendingRequirement::SquashGenerator => bail!(
                "shared squash message generator approval is required; run pando trust merge-approve, or rerun with --no-squash"
            ),
            PendingRequirement::Hook(candidate) => {
                hook_approval::approve_candidate_interactively(&self.plan.repository, candidate)
            }
        }
    }

    /// Discards every pre-approval fact and repeats read-only preparation.
    #[must_use]
    pub(crate) fn reprepare(self) -> Preparation {
        prepare(self.request)
    }

    #[must_use]
    pub(crate) fn into_outcome(self) -> MergeOutcome {
        approval_outcome(&self.plan, &self.requirement)
    }
}

/// Opaque, non-cloneable, single-use authority to start journaled mutation.
pub(crate) struct PreparedMerge {
    request: MergeRequest,
    plan: MergePlan,
}

impl PreparedMerge {
    #[must_use]
    pub(crate) fn run(self, observations: &mut Observations) -> MergeOutcome {
        run_prepared_merge(
            &self.plan,
            &self.request.input,
            self.request.changes,
            observations,
        )
    }
}

/// Non-authoritative observations produced while a merge command is in flight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    ProgressStarted {
        starting: String,
        completed: String,
        failed: String,
    },
    ProgressCompleted,
    ProgressFailed,
    GitOutput {
        content: String,
    },
    CommitMessage {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    Human,
    Captured,
}

struct ActiveProgress {
    progress: ui::TimedProgress,
    completed: String,
    failed: String,
}

/// Concrete delivery for merge observations. It never returns data that the
/// executor can use to authorize or classify a command transition.
pub(crate) struct Observations {
    delivery: Delivery,
    events: Vec<Observation>,
    active: Option<ActiveProgress>,
}

impl Observations {
    #[must_use]
    pub(crate) const fn human() -> Self {
        Self {
            delivery: Delivery::Human,
            events: Vec::new(),
            active: None,
        }
    }

    #[must_use]
    pub(crate) const fn captured() -> Self {
        Self {
            delivery: Delivery::Captured,
            events: Vec::new(),
            active: None,
        }
    }

    #[must_use]
    pub(super) const fn is_human(&self) -> bool {
        matches!(self.delivery, Delivery::Human)
    }

    pub(super) fn progress_started(
        &mut self,
        starting: &str,
        completed: &str,
        failed: &str,
    ) -> LifecycleOutput {
        self.emit(Observation::ProgressStarted {
            starting: starting.into(),
            completed: completed.into(),
            failed: failed.into(),
        });
        if !self.is_human()
            || self
                .active
                .as_ref()
                .is_some_and(|active| active.progress.animated())
        {
            LifecycleOutput::Captured
        } else {
            LifecycleOutput::Displayed
        }
    }

    pub(super) fn progress_completed(&mut self) {
        self.emit(Observation::ProgressCompleted);
    }

    pub(super) fn progress_failed(&mut self) {
        self.emit(Observation::ProgressFailed);
    }

    pub(super) fn git_output(&mut self, content: &str, output: LifecycleOutput) {
        if !content.trim().is_empty() {
            self.record(
                Observation::GitOutput {
                    content: content.into(),
                },
                output == LifecycleOutput::Captured,
            );
        }
    }

    pub(super) fn commit_message(&mut self, message: &str) {
        self.emit(Observation::CommitMessage {
            message: message.into(),
        });
    }

    fn emit(&mut self, observation: Observation) {
        self.record(observation, true);
    }

    fn record(&mut self, observation: Observation, render: bool) {
        if self.is_human() && render {
            self.render(&observation);
        }
        self.events.push(observation);
    }

    fn render(&mut self, observation: &Observation) {
        match observation {
            Observation::ProgressStarted {
                starting,
                completed,
                failed,
            } => {
                if self.active.is_none()
                    && let Ok(progress) = ui::TimedProgress::start(true, starting)
                {
                    self.active = Some(ActiveProgress {
                        progress,
                        completed: completed.clone(),
                        failed: failed.clone(),
                    });
                }
            }
            Observation::ProgressCompleted => {
                if let Some(active) = self.active.take() {
                    let _ = active
                        .progress
                        .complete(&active.completed, ui::Completion::Step);
                }
            }
            Observation::ProgressFailed => {
                if let Some(active) = self.active.take() {
                    let _ = active.progress.fail(&active.failed);
                }
            }
            Observation::GitOutput { content } => {
                let _ = ui::step(render::git_output(content.trim_end()));
            }
            Observation::CommitMessage { message } => {
                let _ = ui::step(render::commit_message(message));
            }
        }
    }

    /// Completes infallible, presentation-only observation delivery.
    #[must_use]
    pub(crate) fn finish(mut self) -> Vec<Observation> {
        if let Some(active) = self.active.take() {
            let _ = active.progress.fail(&active.failed);
        }
        self.events
    }
}

/// Performs read-only planning and returns at most one ordered requirement.
#[must_use]
pub(crate) fn prepare(request: MergeRequest) -> Preparation {
    let plan = match plan_merge(request.input.policy(), request.changes) {
        Ok(plan) => plan,
        Err(error) => return Preparation::Complete(merge_preflight_outcome(&error)),
    };
    let requirement = match next_requirement(&plan) {
        Ok(requirement) => requirement,
        Err(error) => {
            return Preparation::Complete(MergeOutcome {
                result: Err(MergeError {
                    code: "trust.read_failed".into(),
                    message: format!("{error:#}"),
                }),
                context: MergeOutcomeContext::Lifecycle(plan.context.clone()),
                effects: plan.effects.clone(),
                diagnostics: Vec::new(),
                recovery: Vec::new(),
                destination: None,
            });
        }
    };
    if request.input.dry_run {
        return Preparation::Complete(dry_run_outcome(&plan, &request.input, requirement.as_ref()));
    }
    match requirement {
        Some(requirement) => Preparation::ApprovalRequired(PendingApproval {
            request,
            plan,
            requirement,
        }),
        None => Preparation::Ready(PreparedMerge { request, plan }),
    }
}

fn next_requirement(plan: &MergePlan) -> Result<Option<PendingRequirement>> {
    if !plan.context.cleanup_pending && plan.squash.approval_required() {
        return Ok(Some(PendingRequirement::SquashGenerator));
    }
    if !plan.context.cleanup_pending {
        if let hook_approval::Evaluation::ApprovalRequired(candidate) = hook_approval::evaluate(
            &plan.repository,
            HookPhase::PreMerge,
            &plan.config.pre_merge,
        )? {
            return Ok(Some(PendingRequirement::Hook(candidate)));
        }
    }
    if plan.context.policy.removes_topic(plan.context.in_place) {
        if let hook_approval::Evaluation::ApprovalRequired(candidate) = hook_approval::evaluate(
            &plan.repository,
            HookPhase::PreRemove,
            &plan.config.pre_remove,
        )? {
            return Ok(Some(PendingRequirement::Hook(candidate)));
        }
    }
    Ok(None)
}

fn outcome_context(
    plan: &MergePlan,
    requirement: Option<&PendingRequirement>,
) -> MergeOutcomeContext {
    match requirement {
        Some(PendingRequirement::Hook(candidate)) => MergeOutcomeContext::Approval {
            lifecycle: plan.context.clone(),
            approval: merge_approval_context(candidate),
        },
        Some(PendingRequirement::SquashGenerator) | None => {
            MergeOutcomeContext::Lifecycle(plan.context.clone())
        }
    }
}

fn dry_run_outcome(
    plan: &MergePlan,
    input: &MergeInput,
    requirement: Option<&PendingRequirement>,
) -> MergeOutcome {
    let approval_required = requirement.is_some();
    MergeOutcome {
        result: Ok(MergeResult::DryRun {
            plan: if plan.context.in_place {
                "in_place"
            } else if input.no_remove {
                "retained_topic"
            } else {
                "cleanup"
            }
            .into(),
            policy: plan.context.policy,
            ready: !approval_required,
            approval_required,
        }),
        context: outcome_context(plan, requirement),
        effects: plan.effects.clone(),
        diagnostics: Vec::new(),
        recovery: requirement.map_or_else(Vec::new, |requirement| {
            vec![merge_trust_recovery(
                plan,
                matches!(requirement, PendingRequirement::SquashGenerator),
            )]
        }),
        destination: None,
    }
}

fn approval_outcome(plan: &MergePlan, requirement: &PendingRequirement) -> MergeOutcome {
    let squash_blocked = matches!(requirement, PendingRequirement::SquashGenerator);
    MergeOutcome {
        result: Err(MergeError {
            code: if squash_blocked {
                "merge.squash_approval_required"
            } else {
                "merge.hook_approval_required"
            }
            .into(),
            message: if squash_blocked {
                "the shared squash message generator is not trusted"
            } else {
                "configured lifecycle hooks are not trusted"
            }
            .into(),
        }),
        context: outcome_context(plan, Some(requirement)),
        effects: plan.effects.clone(),
        diagnostics: Vec::new(),
        recovery: vec![merge_trust_recovery(plan, squash_blocked)],
        destination: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_observations_replay_without_command_authority() {
        let mut delivered = Observations::captured();
        delivered.progress_started("starting", "completed", "failed");
        delivered.progress_completed();
        delivered.git_output("git output", LifecycleOutput::Captured);
        delivered.commit_message("subject");
        let events = delivered.finish();

        let mut replayed = Observations::captured();
        for event in &events {
            replayed.emit(event.clone());
        }
        assert_eq!(replayed.finish(), events);
    }
}

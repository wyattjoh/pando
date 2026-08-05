use std::path::Path;

use anyhow::{Result, bail};

use crate::{
    config::HookPhase,
    git::{self, LifecycleOutput},
    hook, hook_approval, render, ui,
};

use super::{
    MergeDiagnostic, MergeError, MergeInput, MergeOutcome, MergeOutcomeContext, MergePlan,
    MergeResult, merge_approval_context, merge_preflight_outcome, merge_trust_recovery, plan_merge,
    push_captured_merge_diagnostic, push_merge_diagnostic, run_prepared_merge, write_destination,
};

/// Read-only request accepted by the journaled merge preparation seam.
pub(crate) struct MergeRequest {
    input: MergeInput,
}

impl MergeRequest {
    pub(crate) fn ordinary(input: &MergeInput) -> Self {
        Self {
            input: input.clone(),
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
    pub(crate) fn run(self, output: &MergeExecutionOutput) -> MergeOutcome {
        run_prepared_merge(&self.plan, &self.request.input, output)
    }
}

/// Closed presentation choices for the two ordinary merge adapters.
pub(crate) enum MergeExecutionOutput {
    Human,
    Captured,
}

impl MergeExecutionOutput {
    pub(super) fn run_git(
        &self,
        start: &str,
        success: &str,
        failure: &str,
        operation: impl FnOnce(LifecycleOutput) -> Result<String>,
    ) -> Result<String> {
        match self {
            Self::Human => ui::run_timed(true, start, success, failure, |animated| {
                operation(super::output_for(animated))
            })
            .and_then(|transcript| {
                super::report(&transcript)?;
                Ok(transcript)
            }),
            Self::Captured => operation(LifecycleOutput::Captured),
        }
    }

    pub(super) fn run_action<T>(
        &self,
        start: &str,
        success: &str,
        failure: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Human => ui::run_timed(true, start, success, failure, |_| operation()),
            Self::Captured => operation(),
        }
    }

    pub(super) fn present_commit_message(
        &self,
        diagnostics: &mut Vec<MergeDiagnostic>,
        message: &str,
    ) -> Result<()> {
        match self {
            Self::Human => ui::step(render::commit_message(message)),
            Self::Captured => {
                push_merge_diagnostic(diagnostics, "squash", "stderr", message.as_bytes());
                Ok(())
            }
        }
    }

    pub(super) fn record_transcript(
        &self,
        diagnostics: &mut Vec<MergeDiagnostic>,
        phase: &'static str,
        transcript: &str,
    ) {
        if matches!(self, Self::Captured) {
            push_merge_diagnostic(diagnostics, phase, "stderr", transcript.as_bytes());
        }
    }

    pub(super) fn record_error(
        &self,
        diagnostics: &mut Vec<MergeDiagnostic>,
        phase: &'static str,
        error: &impl std::fmt::Display,
    ) {
        if matches!(self, Self::Captured) {
            push_merge_diagnostic(diagnostics, phase, "stderr", error.to_string().as_bytes());
        }
    }

    pub(super) const fn hook_policy(&self) -> hook::OutputPolicy {
        match self {
            Self::Human => hook::OutputPolicy::Streamed,
            Self::Captured => hook::OutputPolicy::Captured,
        }
    }

    pub(super) fn record_hook_output(
        &self,
        diagnostics: &mut Vec<MergeDiagnostic>,
        phase: &'static str,
        output: hook::HookOutput,
    ) {
        if !matches!(self, Self::Captured) {
            return;
        }
        let hook::HookOutput::Captured(output) = output else {
            return;
        };
        for step in output {
            push_captured_merge_diagnostic(diagnostics, phase, "stdout", step.stdout);
            push_captured_merge_diagnostic(diagnostics, phase, "stderr", step.stderr);
        }
    }

    pub(super) const fn removal_output(&self) -> git::RemovalOutput {
        match self {
            Self::Human => git::RemovalOutput::Displayed,
            Self::Captured => git::RemovalOutput::Captured,
        }
    }

    pub(super) fn finish_removal(
        &self,
        output: Option<git::RemovalDiagnostics>,
        diagnostics: &mut Vec<MergeDiagnostic>,
    ) -> Result<()> {
        match self {
            Self::Human => Ok(()),
            Self::Captured => {
                let output = output.expect("captured removal returns diagnostics");
                push_merge_diagnostic(diagnostics, "cleanup", "stdout", &output.stdout);
                push_merge_diagnostic(diagnostics, "cleanup", "stderr", &output.stderr);
                if output.status.success() {
                    Ok(())
                } else {
                    bail!("git worktree remove failed")
                }
            }
        }
    }

    pub(super) fn record_removal_error(
        &self,
        diagnostics: &mut Vec<MergeDiagnostic>,
        error: &impl std::fmt::Display,
    ) {
        if matches!(self, Self::Human) {
            push_merge_diagnostic(
                diagnostics,
                "cleanup",
                "stderr",
                error.to_string().as_bytes(),
            );
        }
    }

    pub(super) fn write_destination(&self, destination: &Path) -> Result<()> {
        if matches!(self, Self::Human) {
            write_destination(destination)?;
        }
        Ok(())
    }
}

/// Performs read-only planning and returns at most one ordered requirement.
#[must_use]
pub(crate) fn prepare(request: MergeRequest) -> Preparation {
    let plan = match plan_merge(request.input.policy()) {
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
    }
}

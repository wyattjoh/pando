use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{hash, hook::HookOutcome, trust, ui};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IncompleteRecord {
    branch: String,
    destination: String,
}

pub(crate) struct Lifecycle<'a> {
    common_dir: &'a Path,
}

#[derive(Clone, Copy)]
pub(crate) struct SetupIntent<'a> {
    pub(crate) branch: &'a str,
    pub(crate) destination: &'a Path,
}

#[derive(Clone, Copy)]
pub(crate) struct SetupTarget<'a> {
    pub(crate) worktree_identity: &'a Path,
    pub(crate) branch: Option<&'a str>,
}

/// Non-authoritative setup observations available to terminal and captured adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    WorktreeCreated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    Human,
    Captured,
}

pub(crate) struct Observations {
    delivery: Delivery,
    events: Vec<Observation>,
}

impl Observations {
    #[must_use]
    pub(crate) const fn human() -> Self {
        Self {
            delivery: Delivery::Human,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) const fn captured() -> Self {
        Self {
            delivery: Delivery::Captured,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) const fn is_human(&self) -> bool {
        matches!(self.delivery, Delivery::Human)
    }

    pub(crate) fn emit(&mut self, observation: Observation) {
        if self.is_human() {
            let _ = match observation {
                Observation::WorktreeCreated => ui::step("Created worktree"),
            };
        }
        self.events.push(observation);
    }

    /// Completes infallible, presentation-only observation delivery.
    #[must_use]
    pub(crate) fn finish(self) -> Vec<Observation> {
        self.events
    }
}

#[derive(Debug)]
pub(crate) struct PendingSetup<'a> {
    common_dir: &'a Path,
    pending_path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct IncompleteSetup<'a> {
    stable_path: PathBuf,
    pending_path: Option<PathBuf>,
    _lifecycle: std::marker::PhantomData<&'a Lifecycle<'a>>,
}

pub(crate) enum Inspection<'a> {
    Complete(Transition),
    Incomplete(IncompleteSetup<'a>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupState {
    NotCreated,
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryDisposition {
    Enter,
    Stay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Transition {
    pub(crate) setup: SetupState,
    pub(crate) entry: EntryDisposition,
}

#[derive(Debug)]
pub(crate) struct TransitionFailure {
    pub(crate) error: anyhow::Error,
    pub(crate) transition: Transition,
}

impl<'a> Lifecycle<'a> {
    pub(crate) const fn new(common_dir: &'a Path) -> Self {
        Self { common_dir }
    }

    pub(crate) fn prepare(
        &'a self,
        intent: SetupIntent<'_>,
    ) -> std::result::Result<PendingSetup<'a>, TransitionFailure> {
        let path = pending_path(self.common_dir, intent.branch);
        let record = IncompleteRecord {
            branch: intent.branch.to_owned(),
            destination: intent.destination.to_string_lossy().into_owned(),
        };
        let result = serde_json::to_vec_pretty(&record)
            .context("failed to encode incomplete setup state")
            .and_then(|bytes| trust::write_atomic(&path, &bytes))
            .with_context(|| {
                format!(
                    "failed to prepare incomplete setup state for {}",
                    intent.destination.display()
                )
            });
        result.map_err(|error| {
            TransitionFailure::new(error, SetupState::NotCreated, EntryDisposition::Stay)
        })?;
        Ok(PendingSetup {
            common_dir: self.common_dir,
            pending_path: path,
        })
    }

    pub(crate) fn inspect(
        &'a self,
        target: SetupTarget<'_>,
    ) -> std::result::Result<Inspection<'a>, TransitionFailure> {
        let stable_path = marker_path(self.common_dir, target.worktree_identity);
        let pending_path = target
            .branch
            .map(|branch| pending_path(self.common_dir, branch));
        let failed =
            |error| TransitionFailure::new(error, SetupState::Incomplete, EntryDisposition::Stay);
        if inspect_record(&stable_path).map_err(failed)? {
            return Ok(Inspection::Incomplete(IncompleteSetup::new(
                stable_path,
                pending_path,
            )));
        }
        if let Some(path) = pending_path.as_ref()
            && inspect_record(path).map_err(failed)?
        {
            return Ok(Inspection::Incomplete(IncompleteSetup::new(
                stable_path,
                pending_path,
            )));
        }
        Ok(Inspection::Complete(Transition::complete()))
    }
}

impl PendingSetup<'_> {
    pub(crate) fn creation_failed(self, source: anyhow::Error) -> TransitionFailure {
        let error = match remove_record(&self.pending_path) {
            Ok(()) => source,
            Err(cancel_error) => cancel_error.context(format!(
                "worktree creation failed ({source:#}) and pending setup state could not be cleared"
            )),
        };
        TransitionFailure::new(error, SetupState::NotCreated, EntryDisposition::Stay)
    }

    pub(crate) fn created(
        self,
        worktree_identity: Result<PathBuf>,
    ) -> std::result::Result<IncompleteSetup<'static>, TransitionFailure> {
        let identity = worktree_identity.map_err(|error| {
            TransitionFailure::new(error, SetupState::Incomplete, EntryDisposition::Enter)
        })?;
        let stable_path = marker_path(self.common_dir, &identity);
        let result = stable_path
            .parent()
            .context("incomplete setup marker has no parent directory")
            .and_then(|parent| {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))
            })
            .and_then(|()| {
                fs::rename(&self.pending_path, &stable_path).with_context(|| {
                    format!(
                        "failed to promote incomplete setup state from {} to {}",
                        self.pending_path.display(),
                        stable_path.display()
                    )
                })
            });
        result.map_err(|error| {
            TransitionFailure::new(error, SetupState::Incomplete, EntryDisposition::Enter)
        })?;
        Ok(IncompleteSetup::new(stable_path, Some(self.pending_path)))
    }
}

impl IncompleteSetup<'_> {
    fn new(stable_path: PathBuf, pending_path: Option<PathBuf>) -> Self {
        Self {
            stable_path,
            pending_path,
            _lifecycle: std::marker::PhantomData,
        }
    }

    pub(crate) fn post_creation_failed(self, error: anyhow::Error) -> TransitionFailure {
        drop(self.stable_path);
        TransitionFailure::new(error, SetupState::Incomplete, EntryDisposition::Enter)
    }

    #[must_use]
    pub(crate) fn enter_once(self) -> Transition {
        drop(self.stable_path);
        Transition::incomplete(EntryDisposition::Enter)
    }

    pub(crate) fn initial_attempt(
        self,
        attempt: Result<HookOutcome>,
    ) -> std::result::Result<Transition, TransitionFailure> {
        self.attempt(attempt, true)
    }

    pub(crate) fn recovery_attempt(
        self,
        attempt: Result<HookOutcome>,
    ) -> std::result::Result<Transition, TransitionFailure> {
        self.attempt(attempt, false)
    }

    pub(crate) fn mark_complete(self) -> std::result::Result<Transition, TransitionFailure> {
        self.clear(EntryDisposition::Stay)
    }

    pub(crate) fn no_hooks_configured(self) -> std::result::Result<Transition, TransitionFailure> {
        self.clear(EntryDisposition::Stay)
    }

    fn attempt(
        self,
        attempt: Result<HookOutcome>,
        initial: bool,
    ) -> std::result::Result<Transition, TransitionFailure> {
        match attempt {
            Err(error) => Err(TransitionFailure::new(
                error,
                SetupState::Incomplete,
                if initial {
                    EntryDisposition::Enter
                } else {
                    EntryDisposition::Stay
                },
            )),
            Ok(HookOutcome::Failed(status)) => Err(TransitionFailure::new(
                anyhow::anyhow!(
                    "post-create setup failed with status {status}; setup remains incomplete"
                ),
                SetupState::Incomplete,
                EntryDisposition::Enter,
            )),
            Ok(HookOutcome::Interrupted) => Err(TransitionFailure::new(
                anyhow::anyhow!("post-create setup was interrupted; setup remains incomplete"),
                SetupState::Incomplete,
                EntryDisposition::Stay,
            )),
            Ok(HookOutcome::Success) => self.clear(if initial {
                EntryDisposition::Enter
            } else {
                EntryDisposition::Stay
            }),
        }
    }

    fn clear(
        self,
        failure_entry: EntryDisposition,
    ) -> std::result::Result<Transition, TransitionFailure> {
        remove_record(&self.stable_path)
            .and_then(|()| self.pending_path.as_deref().map_or(Ok(()), remove_record))
            .map_err(|error| {
                TransitionFailure::new(error, SetupState::Incomplete, failure_entry)
            })?;
        Ok(Transition::complete())
    }
}

impl Transition {
    const fn complete() -> Self {
        Self {
            setup: SetupState::Complete,
            entry: EntryDisposition::Enter,
        }
    }

    const fn incomplete(entry: EntryDisposition) -> Self {
        Self {
            setup: SetupState::Incomplete,
            entry,
        }
    }
}

impl TransitionFailure {
    fn new(error: anyhow::Error, setup: SetupState, entry: EntryDisposition) -> Self {
        Self {
            error,
            transition: Transition { setup, entry },
        }
    }
}

fn marker_path(common_dir: &Path, worktree_identity: &Path) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(worktree_identity.as_os_str().as_encoded_bytes());
    common_dir
        .join("pando-state/incomplete")
        .join(format!("{}.json", hash::encode_hex(&digest.finalize())))
}

fn pending_path(common_dir: &Path, branch: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(branch.as_bytes());
    common_dir
        .join("pando-state/pending")
        .join(format!("{}.json", hash::encode_hex(&digest.finalize())))
}

fn inspect_record(path: &Path) -> Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read incomplete setup record {}", path.display())
            });
        }
    };
    serde_json::from_slice::<IncompleteRecord>(&bytes)
        .with_context(|| format!("failed to parse incomplete setup record {}", path.display()))?;
    Ok(true)
}

fn remove_record(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove incomplete setup record {}",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    const BRANCH: &str = "topic";
    const IDENTITY: &str = "identity";

    fn prepare<'a>(lifecycle: &'a Lifecycle<'a>) -> Result<PendingSetup<'a>> {
        lifecycle
            .prepare(SetupIntent {
                branch: BRANCH,
                destination: Path::new("dest"),
            })
            .map_err(|failure| failure.error)
    }

    fn promote(lifecycle: &Lifecycle<'_>) -> Result<IncompleteSetup<'static>> {
        prepare(lifecycle)?
            .created(Ok(PathBuf::from(IDENTITY)))
            .map_err(|failure| failure.error)
    }

    fn inspect_incomplete<'a>(lifecycle: &'a Lifecycle<'a>) -> Result<IncompleteSetup<'a>> {
        match lifecycle
            .inspect(SetupTarget {
                worktree_identity: Path::new(IDENTITY),
                branch: Some(BRANCH),
            })
            .map_err(|failure| failure.error)?
        {
            Inspection::Incomplete(incomplete) => Ok(incomplete),
            Inspection::Complete(_) => anyhow::bail!("expected incomplete setup"),
        }
    }

    fn assert_failure(failure: &TransitionFailure, setup: SetupState, entry: EntryDisposition) {
        assert_eq!(failure.transition, Transition { setup, entry });
    }

    fn assert_complete(lifecycle: &Lifecycle<'_>) -> Result<()> {
        let Inspection::Complete(transition) = lifecycle
            .inspect(SetupTarget {
                worktree_identity: Path::new(IDENTITY),
                branch: Some(BRANCH),
            })
            .map_err(|failure| failure.error)?
        else {
            anyhow::bail!("expected complete setup")
        };
        assert_eq!(transition, Transition::complete());
        Ok(())
    }

    fn lifecycle() -> Result<(TempDir, Lifecycle<'static>)> {
        let temp = tempdir()?;
        let path: &'static Path = Box::leak(temp.path().to_path_buf().into_boxed_path());
        Ok((temp, Lifecycle::new(path)))
    }

    #[test]
    fn preparation_failure_has_not_created_stay_policy() -> Result<()> {
        let temp = tempdir()?;
        let common_dir = temp.path().join("not-a-directory");
        fs::write(&common_dir, b"file")?;
        let failure = Lifecycle::new(&common_dir)
            .prepare(SetupIntent {
                branch: BRANCH,
                destination: Path::new("dest"),
            })
            .unwrap_err();
        assert_failure(&failure, SetupState::NotCreated, EntryDisposition::Stay);
        assert!(failure.error.to_string().contains("failed to prepare"));
        Ok(())
    }

    #[test]
    fn creation_failure_cancels_pending() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        let failure = prepare(&lifecycle)?.creation_failed(anyhow::anyhow!("creation failed"));
        assert_failure(&failure, SetupState::NotCreated, EntryDisposition::Stay);
        assert!(failure.error.to_string().contains("creation failed"));
        assert_complete(&lifecycle)
    }

    #[test]
    fn cancellation_failure_preserves_detectable_state_and_both_errors() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        let pending = prepare(&lifecycle)?;
        fs::remove_file(&pending.pending_path)?;
        fs::create_dir(&pending.pending_path)?;
        let failure = pending.creation_failed(anyhow::anyhow!("creation exploded"));
        assert_failure(&failure, SetupState::NotCreated, EntryDisposition::Stay);
        let message = format!("{:#}", failure.error);
        assert!(message.contains("creation exploded"));
        assert!(message.contains("failed to remove incomplete setup record"));
        let inspection = lifecycle.inspect(SetupTarget {
            worktree_identity: Path::new(IDENTITY),
            branch: Some(BRANCH),
        });
        let failure = inspection.err().expect("directory record must be detected");
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Stay);
        Ok(())
    }

    #[test]
    fn identity_and_promotion_failures_retain_pending_fallback() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        let failure = prepare(&lifecycle)?
            .created(Err(anyhow::anyhow!("identity failed")))
            .unwrap_err();
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Enter);
        assert!(failure.error.to_string().contains("identity failed"));
        drop(inspect_incomplete(&lifecycle)?);

        let stable = marker_path(lifecycle.common_dir, Path::new(IDENTITY));
        fs::create_dir_all(&stable)?;
        let failure = prepare(&lifecycle)?
            .created(Ok(PathBuf::from(IDENTITY)))
            .unwrap_err();
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Enter);
        assert!(format!("{:#}", failure.error).contains("failed to promote"));
        assert!(pending_path(lifecycle.common_dir, BRANCH).is_file());
        Ok(())
    }

    #[test]
    fn inspection_precedence_payload_compatibility_and_malformed_records() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        promote(&lifecycle)?;
        let pending = pending_path(lifecycle.common_dir, BRANCH);
        fs::create_dir_all(pending.parent().expect("pending parent"))?;
        fs::write(&pending, b"malformed")?;
        // A valid stable record wins without consulting a malformed pending fallback.
        drop(inspect_incomplete(&lifecycle)?);

        fs::remove_file(marker_path(lifecycle.common_dir, Path::new(IDENTITY)))?;
        let failure = lifecycle
            .inspect(SetupTarget {
                worktree_identity: Path::new(IDENTITY),
                branch: Some(BRANCH),
            })
            .err()
            .expect("malformed state must fail closed");
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Stay);
        assert!(format!("{:#}", failure.error).contains("failed to parse"));

        fs::write(
            &pending,
            serde_json::to_vec(&IncompleteRecord {
                branch: "stored-other-branch".into(),
                destination: "stored-other-destination".into(),
            })?,
        )?;
        // Stored payloads establish schema compatibility, not recovery identity.
        drop(inspect_incomplete(&lifecycle)?);
        Ok(())
    }

    #[test]
    fn observation_delivery_and_replay_cannot_advance_setup_state() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        drop(promote(&lifecycle)?);

        let mut delivered = Observations::captured();
        delivered.emit(Observation::WorktreeCreated);
        let events = delivered.finish();
        let mut replayed = Observations::captured();
        for event in &events {
            replayed.emit(*event);
        }
        assert_eq!(replayed.finish(), events);
        drop(inspect_incomplete(&lifecycle)?);

        inspect_incomplete(&lifecycle)?
            .mark_complete()
            .map_err(|failure| failure.error)?;
        assert_complete(&lifecycle)
    }

    #[test]
    fn dropped_handles_retain_pending_and_stable_state() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        drop(prepare(&lifecycle)?);
        drop(inspect_incomplete(&lifecycle)?);
        let pending = prepare(&lifecycle)?;
        let incomplete = pending
            .created(Ok(PathBuf::from(IDENTITY)))
            .map_err(|f| f.error)?;
        drop(incomplete);
        drop(inspect_incomplete(&lifecycle)?);
        Ok(())
    }

    #[test]
    fn post_creation_and_attempt_failures_preserve_state_with_expected_entry() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        let failure = promote(&lifecycle)?.post_creation_failed(anyhow::anyhow!("announce"));
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Enter);

        let cases = [
            (
                true,
                Err(anyhow::anyhow!("initial io")),
                EntryDisposition::Enter,
            ),
            (
                false,
                Err(anyhow::anyhow!("recovery io")),
                EntryDisposition::Stay,
            ),
            (true, Ok(HookOutcome::Failed(7)), EntryDisposition::Enter),
            (false, Ok(HookOutcome::Failed(8)), EntryDisposition::Enter),
            (true, Ok(HookOutcome::Interrupted), EntryDisposition::Stay),
            (false, Ok(HookOutcome::Interrupted), EntryDisposition::Stay),
        ];
        for (initial, attempt, entry) in cases {
            let incomplete = inspect_incomplete(&lifecycle)?;
            let failure = if initial {
                incomplete.initial_attempt(attempt)
            } else {
                incomplete.recovery_attempt(attempt)
            }
            .unwrap_err();
            assert_failure(&failure, SetupState::Incomplete, entry);
        }
        drop(inspect_incomplete(&lifecycle)?);
        Ok(())
    }

    #[test]
    fn successful_attempts_clear_and_enter_once_retains() -> Result<()> {
        for initial in [true, false] {
            let (_temp, lifecycle) = lifecycle()?;
            let incomplete = promote(&lifecycle)?;
            assert_eq!(
                incomplete.enter_once(),
                Transition::incomplete(EntryDisposition::Enter)
            );
            let incomplete = inspect_incomplete(&lifecycle)?;
            let transition = if initial {
                incomplete.initial_attempt(Ok(HookOutcome::Success))
            } else {
                incomplete.recovery_attempt(Ok(HookOutcome::Success))
            };
            assert_eq!(transition.map_err(|f| f.error)?, Transition::complete());
            assert_complete(&lifecycle)?;
            assert_complete(&lifecycle)?;
        }
        Ok(())
    }

    #[test]
    fn clearing_failure_entry_depends_on_initial_or_recovery_attempt() -> Result<()> {
        for (initial, entry) in [
            (true, EntryDisposition::Enter),
            (false, EntryDisposition::Stay),
        ] {
            let (_temp, lifecycle) = lifecycle()?;
            let incomplete = promote(&lifecycle)?;
            fs::remove_file(&incomplete.stable_path)?;
            fs::create_dir(&incomplete.stable_path)?;
            let failure = if initial {
                incomplete.initial_attempt(Ok(HookOutcome::Success))
            } else {
                incomplete.recovery_attempt(Ok(HookOutcome::Success))
            }
            .unwrap_err();
            assert_failure(&failure, SetupState::Incomplete, entry);
        }
        Ok(())
    }

    #[test]
    fn administrative_completion_commands_clear_or_stay_on_failure() -> Result<()> {
        for mark_complete in [true, false] {
            let (_temp, lifecycle) = lifecycle()?;
            let incomplete = promote(&lifecycle)?;
            let transition = if mark_complete {
                incomplete.mark_complete()
            } else {
                incomplete.no_hooks_configured()
            };
            assert_eq!(transition.map_err(|f| f.error)?, Transition::complete());
            assert_complete(&lifecycle)?;
        }
        for mark_complete in [true, false] {
            let (_temp, lifecycle) = lifecycle()?;
            let incomplete = promote(&lifecycle)?;
            fs::remove_file(&incomplete.stable_path)?;
            fs::create_dir(&incomplete.stable_path)?;
            let failure = if mark_complete {
                incomplete.mark_complete()
            } else {
                incomplete.no_hooks_configured()
            }
            .unwrap_err();
            assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Stay);
        }
        Ok(())
    }

    #[test]
    fn partial_removal_failure_is_detectable_and_retryable() -> Result<()> {
        let (_temp, lifecycle) = lifecycle()?;
        let incomplete = promote(&lifecycle)?;
        let pending = incomplete.pending_path.clone().expect("pending path");
        fs::create_dir_all(&pending)?;
        let failure = incomplete.mark_complete().unwrap_err();
        assert_failure(&failure, SetupState::Incomplete, EntryDisposition::Stay);
        assert!(!marker_path(lifecycle.common_dir, Path::new(IDENTITY)).exists());

        fs::remove_dir(&pending)?;
        fs::write(
            &pending,
            serde_json::to_vec(&IncompleteRecord {
                branch: BRANCH.into(),
                destination: "dest".into(),
            })?,
        )?;
        let retry = inspect_incomplete(&lifecycle)?;
        assert_eq!(
            retry.mark_complete().map_err(|f| f.error)?,
            Transition::complete()
        );
        assert_complete(&lifecycle)
    }
}

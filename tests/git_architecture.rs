use std::{fs, path::Path};

const APPROVED_GIT_PROCESS_MODULES: &[&str] = &["git.rs"];

fn rust_sources(directory: &Path, sources: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory should be readable") {
        let path = entry.expect("source entry should be readable").path();
        if path.is_dir() {
            rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

fn without_whitespace(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn braced_item<'a>(source: &'a str, marker: &str) -> &'a str {
    let start = source
        .find(marker)
        .expect("architecture marker should exist");
    let opening = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .expect("architecture item should have a body");
    let mut depth = 0usize;
    for (offset, character) in source[opening..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[start..=opening + offset];
                }
            }
            _ => {}
        }
    }
    panic!("architecture item should have a complete body")
}

#[test]
fn only_the_git_execution_module_constructs_git_processes() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    rust_sources(&source_root, &mut sources);

    let mut violations = Vec::new();
    for path in sources {
        let relative = path.strip_prefix(&source_root).unwrap();
        if APPROVED_GIT_PROCESS_MODULES
            .iter()
            .any(|approved| relative == Path::new(approved))
        {
            continue;
        }
        let source = fs::read_to_string(&path).expect("Rust source should be UTF-8");
        let production_source = source.split("#[cfg(test)]").next().unwrap_or(&source);
        let compact = without_whitespace(production_source);
        if compact.contains("Command::new(\"git\")")
            || compact.contains("Command::new(OsStr::new(\"git\"))")
        {
            violations.push(relative.display().to_string());
        }
    }

    assert!(
        violations.is_empty(),
        "direct Git process construction belongs in src/git.rs; found in {}",
        violations.join(", ")
    );
}

#[test]
fn hook_execution_stays_separate_from_setup_persistence_and_generators() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hook_source =
        fs::read_to_string(source_root.join("hook.rs")).expect("hook source should be readable");
    let setup_source =
        fs::read_to_string(source_root.join("setup.rs")).expect("setup source should be readable");

    assert!(
        hook_source.contains("HookStep") && hook_source.contains("Command::new(\"/bin/sh\")"),
        "src/hook.rs must own configured hook subprocess execution"
    );
    assert!(
        !setup_source.contains("Command::new") && !setup_source.contains("HookStep"),
        "src/setup.rs must own persistence, not hook subprocess execution"
    );
}

#[test]
fn obsolete_branch_forwarding_interfaces_do_not_return() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let branch = fs::read_to_string(source_root.join("branch.rs")).expect("branch source");
    let git = fs::read_to_string(source_root.join("git.rs")).expect("git source");

    assert!(!branch.contains("struct Resolver"));
    assert!(!git.contains("struct BranchRepository"));
    assert!(branch.contains("struct Snapshot"));
    assert!(git.contains("struct RefMutation"));
}

#[test]
fn all_merge_adapters_only_enter_the_journaled_executor_seam() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lifecycle = fs::read_to_string(source_root.join("lifecycle.rs"))
        .expect("lifecycle source should be readable");
    let journaled = fs::read_to_string(source_root.join("lifecycle/journaled_merge.rs"))
        .expect("journaled merge source should be readable");
    let machine = fs::read_to_string(source_root.join("machine.rs"))
        .expect("machine source should be readable");
    let compact = without_whitespace(&journaled);

    assert!(lifecycle.contains("journaled_merge::prepare"));
    assert!(lifecycle.contains("journaled_merge::MergeRequest::include_all"));
    assert!(lifecycle.contains("prepared.run(&mut observations)"));
    for obsolete in ["fn merge_inner", "enum MergeIntent", "fn cleanup_merge"] {
        assert!(
            !lifecycle.contains(obsolete),
            "the duplicate yolo lifecycle `{obsolete}` must stay deleted"
        );
    }
    assert!(machine.contains("lifecycle::execute_merge_request(&input)"));
    for forbidden in [
        "plan_merge",
        "execute_merge(",
        "hook_approval",
        "MergeJournal",
    ] {
        assert!(
            !machine.contains(forbidden),
            "src/machine.rs must not drive merge internals through `{forbidden}`"
        );
    }
    assert!(compact.contains("enumPreparation{Ready(PreparedMerge),ApprovalRequired(PendingApproval),Complete(MergeOutcome),}"));
    assert!(compact.contains("structPreparedMerge{request:MergeRequest,plan:MergePlan,}"));
    assert!(compact.contains("enumObservation{"));
    assert!(compact.contains("structObservations{"));
    assert!(!compact.contains("pub(crate)request:MergeRequest"));
    assert!(!compact.contains("pub(crate)plan:MergePlan"));
    for obsolete in [
        "MergeExecutionOutput",
        "run_git(",
        "run_action(",
        "write_destination",
        "LifecycleMutation",
        "WorktreeMutation",
        "hook::execute",
    ] {
        assert!(
            !journaled.contains(obsolete),
            "journaled merge observations must not recover execution authority through `{obsolete}`"
        );
    }
    assert!(!journaled.contains("trait "));
}

#[test]
fn observations_cannot_become_outcomes_or_adapter_execution_authority() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hook = fs::read_to_string(source_root.join("hook.rs")).expect("hook source");
    let setup = fs::read_to_string(source_root.join("setup.rs")).expect("setup source");
    let install = fs::read_to_string(source_root.join("install.rs")).expect("install source");
    let git = fs::read_to_string(source_root.join("git.rs")).expect("git source");
    let journaled = fs::read_to_string(source_root.join("lifecycle/journaled_merge.rs"))
        .expect("journaled merge source");
    let worktree =
        fs::read_to_string(source_root.join("worktree_plan.rs")).expect("worktree source");
    let lifecycle = fs::read_to_string(source_root.join("lifecycle.rs")).expect("lifecycle source");
    let machine = fs::read_to_string(source_root.join("machine.rs")).expect("machine source");
    let smart = fs::read_to_string(source_root.join("smart.rs")).expect("smart source");

    for (name, source) in [
        ("hook", hook.as_str()),
        ("setup", setup.as_str()),
        ("install", install.as_str()),
        ("merge", journaled.as_str()),
    ] {
        let observation = braced_item(source, "enum Observation");
        for forbidden in [
            "Effect",
            "Recovery",
            "Outcome",
            "Approval",
            "Transition",
            "ErrorBody",
            "anyhow::Error",
            "destination",
            "effects",
            "recovery",
            "approval",
            "result",
            "error:",
        ] {
            assert!(
                !observation.contains(forbidden),
                "{name} observations must not carry final authority through `{forbidden}`"
            );
        }
    }

    assert!(!hook.contains("enum OutputPolicy"));
    assert!(!hook.contains("enum HookOutput"));
    assert!(!setup.contains("enum Event"));
    assert!(!setup.contains("fn advance("));
    assert!(!journaled.contains("MergeExecutionOutput"));
    assert!(!git.contains("enum RemovalOutput"));

    for (source, outcome) in [
        (&worktree, "struct OperationOutcome"),
        (&lifecycle, "struct MergeOutcome"),
        (&install, "struct InstallOutcome"),
    ] {
        let item = braced_item(source, outcome);
        assert!(item.contains("result"), "{outcome} must own final result");
        assert!(item.contains("effects"), "{outcome} must own effects");
        assert!(item.contains("recovery"), "{outcome} must own recovery");
    }
    assert!(braced_item(&worktree, "struct OperationOutcome").contains("destination"));
    assert!(braced_item(&lifecycle, "struct MergeOutcome").contains("diagnostics"));
    assert!(braced_item(&lifecycle, "struct MergeOutcome").contains("destination"));
    for execution in ["fn execute_merge(", "fn execute_merge_cleanup("] {
        assert!(
            !braced_item(&lifecycle, execution).contains("write_destination"),
            "merge mutation must return destination through its final outcome"
        );
    }
    let human_merge = without_whitespace(braced_item(&lifecycle, "fn finish_human_merge("));
    assert!(human_merge.contains("outcome.destination"));
    assert!(human_merge.contains("write_destination"));

    for forbidden in [
        "OutputPolicy",
        "HookOutput",
        "hook::",
        "setup::",
        "journaled_merge::",
        "MergeExecutionOutput",
        "run_guided_configuration",
        "InstallApproval",
        "execute_planned",
        "mark_effect",
        "write_destination",
    ] {
        assert!(
            !machine.contains(forbidden),
            "the machine adapter must consume final outcomes, not `{forbidden}`"
        );
    }
    assert!(!smart.contains("worktree_plan::execute("));
    assert!(smart.contains("worktree_plan::execute_planned("));
    let install_run = braced_item(&install, "pub fn run(");
    assert!(install_run.contains("let outcome = execute("));
    assert!(install_run.contains("finish_human_install(outcome"));
    assert!(!install_run.contains("run_guided_configuration"));
    assert!(!install.contains("HumanInstallOutcome"));
}

#[test]
fn observation_delivery_is_infallible_and_cannot_change_execution_policy() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hook = fs::read_to_string(source_root.join("hook.rs")).expect("hook source");
    let setup = fs::read_to_string(source_root.join("setup.rs")).expect("setup source");
    let install = fs::read_to_string(source_root.join("install.rs")).expect("install source");
    let journaled = fs::read_to_string(source_root.join("lifecycle/journaled_merge.rs"))
        .expect("journaled merge source");

    for (name, source) in [
        ("hook", hook.as_str()),
        ("setup", setup.as_str()),
        ("install", install.as_str()),
        ("merge", journaled.as_str()),
    ] {
        let observations = without_whitespace(braced_item(source, "struct Observations"));
        let finish = without_whitespace(braced_item(source, "fn finish("));
        assert!(
            !observations.contains("error:"),
            "{name} observation delivery must not retain an authoritative error"
        );
        assert!(
            finish.contains("->Vec<Observation>"),
            "{name} observation completion must be infallible"
        );
        assert!(
            !finish.contains("Result") && !finish.contains("Err("),
            "{name} observation completion must not mask the command outcome"
        );
    }

    let execute = without_whitespace(braced_item(&hook, "pub(crate) fn execute("));
    assert!(execute.contains(".stdin(Stdio::inherit())"));
    assert!(!execute.contains("Stdio::null()"));
    assert!(!execute.contains("observations.is_human()"));
    assert!(execute.contains("capture_child(&mutchild,observations.relay())"));

    let capture = without_whitespace(braced_item(&hook, "fn capture_child("));
    assert!(capture.contains("mpsc::sync_channel"));
    assert!(capture.contains("relay_output(output,&receiver)"));
    let stream = without_whitespace(braced_item(&hook, "fn capture_stream("));
    assert!(!stream.contains("write_all("));
}

#[test]
fn final_outcomes_keep_precedence_and_own_internal_install_effects() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let install = fs::read_to_string(source_root.join("install.rs")).expect("install source");
    let lifecycle = fs::read_to_string(source_root.join("lifecycle.rs")).expect("lifecycle source");
    let smart = fs::read_to_string(source_root.join("smart.rs")).expect("smart source");
    let worktree =
        fs::read_to_string(source_root.join("worktree_plan.rs")).expect("worktree source");

    let install_execute = without_whitespace(braced_item(&install, "fn execute("));
    assert!(
        install_execute
            .contains("run_guided_configuration(&plan.config_path,observations,&muteffects)")
    );
    assert!(install_execute.contains("Err(error)=>returninstall_failure(effects"));
    assert!(install_execute.contains("completion:Some(completion)"));
    let guidance = without_whitespace(braced_item(&install, "fn run_guided_configuration("));
    assert!(guidance.contains("effects:&mutVec<Effect>"));
    assert!(guidance.contains("effects.push(write_effect(write))"));
    assert!(guidance.contains("persist_proposed_command("));
    assert!(
        without_whitespace(&install)
            .contains("pubconstINSTALL_ACTIONS:&[&str]=&[\"file.write\",\"install.approve\"]")
    );
    assert!(!install.contains("HumanInstallOutcome"));

    let install_finish = without_whitespace(braced_item(&install, "fn finish_human_install("));
    assert!(install_finish.contains("ifletErr(failure)=outcome.result"));
    assert!(!install_finish.contains("observations"));

    let merge_finish = without_whitespace(braced_item(&lifecycle, "fn finish_human_merge("));
    let merge_result = merge_finish
        .find("match&outcome.result")
        .expect("human merge must inspect its final result");
    let destination_failure = merge_finish
        .find("destination_delivery?")
        .expect("successful merge must still report destination delivery failure");
    assert!(merge_result < destination_failure);
    assert!(!merge_finish.contains("observation_delivery"));

    assert!(!smart.contains("observation_delivery"));
    assert!(!worktree.contains("record_delivery"));
}

#[test]
fn human_presentation_preserves_git_diagnostics_and_timed_streaming() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lifecycle = fs::read_to_string(source_root.join("lifecycle.rs")).expect("lifecycle source");
    let journaled = fs::read_to_string(source_root.join("lifecycle/journaled_merge.rs"))
        .expect("journaled merge source");
    let git = fs::read_to_string(source_root.join("git.rs")).expect("git source");

    let remove = without_whitespace(braced_item(&lifecycle, "pub fn remove("));
    let render = remove
        .find("render_removal_git_diagnostics(&outcome)")
        .expect("human removal must render captured Git diagnostics");
    let result = remove
        .find("ifletErr(error)=&outcome.result")
        .expect("human removal must use the final result");
    assert!(render < result);

    let merge_git = without_whitespace(braced_item(&lifecycle, "fn observe_merge_git("));
    assert!(
        merge_git.contains("letoutput=observations.progress_started(starting,completed,failed)")
    );
    assert!(merge_git.contains("letresult=operation(output)"));
    assert!(merge_git.contains("observations.git_output(transcript,output)"));

    let progress = without_whitespace(braced_item(&journaled, "fn progress_started("));
    assert!(progress.contains("active.progress.animated()"));
    assert!(progress.contains("LifecycleOutput::Captured"));
    assert!(progress.contains("LifecycleOutput::Displayed"));

    let lifecycle_git = without_whitespace(braced_item(&git, "fn run_lifecycle_git("));
    assert!(
        lifecycle_git.contains("LifecycleOutput::Captured=>process.captured_inheriting_stdin()")
    );
    assert!(lifecycle_git.contains("LifecycleOutput::Displayed=>process.streamed()"));
    assert!(lifecycle_git.contains("lettranscript=combined_output(&execution)"));
}

#[test]
fn git_execution_stays_private_and_concrete() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/git.rs"))
        .expect("Git source should be readable");
    let compact = without_whitespace(&source);

    for forbidden in [
        "pubtrait",
        "pub(crate)trait",
        "pubstructGitProcess",
        "pub(crate)structGitProcess",
        "pubfnrun_git",
        "pub(crate)fnrun_git",
    ] {
        assert!(
            !compact.contains(forbidden),
            "src/git.rs must expose typed concrete capabilities, not `{forbidden}`"
        );
    }
}

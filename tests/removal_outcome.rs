use std::{env, fs, path::Path, process::Command};

use pando::{
    lifecycle::{RemovalTargetStatus, execute_removal, plan_remove},
    protocol::BytePath,
};
use tempfile::TempDir;

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should start");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn typed_removal_plan_and_executor_track_force_targets_and_effects() {
    let root = TempDir::new().unwrap();
    let main = root.path().join("main");
    let topic = root.path().join("topic");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-b", "main"]);
    git(&main, &["config", "user.name", "Pando Test"]);
    git(&main, &["config", "user.email", "pando@example.com"]);
    fs::write(main.join("tracked"), "base\n").unwrap();
    git(&main, &["add", "tracked"]);
    git(&main, &["commit", "-m", "base"]);
    git(
        &main,
        &["worktree", "add", "-b", "topic", topic.to_str().unwrap()],
    );
    fs::write(topic.join("tracked"), "dirty\n").unwrap();

    let original = env::current_dir().unwrap();
    env::set_current_dir(&main).unwrap();
    let failure = plan_remove(&["topic".into()], false).unwrap_err();
    assert!(failure.to_string().contains("--force"));

    let plan = plan_remove(&["topic".into()], true).unwrap();
    assert!(plan.force);
    assert_eq!(plan.context.targets.len(), 1);
    assert!(plan.context.targets[0].branch_retained);
    assert!(plan.context.targets[0].force);
    assert!(plan.context.destination.is_none());
    assert_eq!(plan.effects.len(), 2);
    assert!(
        plan.effects
            .iter()
            .all(|effect| !effect.attempted && !effect.completed)
    );
    assert!(matches!(
        plan.context.primary_worktree,
        BytePath::Utf8 { .. }
    ));

    let outcome = execute_removal(&plan);
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert_eq!(outcome.targets[0].status, RemovalTargetStatus::Completed);
    assert!(outcome.effects[0].completed);
    assert!(!outcome.effects[0].attempted);
    assert!(outcome.effects[1].attempted);
    assert!(outcome.effects[1].completed);
    assert!(!topic.exists());
    git(&main, &["show-ref", "--verify", "refs/heads/topic"]);
    env::set_current_dir(original).unwrap();
}

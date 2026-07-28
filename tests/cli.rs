use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use nix::{
    pty::{Winsize, openpty},
    unistd::dup,
};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

#[cfg(target_os = "linux")]
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
};

struct Repository {
    temp: TempDir,
    main: std::path::PathBuf,
    linked: std::path::PathBuf,
}

impl Repository {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let linked = temp.path().join("feature worktree");
        fs::create_dir(&main).unwrap();
        git(&main, ["init", "-b", "main"]);
        git(&main, ["config", "user.email", "test@example.com"]);
        git(&main, ["config", "user.name", "Test User"]);
        fs::write(main.join("README.md"), "initial\n").unwrap();
        git(&main, ["add", "README.md"]);
        git(&main, ["commit", "-m", "initial"]);
        add_worktree(&main, &linked, "feature");
        Self { temp, main, linked }
    }

    fn add_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.temp.path().join(name);
        add_worktree(&self.main, &path, branch);
        path
    }
}

fn add_worktree(main: &Path, path: &Path, branch: &str) {
    git(
        main,
        ["worktree", "add", "-b", branch, path.to_str().unwrap()],
    );
}

fn git<const N: usize>(dir: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_with_dates(dir: &Path, message: &str, author_date: &str, committer_date: &str) {
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_DATE", committer_date)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn list_shows_current_repository_worktrees_from_nested_directory() {
    let repo = Repository::new();
    let nested = repo.main.join("nested");
    fs::create_dir(&nested).unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    let output = command.arg("list").current_dir(&nested).output().unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("BRANCH"));
    assert!(!stderr.contains("STATE"));
    assert!(stderr.contains("PATH"));
    assert!(stderr.contains("* main"));
    assert!(stderr.contains(repo.main.to_str().unwrap()));
    assert!(stderr.contains("feature"));
    assert!(stderr.contains(repo.linked.to_str().unwrap()));
    assert!(
        stderr.find(repo.main.to_str().unwrap()).unwrap()
            < stderr.find(repo.linked.to_str().unwrap()).unwrap(),
        "{stderr}"
    );
    assert!(!stderr.contains("clean"));
    assert!(stderr.contains("2 worktrees"), "{stderr}");
}

#[test]
fn list_uses_committer_timestamp_and_converts_it_to_local_time() {
    let repo = Repository::new();
    fs::write(repo.main.join("timestamp.txt"), "timestamp\n").unwrap();
    git(&repo.main, ["add", "timestamp.txt"]);
    commit_with_dates(
        &repo.main,
        "record timestamp",
        "2030-06-07T08:09:10+0000",
        "2024-01-02T03:04:05-0500",
    );

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("TZ", "UTC0")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let branch = stderr.find("BRANCH").unwrap();
    let last_commit = stderr.find("LAST COMMIT AT").unwrap();
    let path = stderr.find("PATH").unwrap();
    assert!(branch < last_commit && last_commit < path, "{stderr}");
    assert!(stderr.contains("2024-01-02 08:04"), "{stderr}");
    assert!(!stderr.contains("2030-06-07"), "{stderr}");
}

#[test]
fn list_honors_global_sort_and_ignored_local_override() {
    let repo = Repository::new();
    fs::write(repo.linked.join("older.txt"), "older\n").unwrap();
    git(&repo.linked, ["add", "older.txt"]);
    commit_with_dates(
        &repo.linked,
        "older feature",
        "2023-01-01T00:00:00+0000",
        "2023-01-01T00:00:00+0000",
    );
    fs::write(repo.main.join("newer.txt"), "newer\n").unwrap();
    git(&repo.main, ["add", "newer.txt"]);
    commit_with_dates(
        &repo.main,
        "newer main",
        "2024-01-01T00:00:00+0000",
        "2024-01-01T00:00:00+0000",
    );

    let xdg = tempfile::tempdir().unwrap();
    let config_dir = xdg.path().join("worktrees");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "worktrees:\n  default-sort: branch\n",
    )
    .unwrap();

    let global = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(global.status.success());
    assert!(global.stdout.is_empty());
    let global_stderr = String::from_utf8(global.stderr).unwrap();
    assert!(
        global_stderr.contains("Worktrees (branch A-Z)"),
        "{global_stderr}"
    );
    assert!(global_stderr.contains("BRANCH ↑"), "{global_stderr}");
    assert!(
        global_stderr.find(repo.linked.to_str().unwrap()).unwrap()
            < global_stderr.find(repo.main.to_str().unwrap()).unwrap(),
        "{global_stderr}"
    );

    fs::write(
        repo.main.join(".git/info/exclude"),
        "/.worktrees.local.yaml\n",
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.local.yaml"),
        "worktrees:\n  default-sort: last-commit-at\n",
    )
    .unwrap();
    let local = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(local.status.success());
    assert!(local.stdout.is_empty());
    let local_stderr = String::from_utf8(local.stderr).unwrap();
    assert!(
        local_stderr.contains("Worktrees (last commit newest-first)"),
        "{local_stderr}"
    );
    assert!(local_stderr.contains("LAST COMMIT AT ↓"), "{local_stderr}");
    assert!(
        local_stderr.find(repo.main.to_str().unwrap()).unwrap()
            < local_stderr.find(repo.linked.to_str().unwrap()).unwrap(),
        "{local_stderr}"
    );
}

#[test]
fn list_rejects_invalid_and_shared_default_sort_with_source_context() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    let config_dir = xdg.path().join("worktrees");
    let global_path = config_dir.join("config.yaml");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(&global_path, "worktrees:\n  default-sort: newest\n").unwrap();

    let invalid = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    let invalid_stderr = String::from_utf8(invalid.stderr).unwrap();
    assert!(invalid_stderr.contains(&global_path.display().to_string()));
    assert!(
        invalid_stderr.contains("unknown variant `newest`"),
        "{invalid_stderr}"
    );

    fs::remove_file(global_path).unwrap();
    let shared_path = repo.main.join(".worktrees.yaml");
    fs::write(&shared_path, "worktrees:\n  default-sort: path\n").unwrap();
    let shared = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(!shared.status.success());
    assert!(shared.stdout.is_empty());
    let shared_stderr = String::from_utf8(shared.stderr).unwrap();
    assert!(shared_stderr.contains(&shared_path.display().to_string()));
    assert!(
        shared_stderr.contains("unknown field `default-sort`"),
        "{shared_stderr}"
    );
}

#[test]
fn list_abbreviates_home_directory_with_tilde() {
    let repo = Repository::new();
    let home = repo.temp.path().canonicalize().unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("HOME", &home)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("~/main"), "{stderr}");
    assert!(stderr.contains("~/feature worktree"), "{stderr}");
    assert!(!stderr.contains(home.to_str().unwrap()), "{stderr}");
}

#[test]
fn list_uses_semantic_terminal_styles_without_writing_stdout() {
    let repo = Repository::new();
    fs::write(repo.linked.join("dirty.txt"), "dirty\n").unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("list")
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let output = run_terminal_command(command);

    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::heading_style(),
            "Worktrees (Git order)"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::worktree_data_style().bold(),
            "main"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains(&forced_style(worktrees::ui::warning_style(), "*")),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::muted_style(),
            "2 worktrees, 1 dirty"
        )),
        "{}",
        output.stderr
    );

    let mut no_color = Command::cargo_bin("worktrees").unwrap();
    no_color
        .arg("list")
        .current_dir(&repo.main)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    let plain = run_terminal_command(no_color);
    assert!(plain.status.success(), "{}", plain.stderr);
    assert!(plain.stdout.is_empty());
    assert!(!plain.stderr.contains('\u{1b}'), "{}", plain.stderr);
    assert!(plain.stderr.contains("* main"), "{}", plain.stderr);
}

#[test]
fn current_worktree_paths_with_trailing_spaces_are_marked_and_defaulted() {
    let repo = Repository::new();
    let trailing = repo.add_worktree("trailing-space ", "trailing-branch");

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&trailing)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("* trailing-branch"), "{stderr}");

    let switched = run_switch(&trailing, b"\r");
    assert!(switched.status.success(), "{}", switched.stderr);
    assert_eq!(
        switched.stdout,
        format!("{}\n", trailing.canonicalize().unwrap().display())
    );
}

#[test]
fn list_labels_staged_unstaged_and_untracked_changes_dirty() {
    let repo = Repository::new();
    let untracked = repo.add_worktree("untracked", "untracked-branch");

    fs::write(repo.main.join("staged.txt"), "staged\n").unwrap();
    git(&repo.main, ["add", "staged.txt"]);
    fs::write(repo.linked.join("README.md"), "unstaged\n").unwrap();
    fs::write(untracked.join("new.txt"), "untracked\n").unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("* main *"), "{stderr}");
    assert!(stderr.contains("feature *"), "{stderr}");
    assert!(stderr.contains("untracked-branch *"), "{stderr}");
    assert_eq!(stderr.matches("dirty").count(), 1, "{stderr}");
    assert!(stderr.contains("3 worktrees, 3 dirty"), "{stderr}");
}

#[test]
fn list_preserves_detached_locked_prunable_and_bare_records() {
    let repo = Repository::new();
    let missing = repo.add_worktree("missing", "missing-branch");
    git(&repo.linked, ["checkout", "--detach"]);
    git(
        &repo.main,
        [
            "worktree",
            "lock",
            "--reason",
            "maintenance",
            repo.linked.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&missing).unwrap();

    let bare = repo.temp.path().join("bare.git");
    git(
        repo.temp.path(),
        [
            "clone",
            "--bare",
            repo.main.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let bare_linked = repo.temp.path().join("bare-linked");
    git(
        &bare,
        ["worktree", "add", bare_linked.to_str().unwrap(), "main"],
    );

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("(detached)"), "{stderr}");
    assert!(stderr.contains("feature"), "{stderr}");
    assert!(stderr.contains("missing-branch"), "{stderr}");
    assert!(stderr.contains("1 locked, 1 prunable"), "{stderr}");

    let bare_output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&bare_linked)
        .output()
        .unwrap();
    assert!(bare_output.stdout.is_empty());
    let bare_stderr = String::from_utf8(bare_output.stderr).unwrap();
    assert!(bare_stderr.contains("(bare)"), "{bare_stderr}");
    assert!(bare_stderr.contains("bare"), "{bare_stderr}");
}

#[test]
fn inaccessible_worktrees_are_labeled_and_not_selectable_when_permissions_allow() {
    let repo = Repository::new();
    let original_permissions = fs::metadata(&repo.linked).unwrap().permissions();
    fs::set_permissions(&repo.linked, fs::Permissions::from_mode(0o000)).unwrap();
    let platform_can_still_enter = Command::new("sh")
        .arg("-c")
        .arg(":")
        .current_dir(&repo.linked)
        .status()
        .is_ok_and(|status| status.success());
    if platform_can_still_enter {
        fs::set_permissions(&repo.linked, original_permissions).unwrap();
        eprintln!("skipping inaccessible-path assertions: permissions are bypassed");
        return;
    }

    let listed = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .output()
        .unwrap();
    let switched = run_switch(&repo.main, b"\r");
    fs::set_permissions(&repo.linked, original_permissions).unwrap();

    assert!(listed.status.success());
    assert!(listed.stdout.is_empty());
    let stderr = String::from_utf8(listed.stderr).unwrap();
    assert!(stderr.contains("inaccessible"), "{stderr}");
    assert!(switched.status.success(), "{}", switched.stderr);
    assert!(!switched.stderr.contains("feature"), "{}", switched.stderr);
}

#[test]
fn list_reports_unknown_when_git_status_fails() {
    let repo = Repository::new();
    let fake_bin = repo.temp.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nif [ \"$1\" = status ]; then exit 71; fi\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let real_git = find_executable("git");

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("PATH", &fake_bin)
        .env("REAL_GIT", real_git)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("unknown").count(), 1, "{stderr}");
    assert!(stderr.contains("2 worktrees, 2 unknown"), "{stderr}");
}

#[test]
fn metadata_failure_warns_once_for_human_list_and_is_structured_for_json() {
    let repo = Repository::new();
    let fake_bin = repo.temp.path().join("metadata-failure-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nif [ \"$1\" = cat-file ]; then echo 'metadata command failed' >&2; exit 71; fi\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let real_git = find_executable("git");

    let human = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("PATH", &fake_bin)
        .env("REAL_GIT", &real_git)
        .output()
        .unwrap();

    assert!(human.status.success());
    assert!(human.stdout.is_empty());
    let human_stderr = String::from_utf8(human.stderr).unwrap();
    let stderr = console::strip_ansi_codes(&human_stderr);
    assert_eq!(
        stderr
            .matches("failed to load last-commit metadata")
            .count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("metadata command failed"), "{stderr}");

    let json = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["list", "--output", "json"])
        .current_dir(&repo.main)
        .env("PATH", &fake_bin)
        .env("REAL_GIT", &real_git)
        .output()
        .unwrap();

    assert!(json.status.success());
    let json = assert_json_pure(&json);
    assert!(
        json["result"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|worktree| worktree["last_commit_at"].is_null())
    );
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{json}");
    assert_eq!(diagnostics[0]["source"], "git.commit_metadata");
    assert_eq!(diagnostics[0]["stream"], "metadata");
    assert!(
        diagnostics[0]["content"]
            .as_str()
            .unwrap()
            .contains("metadata command failed"),
        "{json}"
    );
}

#[test]
fn metadata_uses_one_batch_and_is_skipped_for_get() {
    let repo = Repository::new();
    fs::write(repo.main.join("main-head.txt"), "main\n").unwrap();
    git(&repo.main, ["add", "main-head.txt"]);
    git(&repo.main, ["commit", "-m", "main head"]);
    fs::write(repo.linked.join("feature-head.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature-head.txt"]);
    git(&repo.linked, ["commit", "-m", "feature head"]);

    let fake_bin = repo.temp.path().join("metadata-batch-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nif [ \"$1\" = cat-file ]; then printf 'call\\n' >> \"$CALL_LOG\"; /bin/cat > \"$INPUT_LOG\"; exec \"$REAL_GIT\" \"$@\" < \"$INPUT_LOG\"; fi\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let real_git = find_executable("git");
    let call_log = repo.temp.path().join("cat-file-calls");
    let input_log = repo.temp.path().join("cat-file-input");

    let get = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["get", "branch"])
        .current_dir(&repo.main)
        .env("PATH", &fake_bin)
        .env("REAL_GIT", &real_git)
        .env("CALL_LOG", &call_log)
        .env("INPUT_LOG", &input_log)
        .output()
        .unwrap();
    assert!(
        get.status.success(),
        "{}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert!(!call_log.exists());

    let list = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&repo.main)
        .env("PATH", &fake_bin)
        .env("REAL_GIT", &real_git)
        .env("CALL_LOG", &call_log)
        .env("INPUT_LOG", &input_log)
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert_eq!(fs::read_to_string(&call_log).unwrap(), "call\n");
    let requested_heads = fs::read_to_string(&input_log).unwrap();
    assert_eq!(requested_heads.lines().count(), 2, "{requested_heads}");
}

#[test]
fn list_reports_an_actionable_error_outside_a_repository() {
    let temp = tempfile::tempdir().unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("list")
        .current_dir(temp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Git worktrees").or(predicate::str::contains("git worktree")),
        );
}

#[test]
fn list_reports_when_git_cannot_be_started() {
    let temp = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();

    Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(temp.path())
        .env("PATH", empty_path.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to list Git worktrees"));
}

#[test]
fn switch_defaults_to_current_worktree_and_keeps_stdout_pure() {
    let repo = Repository::new();

    let output = run_switch(&repo.linked, b"\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display())
    );
    assert!(
        output.stderr.contains("Choose a worktree"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("feature"), "{}", output.stderr);
}

#[test]
fn switch_ctrl_s_preserves_selection_and_stdout_purity() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("NO_COLOR", "1");

    let output =
        run_resized_pty_command(command, (24, 600), (24, 600), b"\x1b[B\x13\x13\x13\x13\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display())
    );
    let stderr = console::strip_ansi_codes(&output.stderr);
    assert!(stderr.contains("BRANCH ↑"), "{stderr}");
    assert!(stderr.contains("Ctrl-S sort"), "{stderr}");
    assert!(stderr.contains("branch A-Z"), "{stderr}");
    assert!(stderr.contains("last commit newest-first"), "{stderr}");
    assert!(stderr.contains("path A-Z"), "{stderr}");
    assert!(stderr.matches("Git order").count() >= 2, "{stderr}");
}

#[test]
fn switch_picker_preflights_stdin_before_rendering() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command.arg("switch").current_dir(&repo.main);

    let output = run_terminal_command(command);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("no interactive terminal"),
        "{}",
        output.stderr
    );
    assert!(
        !output.stderr.contains("Choose a worktree"),
        "{}",
        output.stderr
    );
}

#[test]
fn switch_picker_uses_semantic_styles_and_keeps_stdout_pure() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let output = run_pty_command(command, b"\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::heading_style(),
            "Choose a worktree"
        )),
        "{}",
        output.stderr
    );
    let discovery = worktrees::git::discover_with_metadata(&repo.main).unwrap();
    let choices: Vec<_> = discovery
        .worktrees
        .iter()
        .filter(|worktree| worktree.navigable())
        .collect();
    let labels = worktrees::render::menu_labels(&choices);
    let current = choices
        .iter()
        .position(|worktree| worktree.current)
        .unwrap();
    let selected_label = console::strip_ansi_codes(&labels[current]);
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::selected_style(),
            selected_label
        )),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::worktree_data_style().bold(),
            "feature"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::muted_style(),
            "type to filter"
        )),
        "{}",
        output.stderr
    );
    for shortcut in ["Ctrl-A then 1–9", "Shift-Tab", "Enter", "Esc/Ctrl-C"] {
        assert!(
            output
                .stderr
                .contains(&forced_style(worktrees::ui::shortcut_style(), shortcut)),
            "missing semantic shortcut {shortcut:?}: {}",
            output.stderr
        );
    }
}

#[test]
fn switch_picker_honors_disabled_color_without_polluting_stdout() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");

    let output = run_pty_command(command, b"\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    assert!(!output.stdout.contains('\u{1b}'));
    assert!(!contains_sgr(&output.stderr), "{}", output.stderr);
    assert!(
        output.stderr.contains("Choose a worktree"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("* main"), "{}", output.stderr);
}

#[test]
fn switch_pagination_hint_uses_muted_semantic_style() {
    let repo = Repository::new();
    for index in 0..8 {
        repo.add_worktree(&format!("page-{index}"), &format!("page-{index}"));
    }
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let output = run_pty_command_with_rows(command, b"\r", 10);

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    let plain = console::strip_ansi_codes(&output.stderr);
    let hint = plain
        .lines()
        .find_map(|line| line.find('↓').map(|start| line[start..].trim().to_owned()))
        .expect("a constrained terminal should show a lower-page hint");
    assert!(
        output
            .stderr
            .contains(&forced_style(worktrees::ui::muted_style(), &hint)),
        "{}",
        output.stderr
    );
}

#[test]
fn switch_picker_redraws_for_a_narrower_terminal() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("NO_COLOR", "1");

    let terminal_columns = 18;
    let output = run_resized_pty_command(command, (24, 80), (10, terminal_columns), b"m\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let frame_start = output
        .stderr
        .rfind("◆  Choose")
        .expect("the resized frame should retain its header");
    let frame_end = output.stderr[frame_start..]
        .find("\x1b[?25h")
        .map_or(output.stderr.len(), |offset| frame_start + offset);
    let frame = &output.stderr[frame_start..frame_end];
    assert!(
        frame.lines().any(|line| line.trim_end() == "│  m"),
        "{frame}"
    );
    assert!(frame.contains("● * main"), "{frame}");
    assert!(frame.contains("└"), "{frame}");
    assert!(frame.lines().all(|line| {
        unicode_width::UnicodeWidthStr::width(console::strip_ansi_codes(line).as_ref())
            <= usize::from(terminal_columns)
    }));
    assert!(
        frame.lines().count() <= 10,
        "resized frame exceeded terminal rows: {frame}"
    );
    assert!(
        output.stderr.contains("selection cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("panicked"), "{}", output.stderr);
}

#[test]
fn switch_ctrl_a_number_selects_the_numbered_worktree() {
    let repo = Repository::new();
    let second = repo.add_worktree("second-shortcut", "second-shortcut");

    let output = run_switch(&repo.main, b"\x012\x1b");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", second.canonicalize().unwrap().display())
    );
}

#[test]
fn switch_filter_mode_filters_choices_and_selects_the_match() {
    let repo = Repository::new();
    let filtered = repo.add_worktree("filtered-choice", "needle-filter");

    let output = run_switch(&repo.main, b"needle\x1b[B\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", filtered.canonicalize().unwrap().display())
    );
    assert!(output.stderr.contains("needle-filter"), "{}", output.stderr);
}

#[test]
fn switch_empty_filter_can_be_recovered_with_backspace() {
    let repo = Repository::new();

    let output = run_switch(&repo.main, b"mainx\x7f\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    assert!(!output.stderr.contains("panicked"), "{}", output.stderr);
}

#[test]
fn switch_empty_filter_can_be_cancelled_without_panicking() {
    let repo = Repository::new();

    let output = run_switch(&repo.main, b"no-such-worktree\r\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("No worktrees match this filter"),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains("selection cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("panicked"), "{}", output.stderr);
}

#[test]
fn switch_picker_marks_dirty_branches_and_shows_paths() {
    let repo = Repository::new();
    let long = repo.add_worktree(
        &format!("worktree-{}", "very-long-path-segment-".repeat(8)),
        "long-path-choice",
    );
    fs::write(long.join("dirty.txt"), "dirty\n").unwrap();

    let output = run_switch(&repo.main, b"\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    let picker = console::strip_ansi_codes(&output.stderr);
    assert!(output.stderr.contains("main"), "{}", output.stderr);
    assert!(
        output.stderr.contains(repo.main.to_str().unwrap()),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(long.to_str().unwrap()),
        "{}",
        output.stderr
    );
    assert!(picker.contains("* main"), "{}", output.stderr);
    assert!(picker.contains("long-path-choice *"), "{}", output.stderr);
    assert!(
        !output.stderr.contains("current, clean"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("dirty"), "{}", output.stderr);
}

#[test]
fn switch_omits_missing_and_bare_records() {
    let repo = Repository::new();
    let missing = repo.add_worktree("missing-switch", "missing-switch-branch");
    fs::remove_dir_all(&missing).unwrap();

    let output = run_switch(&repo.main, b"feature\x1b[B\r");
    assert!(output.status.success(), "{}", output.stderr);
    assert!(!output.stderr.contains("missing-switch-branch"));

    let bare = repo.temp.path().join("switch-bare.git");
    git(
        repo.temp.path(),
        [
            "clone",
            "--bare",
            repo.main.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let linked = repo.temp.path().join("switch-bare-linked");
    git(&bare, ["worktree", "add", linked.to_str().unwrap(), "main"]);
    let bare_output = run_switch(&linked, b"\r");
    assert!(bare_output.status.success(), "{}", bare_output.stderr);
    assert!(!bare_output.stderr.contains("(bare)"));
}

#[test]
fn switch_escape_cancels_without_a_destination() {
    let repo = Repository::new();

    let output = run_switch(&repo.main, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("selection cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("error:"), "{}", output.stderr);
    assert!(
        !output.stderr.contains("failed to read"),
        "{}",
        output.stderr
    );
}

#[test]
fn switch_rejects_a_repository_with_no_navigable_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let bare = temp.path().join("only-bare.git");
    git(temp.path(), ["init", "--bare", bare.to_str().unwrap()]);

    Command::cargo_bin("worktrees")
        .unwrap()
        .arg("switch")
        .current_dir(&bare)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no navigable worktrees"));
}

#[test]
fn lifecycle_completion_uses_semantic_success_without_polluting_stdout() {
    let repo = Repository::new();
    let mut remove = Command::cargo_bin("worktrees").unwrap();
    remove
        .args(["remove", "feature"])
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let removed = run_pty_command(remove, b"");

    assert!(removed.status.success(), "{}", removed.stderr);
    assert!(removed.stdout.is_empty());
    assert!(
        removed.stderr.contains(&forced_style(
            worktrees::ui::success_style(),
            "Removed 1 worktree; branches retained."
        )),
        "{}",
        removed.stderr
    );
    git(&repo.main, ["show-ref", "--verify", "refs/heads/feature"]);

    let topic = repo.add_worktree("merge-topic", "merge-topic");
    fs::write(
        topic.join(".worktrees.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&topic, ["add", ".worktrees.yaml"]);
    git(&topic, ["commit", "-m", "configure merge target"]);
    let mut merge = Command::cargo_bin("worktrees").unwrap();
    merge
        .args(["merge", "--no-remove"])
        .current_dir(&topic)
        .env("CLICOLOR_FORCE", "1");

    let merged = run_pty_command(merge, b"");

    assert!(merged.status.success(), "{}", merged.stderr);
    assert!(merged.stdout.is_empty());
    assert!(
        merged.stderr.contains(&format!(
            "{} {} {} {}{}",
            forced_style(worktrees::ui::success_style(), "Merged"),
            forced_style(worktrees::ui::worktree_data_style(), "merge-topic"),
            forced_style(worktrees::ui::success_style(), "into"),
            forced_style(worktrees::ui::worktree_data_style(), "main"),
            forced_style(worktrees::ui::success_style(), "; worktree retained.")
        )),
        "{}",
        merged.stderr
    );
    assert_eq!(
        git_output(&repo.main, ["log", "-1", "--format=%s"]),
        "configure merge target"
    );
}

#[test]
fn merge_falls_back_to_main_without_target_configuration() {
    let repo = Repository::new();
    fs::write(repo.linked.join("feature.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature.txt"]);
    git(&repo.linked, ["commit", "-m", "feature change"]);

    let mut merge = Command::cargo_bin("worktrees").unwrap();
    merge
        .args(["merge", "--no-remove"])
        .current_dir(&repo.linked)
        .env("CLICOLOR_FORCE", "1");
    let merged = run_pty_command(merge, b"");

    assert!(merged.status.success(), "{}", merged.stderr);
    assert!(merged.stderr.contains("into"), "{}", merged.stderr);
    assert_eq!(
        git_output(&repo.main, ["log", "-1", "--format=%s"]),
        "feature change"
    );
}

#[test]
fn merge_yolo_stages_commits_and_merges_all_changes() {
    let repo = Repository::new();
    fs::write(
        repo.linked.join(".worktrees.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&repo.linked, ["add", ".worktrees.yaml"]);
    git(&repo.linked, ["commit", "-m", "configure merge target"]);
    fs::write(repo.linked.join("yolo.txt"), "ship it\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: 'printf \"feat: yolo merge\\n\"'\n",
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["merge", "--yolo", "--no-remove"])
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        git_output(&repo.main, ["log", "-1", "--format=%s"]),
        "feat: yolo merge"
    );
    assert_eq!(git_output(&repo.main, ["show", "HEAD:yolo.txt"]), "ship it");
}

#[test]
fn install_decline_makes_no_filesystem_changes() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();
    let zshrc = zdot.path().join(".zshrc");
    fs::write(&zshrc, b"export KEEP=yes\n").unwrap();

    let output = run_install(home.path(), xdg.path(), Some(zdot.path()), b"n\r");
    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains("declined"), "{}", output.stderr);
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::warning_style(),
            "Installation declined; no files were changed."
        )),
        "{}",
        output.stderr
    );

    assert_eq!(fs::read(&zshrc).unwrap(), b"export KEEP=yes\n");
    assert!(!xdg.path().join("worktrees/worktrees.zsh").exists());
}

#[test]
fn install_escape_reports_cancellation_without_filesystem_changes() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    let output = run_install(home.path(), xdg.path(), Some(zdot.path()), b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("installation cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("error:"), "{}", output.stderr);
    assert!(!xdg.path().join("worktrees/worktrees.zsh").exists());
    assert!(!zdot.path().join(".zshrc").exists());
}

#[test]
fn install_rejects_a_noninteractive_confirmation_before_rendering_a_prompt() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    let output = install_command(home.path(), xdg.path(), Some(zdot.path()))
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("no interactive terminal"), "{stderr}");
    assert!(
        !stderr.contains("Planned zsh integration changes"),
        "{stderr}"
    );
    assert!(!xdg.path().join("worktrees/worktrees.zsh").exists());
    assert!(!zdot.path().join(".zshrc").exists());
}

#[test]
fn install_preserves_zshrc_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();
    let zshrc = zdot.path().join(".zshrc");
    let original = b"export KEEP='exactly this'\n# unrelated\n";
    fs::write(&zshrc, original).unwrap();

    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    assert!(installed.stdout.is_empty());
    assert!(
        installed.stderr.contains(&forced_style(
            worktrees::ui::success_style(),
            "Installed zsh integration."
        )),
        "{}",
        installed.stderr
    );
    assert!(
        installed.stderr.contains(&forced_style(
            worktrees::ui::success_style(),
            "Zsh integration installed."
        )),
        "{}",
        installed.stderr
    );

    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let generated = fs::read_to_string(&integration).unwrap();
    assert!(generated.contains("worktrees() { _worktrees_dispatch worktrees \"$@\"; }"));
    assert!(generated.contains("wt() { _worktrees_dispatch wt \"$@\"; }"));
    assert!(generated.contains("builtin cd -- \"$destination\""));
    assert!(generated.contains("command \"$executable\" \"$@\""));

    let first_zshrc = fs::read(&zshrc).unwrap();
    assert!(first_zshrc.starts_with(original));
    let first_text = String::from_utf8(first_zshrc.clone()).unwrap();
    assert_eq!(
        first_text
            .matches("# >>> worktrees shell integration >>>")
            .count(),
        1
    );

    let current = install_command(home.path(), xdg.path(), Some(zdot.path()))
        .output()
        .unwrap();
    assert!(current.status.success());
    assert!(current.stdout.is_empty());
    assert!(
        String::from_utf8(current.stderr)
            .unwrap()
            .contains("already current")
    );
    assert_eq!(fs::read(&zshrc).unwrap(), first_zshrc);
    assert_eq!(fs::read_to_string(&integration).unwrap(), generated);
}

#[test]
fn install_preserves_a_symlinked_zshrc() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target = target_dir.path().join("real-zshrc");
    fs::write(&target, b"export PRESERVED=yes\n").unwrap();
    let zshrc = zdot.path().join(".zshrc");
    symlink(&target, &zshrc).unwrap();

    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    assert!(
        fs::symlink_metadata(&zshrc)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let target_content = fs::read(&target).unwrap();
    assert!(target_content.starts_with(b"export PRESERVED=yes\n"));
    assert!(
        String::from_utf8(target_content)
            .unwrap()
            .contains("# >>> worktrees shell integration >>>")
    );
}

#[test]
fn install_falls_back_to_home_configuration_paths() {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("install")
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ZDOTDIR");

    let output = run_pty_command(command, b"y\r");
    assert!(output.status.success(), "{}", output.stderr);
    assert!(
        home.path()
            .join(".config/worktrees/worktrees.zsh")
            .is_file()
    );
    assert!(home.path().join(".zshrc").is_file());
}

#[test]
fn installed_zsh_wt_function_changes_the_invoking_shell_directory() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let repo = Repository::new();
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("worktrees")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let bin = tempfile::tempdir().unwrap();
    symlink(&binary, bin.path().join("wt")).unwrap();
    let script = format!(
        "source {}; wt switch || exit $?; builtin pwd -P",
        shell_quote(&integration)
    );
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );

    let output = run_pty_command(
        {
            let mut command = Command::new("zsh");
            command
                .args(["-f", "-c", &script])
                .current_dir(&repo.main)
                .env("PATH", path);
            command
        },
        b"feature\x1b[B\r",
    );
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!(
            "{}\n{}\n",
            repo.linked.canonicalize().unwrap().display(),
            repo.main.canonicalize().unwrap().display()
        )
    );
}

#[test]
fn installed_zsh_function_passes_merge_help_through_without_changing_directory() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let repo = Repository::new();
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("worktrees")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; before=$PWD; worktrees merge --help; rc=$?; [[ $PWD == $before ]] || exit 99; exit $rc",
        shell_quote(&integration)
    );
    let path = format!(
        "{}:{}",
        binary.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );

    let output = run_pty_command(
        {
            let mut command = Command::new("zsh");
            command
                .args(["-f", "-i", "-c", &script])
                .current_dir(&repo.main)
                .env("PATH", path);
            command
        },
        b"",
    );

    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.contains("Usage: worktrees merge [OPTIONS]"));
    assert!(
        !output.stderr.contains("file name too long"),
        "{}",
        output.stderr
    );
}

#[test]
fn installed_zsh_function_preserves_directory_and_status_on_cancellation() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let repo = Repository::new();
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("worktrees")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; before=$PWD; worktrees switch; rc=$?; [[ $PWD == $before ]] || exit 99; builtin pwd -P; exit $rc",
        shell_quote(&integration)
    );
    let path = format!(
        "{}:{}",
        binary.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let output = run_pty_command(
        {
            let mut command = Command::new("zsh");
            command
                .args(["-f", "-c", &script])
                .current_dir(&repo.main)
                .env("PATH", path);
            command
        },
        b"\x1b",
    );
    assert!(!output.status.success());
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.main.canonicalize().unwrap().display())
    );
    assert!(
        output.stderr.contains("selection cancelled"),
        "{}",
        output.stderr
    );
}

#[test]
fn installed_zsh_function_delegates_other_commands_unchanged() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let fake_bin = tempfile::tempdir().unwrap();
    let fake = fake_bin.path().join("worktrees");
    fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        fake_bin.path().display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new(find_executable("zsh"))
        .args([
            "-f",
            "-c",
            &format!(
                "source {}; worktrees list --future",
                shell_quote(&integration)
            ),
        ])
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "list\n--future\n"
    );
}

#[test]
fn switch_explicitly_enters_an_existing_worktree() {
    let repo = Repository::new();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "feature"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display()).as_bytes()
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Worktree destination printed.")
    );
}

#[test]
fn machine_readable_commands_keep_themed_feedback_off_stdout() {
    let repo = Repository::new();
    let mut get = Command::cargo_bin("worktrees").unwrap();
    get.args(["get", "branch"])
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let queried = run_pty_command(get, b"");

    assert!(queried.status.success(), "{}", queried.stderr);
    assert_eq!(queried.stdout, "main\n");
    assert!(!queried.stdout.contains('\u{1b}'));
    assert!(
        queried.stderr.contains(&forced_style(
            worktrees::ui::muted_style(),
            "Branch printed."
        )),
        "{}",
        queried.stderr
    );

    let xdg = tempfile::tempdir().unwrap();
    let mut trust = Command::cargo_bin("worktrees").unwrap();
    trust
        .args(["trust", "status"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("CLICOLOR_FORCE", "1");

    let inspected = run_pty_command(trust, b"");

    assert!(inspected.status.success(), "{}", inspected.stderr);
    assert!(inspected.stdout.is_empty());
    assert!(
        inspected.stderr.contains(&forced_style(
            worktrees::ui::muted_style(),
            "Hook trust status checked."
        )),
        "{}",
        inspected.stderr
    );
}

#[test]
fn get_prints_exact_current_context_values_and_stable_ports() {
    let repo = Repository::new();
    let nested = repo.linked.join("nested");
    fs::create_dir(&nested).unwrap();

    for (property, expected) in [
        ("branch", "feature".to_owned()),
        (
            "worktree-path",
            repo.linked.canonicalize().unwrap().display().to_string(),
        ),
        (
            "primary-worktree-path",
            repo.main.canonicalize().unwrap().display().to_string(),
        ),
        ("port", "13054".to_owned()),
    ] {
        let output = Command::cargo_bin("worktrees")
            .unwrap()
            .args(["get", property])
            .current_dir(&nested)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{property}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            format!("{expected}\n").as_bytes(),
            "{property}"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("printed."), "{property}: {stderr}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn path_queries_preserve_non_utf8_unix_path_bytes() {
    let repo = Repository::new();
    let non_utf8_parent = repo
        .temp
        .path()
        .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
    fs::create_dir(&non_utf8_parent).unwrap();
    let path = non_utf8_parent.join("linked");
    let output = Command::new("git")
        .args(["worktree", "add", "-b", "non-utf8-path"])
        .arg(&path)
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let queried = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["get", "worktree-path"])
        .current_dir(&path)
        .output()
        .unwrap();
    let mut expected = path.canonicalize().unwrap().as_os_str().as_bytes().to_vec();
    expected.push(b'\n');

    assert!(
        queried.status.success(),
        "{}",
        String::from_utf8_lossy(&queried.stderr)
    );
    assert_eq!(queried.stdout, expected);
}

#[test]
fn switch_creates_an_existing_branch_at_the_configured_root() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "topic/nested"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "topic/nested"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();
    let destination = root.join("topic/nested");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display()).as_bytes()
    );
    assert!(destination.join(".git").exists());
}

#[test]
fn failed_post_create_hook_preserves_destination_and_nonzero_status() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "hooked"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - name: prepare\n      command: printf hook-output; exit 23\n",
    )
    .unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "hooked"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"y\r");
    let destination = root.join("hooked");

    assert!(!output.status.success());
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display())
    );
    assert!(output.stderr.contains("prepare"), "{}", output.stderr);
    assert!(
        output.stderr.contains("printf hook-output; exit 23"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("hook-output"), "{}", output.stderr);
    assert!(destination.exists());
}

#[test]
fn detached_branch_queries_fail_without_stdout() {
    let repo = Repository::new();
    git(&repo.linked, ["checkout", "--detach"]);

    for property in ["branch", "port"] {
        let output = Command::cargo_bin("worktrees")
            .unwrap()
            .args(["get", property])
            .current_dir(&repo.linked)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("detached"));
    }
}

#[test]
fn ignored_local_configuration_overrides_the_global_root() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    let global_root = repo.temp.path().join("global");
    let local_root = repo.temp.path().join("local");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", global_root.display()),
    )
    .unwrap();
    fs::write(repo.main.join(".gitignore"), "/.worktrees.local.yaml\n").unwrap();
    fs::write(
        repo.main.join(".worktrees.local.yaml"),
        format!("worktrees:\n  root: {}\n", local_root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["get", "worktree-root"])
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!(
            "{}\n",
            repo.temp
                .path()
                .canonicalize()
                .unwrap()
                .join("local")
                .display()
        )
        .as_bytes()
    );
}

#[test]
fn nonignored_local_configuration_is_rejected() {
    let repo = Repository::new();
    fs::write(
        repo.main.join(".worktrees.local.yaml"),
        "worktrees:\n  root: local\n",
    )
    .unwrap();

    Command::cargo_bin("worktrees")
        .unwrap()
        .args(["get", "worktree-root"])
        .current_dir(&repo.main)
        .env("HOME", repo.temp.path())
        .env_remove("XDG_CONFIG_HOME")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("must be Git-ignored"));
}

#[test]
fn tracked_local_configuration_is_rejected_even_with_an_ignore_rule() {
    let repo = Repository::new();
    fs::write(
        repo.main.join(".worktrees.local.yaml"),
        "worktrees:\n  root: local\n",
    )
    .unwrap();
    git(&repo.main, ["add", ".worktrees.local.yaml"]);
    git(&repo.main, ["commit", "-m", "track unsafe local config"]);
    fs::write(repo.main.join(".gitignore"), "/.worktrees.local.yaml\n").unwrap();

    Command::cargo_bin("worktrees")
        .unwrap()
        .args(["get", "worktree-root"])
        .current_dir(&repo.main)
        .env("HOME", repo.temp.path())
        .env_remove("XDG_CONFIG_HOME")
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be Git-ignored"));
}

#[test]
fn switch_creates_a_tracking_worktree_from_fetched_remote_state() {
    let repo = Repository::new();
    let remote = repo.temp.path().join("origin.git");
    git(
        repo.temp.path(),
        ["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repo.main,
        ["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo.main, ["branch", "collab"]);
    git(&repo.main, ["push", "origin", "collab"]);
    git(&repo.main, ["branch", "-D", "collab"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "collab"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = root.join("collab");
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display()).as_bytes()
    );
    let upstream = Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])
        .current_dir(&destination)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(upstream.stdout).unwrap().trim(),
        "origin/collab"
    );
}

#[test]
fn ambiguous_remote_branches_fail_noninteractively_before_mutation() {
    let repo = Repository::new();
    let origin = repo.temp.path().join("origin.git");
    let upstream = repo.temp.path().join("upstream.git");
    git(
        repo.temp.path(),
        ["init", "--bare", origin.to_str().unwrap()],
    );
    git(
        repo.temp.path(),
        ["init", "--bare", upstream.to_str().unwrap()],
    );
    git(
        &repo.main,
        ["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(
        &repo.main,
        ["remote", "add", "upstream", upstream.to_str().unwrap()],
    );
    git(&repo.main, ["branch", "ambiguous"]);
    git(&repo.main, ["push", "origin", "ambiguous"]);
    git(&repo.main, ["push", "upstream", "ambiguous"]);
    git(&repo.main, ["branch", "-D", "ambiguous"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "ambiguous"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("origin/ambiguous"), "{stderr}");
    assert!(stderr.contains("upstream/ambiguous"), "{stderr}");
    assert!(!root.join("ambiguous").exists());

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "ambiguous"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let selected = run_pty_command(command, b"\x1b[B\r");
    assert!(selected.status.success(), "{}", selected.stderr);
    assert_eq!(
        git_output(
            &root.join("ambiguous"),
            [
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}"
            ],
        ),
        "upstream/ambiguous"
    );
}

#[test]
fn remote_selection_caps_long_lists_to_a_scrollable_viewport() {
    let repo = Repository::new();
    for index in 0..12 {
        let remote = format!("remote-{index:02}");
        git(
            &repo.main,
            [
                "remote",
                "add",
                remote.as_str(),
                repo.main.to_str().unwrap(),
            ],
        );
        let reference = format!("refs/remotes/{remote}/many");
        git(&repo.main, ["update-ref", reference.as_str(), "HEAD"]);
    }

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command.args(["switch", "many"]).current_dir(&repo.main);
    let output = run_pty_command_with_rows(command, b"\x1b", 8);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("remote-04/many"),
        "{}",
        output.stderr
    );
    assert!(
        !output.stderr.contains("remote-05/many"),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains("remote selection cancelled"),
        "{}",
        output.stderr
    );
}

#[test]
fn remote_matching_requires_the_complete_branch_name() {
    let repo = Repository::new();
    let remote = repo.temp.path().join("origin.git");
    git(
        repo.temp.path(),
        ["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repo.main,
        ["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo.main, ["branch", "team/foo"]);
    git(&repo.main, ["push", "origin", "team/foo"]);
    git(&repo.main, ["branch", "-D", "team/foo"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "foo"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!root.join("foo").exists());
    let local = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/foo"])
        .current_dir(&repo.main)
        .status()
        .unwrap();
    assert!(!local.success());
}

#[test]
fn new_branch_is_confirmed_and_created_from_invoking_head() {
    let repo = Repository::new();
    fs::write(repo.linked.join("feature.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature.txt"]);
    git(&repo.linked, ["commit", "-m", "feature head"]);
    fs::write(repo.linked.join("dirty.txt"), "left behind\n").unwrap();
    let source_head = git_output(&repo.linked, ["rev-parse", "HEAD"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "stacked/new"])
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let output = run_pty_command(command, b"y\r");
    let destination = root.join("stacked/new");

    assert!(output.status.success(), "{}", output.stderr);
    assert!(
        output.stderr.contains("remain in the source worktree"),
        "{}",
        output.stderr
    );
    assert_eq!(git_output(&destination, ["rev-parse", "HEAD"]), source_head);
    assert!(!destination.join("dirty.txt").exists());
}

#[test]
fn new_branch_confirmation_escape_is_reported_as_cancellation() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "cancelled-branch"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let output = run_pty_command(command, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("branch creation cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("error:"), "{}", output.stderr);
    assert!(!root.join("cancelled-branch").exists());
}

#[test]
fn shared_and_local_hooks_run_in_order_and_unchanged_commands_reuse_trust() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "hook-one"]);
    git(&repo.main, ["branch", "hook-two"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - name: shared\n      command: printf shared >> setup-order\n",
    )
    .unwrap();
    fs::write(repo.main.join(".gitignore"), "/.worktrees.local.yaml\n").unwrap();
    fs::write(
        repo.main.join(".worktrees.local.yaml"),
        "hooks:\n  post-create:\n    - name: local\n      command: printf local >> setup-order\n",
    )
    .unwrap();

    let mut first = Command::cargo_bin("worktrees").unwrap();
    first
        .args(["switch", "hook-one"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let approved = run_pty_command(first, b"y\r");
    assert!(approved.status.success(), "{}", approved.stderr);
    assert_eq!(
        fs::read_to_string(root.join("hook-one/setup-order")).unwrap(),
        "sharedlocal"
    );

    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - name: renamed only\n      command: printf shared >> setup-order\n",
    )
    .unwrap();
    let reused = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "hook-two"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();
    assert!(
        reused.status.success(),
        "{}",
        String::from_utf8_lossy(&reused.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("hook-two/setup-order")).unwrap(),
        "sharedlocal"
    );
}

#[test]
fn declining_hook_approval_is_a_warning_without_mutation() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "declined-hook"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: touch should-not-exist\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "declined-hook"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .env("CLICOLOR_FORCE", "1");

    let output = run_pty_command(command, b"n\r");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains(&forced_style(
            worktrees::ui::warning_style(),
            "post-create commands approval declined; no commands were run"
        )),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("error:"), "{}", output.stderr);
    assert!(!root.join("declined-hook").exists());
}

#[test]
fn hook_approval_escape_reports_cancellation_without_mutation() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "cancelled-hook"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: touch should-not-exist\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "cancelled-hook"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let output = run_pty_command(command, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output
            .stderr
            .contains("post-create commands approval cancelled"),
        "{}",
        output.stderr
    );
    assert!(!output.stderr.contains("error:"), "{}", output.stderr);
    assert!(!root.join("cancelled-hook").exists());
}

#[test]
fn incomplete_setup_supports_enter_once_then_mark_complete() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "recover"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: exit 9\n",
    )
    .unwrap();

    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("worktrees").unwrap();
        command
            .args(["switch", "recover"])
            .current_dir(&repo.main)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("HOME", repo.temp.path());
        run_pty_command(command, input)
    };
    let failed = run(b"y\r");
    assert!(!failed.status.success());
    let cancelled = run(b"\x1b");
    assert!(!cancelled.status.success());
    assert!(cancelled.stdout.is_empty());
    assert!(
        cancelled.stderr.contains("setup recovery cancelled"),
        "{}",
        cancelled.stderr
    );
    assert!(!cancelled.stderr.contains("error:"), "{}", cancelled.stderr);
    let moved = repo.temp.path().join("moved-recover");
    git(
        &repo.main,
        [
            "worktree",
            "move",
            root.join("recover").to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let once = run(b"\x1b[B\r");
    assert!(!once.status.success());
    assert_eq!(
        once.stdout,
        format!("{}\n", moved.canonicalize().unwrap().display())
    );
    assert!(
        once.stderr.contains("remains incomplete"),
        "{}",
        once.stderr
    );
    let accepted = run(b"\x1b[B\x1b[B\r");
    assert!(accepted.status.success(), "{}", accepted.stderr);
    let final_switch = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "recover"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();
    assert!(final_switch.status.success());
}

#[test]
fn picker_branch_action_uses_the_shared_resolver_and_branch_entry_cancels() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "picker-existing"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("worktrees").unwrap();
        command
            .arg("switch")
            .current_dir(&repo.main)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("HOME", repo.temp.path());
        run_pty_command(command, input)
    };

    let created = run(b"Create\rpicker-existing\r");
    assert!(created.status.success(), "{}", created.stderr);
    assert!(
        created.stderr.contains(
            &worktrees::ui::interactive(worktrees::ui::heading_style())
                .apply_to("Branch name:")
                .to_string()
        ),
        "{}",
        created.stderr
    );
    assert!(root.join("picker-existing").exists(), "{}", created.stderr);

    let cancelled = run(b"\x1b[Z\x1b");
    assert!(!cancelled.status.success());
    assert!(cancelled.stdout.is_empty());
    assert!(
        cancelled.stderr.contains("branch entry cancelled"),
        "{}",
        cancelled.stderr
    );
    assert!(!cancelled.stderr.contains("error:"), "{}", cancelled.stderr);
}

#[test]
fn malformed_incomplete_state_is_a_contextual_error() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "malformed-state"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: exit 7\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "malformed-state"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    assert!(!run_pty_command(command, b"y\r").status.success());
    let common = PathBuf::from(git_output(
        &repo.main,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let marker_dir = common.join("worktrees-state/incomplete");
    let marker = fs::read_dir(marker_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(marker, "truncated").unwrap();

    let retried = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "malformed-state"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();

    assert!(!retried.status.success());
    assert!(retried.stdout.is_empty());
    assert!(
        String::from_utf8(retried.stderr)
            .unwrap()
            .contains("failed to parse incomplete setup record")
    );
}

#[test]
fn detached_incomplete_worktree_can_retry_setup_from_the_picker() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "detached-retry"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: exit 8\n",
    )
    .unwrap();
    let mut create = Command::cargo_bin("worktrees").unwrap();
    create
        .args(["switch", "detached-retry"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    assert!(!run_pty_command(create, b"y\r").status.success());
    let destination = root.join("detached-retry");
    git(&destination, ["checkout", "--detach"]);
    let mut retry = Command::cargo_bin("worktrees").unwrap();
    retry
        .arg("switch")
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let retried = run_pty_command(retry, b"detached-retry\x1b[B\r\r");

    assert!(
        !retried.status.success(),
        "stdout={} stderr={}",
        retried.stdout,
        retried.stderr
    );
    assert_eq!(
        retried.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display())
    );
    assert!(
        retried.stderr.contains("post-create setup failed"),
        "{}",
        retried.stderr
    );
}

#[test]
fn recovery_retry_uses_current_commands_and_rechecks_trust() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "retry"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: exit 9\n",
    )
    .unwrap();
    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("worktrees").unwrap();
        command
            .args(["switch", "retry"])
            .current_dir(&repo.main)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("HOME", repo.temp.path());
        run_pty_command(command, input)
    };
    assert!(!run(b"y\r").status.success());
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: printf repaired > repaired.txt\n",
    )
    .unwrap();

    let retried = run(b"\ry\r");

    assert!(retried.status.success(), "{}", retried.stderr);
    assert!(
        retried.stderr.contains("repaired.txt"),
        "{}",
        retried.stderr
    );
    assert_eq!(
        fs::read_to_string(root.join("retry/repaired.txt")).unwrap(),
        "repaired"
    );
}

#[test]
fn interrupted_setup_emits_no_destination_and_empty_hooks_clear_its_record() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "interrupted"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: kill -INT $$\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["switch", "interrupted"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let interrupted = run_pty_command(command, b"y\r");

    assert!(!interrupted.status.success());
    assert!(interrupted.stdout.is_empty());
    assert!(root.join("interrupted").exists());
    fs::remove_file(repo.main.join(".worktrees.yaml")).unwrap();
    let cleared = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "interrupted"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap();
    assert!(
        cleared.status.success(),
        "{}",
        String::from_utf8_lossy(&cleared.stderr)
    );
    assert!(!cleared.stdout.is_empty());
}

#[test]
fn installed_zsh_enters_destination_but_preserves_hook_failure_status() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let repo = Repository::new();
    git(&repo.main, ["branch", "zsh-hook"]);
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let root = repo.temp.path().join("created");
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: exit 17\n",
    )
    .unwrap();
    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("worktrees")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; worktrees switch zsh-hook; rc=$?; builtin pwd -P; exit $rc",
        shell_quote(&integration)
    );
    let path = format!(
        "{}:{}",
        binary.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let output = run_pty_command(
        {
            let mut command = Command::new("zsh");
            command
                .args(["-f", "-c", &script])
                .current_dir(&repo.main)
                .env("PATH", path)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", xdg.path());
            command
        },
        b"y\r",
    );

    assert!(!output.status.success());
    assert_eq!(
        output.stdout,
        format!(
            "{}\n{}\n",
            root.join("zsh-hook").canonicalize().unwrap().display(),
            repo.main.canonicalize().unwrap().display()
        )
    );
}

#[test]
fn trust_status_reset_and_reapproval_follow_the_current_command_hash() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "trusted-one"]);
    git(&repo.main, ["branch", "trusted-two"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "hooks:\n  post-create:\n    - command: 'true'\n",
    )
    .unwrap();
    let command = |args: &[&str]| {
        Command::cargo_bin("worktrees")
            .unwrap()
            .args(args)
            .current_dir(&repo.main)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("HOME", repo.temp.path())
            .output()
            .unwrap()
    };
    let untrusted = command(&["trust", "status"]);
    assert!(untrusted.stdout.is_empty());
    assert!(
        String::from_utf8(untrusted.stderr)
            .unwrap()
            .contains("not trusted")
    );

    let mut approve = Command::cargo_bin("worktrees").unwrap();
    approve
        .args(["switch", "trusted-one"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    assert!(run_pty_command(approve, b"y\r").status.success());
    let trusted = command(&["trust", "status"]);
    assert!(trusted.stdout.is_empty());
    assert!(
        String::from_utf8(trusted.stderr)
            .unwrap()
            .contains("are trusted")
    );

    let reset = command(&["trust", "reset"]);
    assert!(reset.stdout.is_empty());
    assert!(String::from_utf8(reset.stderr).unwrap().contains("Reset"));
    let reset_again = command(&["trust", "reset"]);
    assert!(reset_again.stdout.is_empty());
    assert!(
        String::from_utf8(reset_again.stderr)
            .unwrap()
            .contains("No saved")
    );
    let trust_json = fs::read(xdg.path().join("worktrees/trust.json")).unwrap();
    serde_json::from_slice::<serde_json::Value>(&trust_json).unwrap();

    let refused = command(&["switch", "trusted-two"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("no interactive terminal")
    );
    assert!(!root.join("trusted-two").exists());
}

#[test]
fn trust_status_rejects_malformed_storage_even_without_commands() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(xdg.path().join("worktrees/trust.json"), "not-json").unwrap();

    Command::cargo_bin("worktrees")
        .unwrap()
        .args(["trust", "status"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to parse trust storage"));
}

fn git_output<const N: usize>(dir: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn run_install(home: &Path, xdg: &Path, zdotdir: Option<&Path>, input: &[u8]) -> PtyOutput {
    let mut command = install_command(home, xdg, zdotdir);
    command.env("CLICOLOR_FORCE", "1");
    run_pty_command(command, input)
}

fn install_command(home: &Path, xdg: &Path, zdotdir: Option<&Path>) -> Command {
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .arg("install")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg);
    if let Some(zdotdir) = zdotdir {
        command.env("ZDOTDIR", zdotdir);
    } else {
        command.env_remove("ZDOTDIR");
    }
    command
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

struct PtyOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn forced_style(style: console::Style, value: impl std::fmt::Display) -> String {
    style.force_styling(true).apply_to(value).to_string()
}

fn contains_sgr(value: &str) -> bool {
    let bytes = value.as_bytes();
    (0..bytes.len().saturating_sub(2)).any(|start| {
        bytes[start..].starts_with(b"\x1b[")
            && bytes[start + 2..]
                .iter()
                .find(|byte| byte.is_ascii_alphabetic())
                .is_some_and(|terminator| *terminator == b'm')
    })
}

fn run_switch(cwd: &Path, input: &[u8]) -> PtyOutput {
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command.arg("switch").current_dir(cwd);
    run_pty_command(command, input)
}

fn run_terminal_command(mut command: Command) -> PtyOutput {
    let stdout_pty = openpty(None, None).unwrap();
    let stderr_pty = openpty(None, None).unwrap();
    let mut stdout_reader = fs::File::from(stdout_pty.master);
    let mut stderr_reader = fs::File::from(stderr_pty.master);
    let mut child = command
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_pty.slave))
        .stderr(Stdio::from(stderr_pty.slave))
        .spawn()
        .unwrap();
    drop(command);

    let stdout = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout_reader.read_to_end(&mut bytes);
        bytes
    });
    let stderr = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr_reader.read_to_end(&mut bytes);
        bytes
    });
    let status = child.wait().unwrap();
    PtyOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout.join().unwrap()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.join().unwrap()).into_owned(),
    }
}

fn run_pty_command(command: Command, input: &[u8]) -> PtyOutput {
    run_pty_command_with_rows(command, input, 24)
}

fn run_pty_command_with_rows(command: Command, input: &[u8], rows: u16) -> PtyOutput {
    run_pty_command_with_size(command, input, rows, 600)
}

struct PtySession {
    child: std::process::Child,
    master_writer: fs::File,
    master_reader: fs::File,
}

fn start_pty_command(mut command: Command, window: Winsize) -> PtySession {
    let pty = openpty(Some(&window), None).unwrap();
    let stdin_fd = dup(&pty.slave).unwrap();
    let stderr_fd = dup(&pty.slave).unwrap();
    let master_writer = fs::File::from(dup(&pty.master).unwrap());
    let master_reader = fs::File::from(pty.master);
    let child = command
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_fd))
        .spawn()
        .unwrap();
    drop(command);
    drop(pty.slave);
    PtySession {
        child,
        master_writer,
        master_reader,
    }
}

fn finish_pty_command(
    mut child: std::process::Child,
    reader: thread::JoinHandle<Vec<u8>>,
) -> PtyOutput {
    let status = child.wait().unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let stderr = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    PtyOutput {
        status,
        stdout,
        stderr,
    }
}

fn run_pty_command_with_size(command: Command, input: &[u8], rows: u16, columns: u16) -> PtyOutput {
    let window = Winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let PtySession {
        child,
        mut master_writer,
        mut master_reader,
    } = start_pty_command(command, window);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = master_reader.read_to_end(&mut bytes);
        bytes
    });
    master_writer.write_all(input).unwrap();
    master_writer.flush().unwrap();
    drop(master_writer);
    finish_pty_command(child, reader)
}

fn run_resized_pty_command(
    command: Command,
    initial_size: (u16, u16),
    resized: (u16, u16),
    input: &[u8],
) -> PtyOutput {
    let initial_window = Winsize {
        ws_row: initial_size.0,
        ws_col: initial_size.1,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let PtySession {
        child,
        mut master_writer,
        mut master_reader,
    } = start_pty_command(command, initial_window);
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        let mut ready_sender = Some(ready_sender);
        loop {
            match master_reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes
                        .windows("Choose a worktree".len())
                        .any(|window| window == b"Choose a worktree")
                        && let Some(sender) = ready_sender.take()
                    {
                        sender.send(()).unwrap();
                    }
                }
            }
        }
        bytes
    });
    ready_receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("picker did not render before the resize");
    let resized_window = Winsize {
        ws_row: resized.0,
        ws_col: resized.1,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `master_writer` owns a valid PTY descriptor and the ioctl only
    // reads the fully initialized window-size value for this call.
    let resized = unsafe {
        nix::libc::ioctl(
            master_writer.as_raw_fd(),
            nix::libc::TIOCSWINSZ,
            &raw const resized_window,
        )
    };
    assert_eq!(resized, 0);
    master_writer.write_all(input).unwrap();
    master_writer.flush().unwrap();
    drop(master_writer);
    finish_pty_command(child, reader)
}

fn find_executable(name: &str) -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .unwrap();
    assert!(output.status.success());
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

#[test]
fn commit_with_explicit_message_stages_all_change_kinds() {
    let repo = Repository::new();
    fs::write(repo.main.join("README.md"), "updated\n").unwrap();
    fs::write(repo.main.join("added.txt"), "added\n").unwrap();
    fs::write(repo.main.join("deleted.txt"), "delete me\n").unwrap();
    git(&repo.main, ["add", "deleted.txt"]);
    git(&repo.main, ["commit", "-m", "add deletable file"]);
    fs::remove_file(repo.main.join("deleted.txt")).unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["commit", "--stage-all", "-m", "feat: commit every change"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Created commit"), "{stderr}");
    assert!(stderr.contains("feat: commit every change"), "{stderr}");
    assert!(stderr.contains("Staged changes:"), "{stderr}");
    assert!(stderr.contains("README.md"), "{stderr}");
    assert!(stderr.contains("added.txt"), "{stderr}");
    assert!(stderr.contains("deleted.txt"), "{stderr}");
    assert!(
        stderr.contains("insertions(+)") && stderr.contains("deletions(-)"),
        "{stderr}"
    );
    let subject = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(subject.stdout).unwrap().trim(),
        "feat: commit every change"
    );
    let changed = Command::new("git")
        .args(["show", "--format=", "--name-only", "HEAD"])
        .current_dir(&repo.main)
        .output()
        .unwrap();
    let changed = String::from_utf8(changed.stdout).unwrap();
    assert!(
        changed.contains("README.md")
            && changed.contains("added.txt")
            && changed.contains("deleted.txt")
    );
}

#[test]
fn shared_commit_generator_requires_standalone_approval_interactively() {
    let repo = Repository::new();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path());

    let output = run_pty_command(command, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("run worktrees trust commit-approve"),
        "{}",
        output.stderr
    );
    assert_eq!(
        Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&repo.main)
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
}

#[test]
fn shared_commit_generator_approval_preflights_noninteractive_terminals() {
    let repo = Repository::new();
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        stderr.contains("run worktrees trust commit-approve"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("repository requests these commit generation settings"),
        "{stderr}"
    );
    assert_eq!(
        Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&repo.main)
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
}

#[test]
fn commit_generates_message_from_global_configuration() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: \"printf 'feat: generated\\n\\n- first change\\n- second change\\n'\"\n",
    ).unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("CLICOLOR_FORCE", "1");
    let output = run_terminal_command(command);

    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    let stderr = &output.stderr;
    for heading in [
        "Staged changes:",
        "Generating commit message...",
        "Generated commit message:",
    ] {
        assert!(
            stderr.contains(&forced_style(worktrees::ui::heading_style(), heading)),
            "missing semantic heading {heading:?}: {stderr}"
        );
    }
    let plain_stderr = console::strip_ansi_codes(stderr);
    assert_eq!(
        plain_stderr.matches("Generated commit message").count(),
        1,
        "generation completion was printed more than once: {stderr}"
    );
    assert!(
        plain_stderr.lines().any(|line| {
            line.find("Generated commit message:").is_some_and(|start| {
                line[start + "Generated commit message:".len()..]
                    .split_whitespace()
                    .next()
                    .and_then(|duration| duration.strip_suffix('s'))
                    .is_some_and(|seconds| seconds.parse::<u64>().is_ok())
            })
        }),
        "generation completion omitted its duration: {stderr}"
    );
    assert!(
        stderr.contains(&forced_style(
            worktrees::ui::worktree_data_style(),
            "generated.txt"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&forced_style(
            worktrees::ui::worktree_data_style().bold(),
            "feat: generated"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&forced_style(
            worktrees::ui::success_style(),
            "Committed changes @"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("\u{1b}[2m0s\u{1b}[0m") || stderr.contains("\u{1b}[2m1s\u{1b}[0m"),
        "missing muted elapsed metadata: {stderr}"
    );
    let hash = git_output(&repo.main, ["rev-parse", "--short=7", "HEAD"]);
    assert!(
        stderr.contains(&forced_style(worktrees::ui::muted_style(), hash)),
        "{stderr}"
    );
    let message = Command::new("git")
        .args(["log", "-1", "--format=%B"])
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(message.stdout).unwrap(),
        "feat: generated\n\n- first change\n- second change\n\n"
    );
}

#[test]
fn commit_generation_spinner_reports_elapsed_time() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: \"sleep 2; printf 'feat: generated\\n\\n- first change\\n- second change\\n'\"\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("CLICOLOR_FORCE", "1");
    let output = run_terminal_command(command);

    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    let plain_stderr = console::strip_ansi_codes(&output.stderr);
    assert!(
        plain_stderr.contains("Generating commit message... 1s"),
        "generation spinner never displayed a nonzero elapsed time: {}",
        output.stderr
    );
    let completion_seconds = plain_stderr.lines().find_map(|line| {
        let start = line.find("Generated commit message:")?;
        line[start + "Generated commit message:".len()..]
            .split_whitespace()
            .next()?
            .strip_suffix('s')?
            .parse::<u64>()
            .ok()
    });
    assert!(
        completion_seconds.is_some_and(|seconds| seconds >= 2),
        "generation completion did not report the elapsed time: {}",
        output.stderr
    );
}

#[test]
fn commit_generator_failure_finishes_the_spinner_with_an_error_state() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: sleep 2; exit 23\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("CLICOLOR_FORCE", "1");

    let output = run_terminal_command(command);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let plain_stderr = console::strip_ansi_codes(&output.stderr);
    assert!(
        plain_stderr.contains("Generating commit message... 1s"),
        "generation spinner never displayed a nonzero elapsed time: {}",
        output.stderr
    );
    assert_eq!(
        plain_stderr
            .matches("Failed to generate commit message")
            .count(),
        1,
        "generation failure was printed more than once: {}",
        output.stderr
    );
    assert!(
        plain_stderr.contains("commit generator failed with status"),
        "{}",
        output.stderr
    );
}

#[test]
fn commit_generator_trust_commands_distinguish_absent_and_user_controlled_settings() {
    let repo = Repository::new();
    let absent_xdg = tempfile::tempdir().unwrap();
    let absent = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["trust", "commit-status"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", absent_xdg.path())
        .output()
        .unwrap();
    assert!(absent.status.success());
    assert!(absent.stdout.is_empty());
    assert!(
        String::from_utf8(absent.stderr)
            .unwrap()
            .contains("No commit generator is configured.")
    );

    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    let controlled = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["trust", "commit-status"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(controlled.status.success());
    assert!(controlled.stdout.is_empty());
    assert!(
        String::from_utf8(controlled.stderr)
            .unwrap()
            .contains("The effective commit generator is user-controlled.")
    );

    let reset = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["trust", "commit-reset"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(reset.status.success());
    assert!(reset.stdout.is_empty());
    assert!(
        String::from_utf8(reset.stderr)
            .unwrap()
            .contains("No saved commit generator trust existed for this repository.")
    );
}

#[test]
fn commit_renders_custom_template_with_staged_context() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    let captured = xdg.path().join("prompt.txt");
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: 'cat > \"$CAPTURE\"; printf \"chore: generated\\n\\n- one\\n- two\\n\"'\n    template: |\n      repo={{ repo }} branch={{ branch }}\n      {% for subject in recent_commits %}history={{ subject }}\n      {% endfor %}{{ git_diff_stat }}\n      {{ git_diff }}\n",
    ).unwrap();
    fs::write(repo.main.join("custom.txt"), "content\n").unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("CAPTURE", &captured)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let prompt = fs::read_to_string(captured).unwrap();
    assert!(prompt.contains("repo=main branch=main"), "{prompt}");
    assert!(
        prompt.contains("history=initial") && prompt.contains("custom.txt"),
        "{prompt}"
    );
}

#[test]
fn bare_commit_uses_only_the_existing_index() {
    let repo = Repository::new();
    fs::write(repo.main.join("README.md"), "staged\n").unwrap();
    git(&repo.main, ["add", "README.md"]);
    fs::write(repo.main.join("README.md"), "unstaged\n").unwrap();
    fs::write(repo.main.join("untracked.txt"), "excluded\n").unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["commit", "-m", "fix: commit staged snapshot"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git_output(&repo.main, ["show", "HEAD:README.md"]), "staged");
    assert_eq!(
        fs::read_to_string(repo.main.join("README.md")).unwrap(),
        "unstaged\n"
    );
    assert!(repo.main.join("untracked.txt").exists());
}

#[test]
fn json_dry_run_is_one_document_and_does_not_commit() {
    let repo = Repository::new();
    fs::write(repo.main.join("staged.txt"), "ready\n").unwrap();
    git(&repo.main, ["add", "staged.txt"]);
    let before = git_output(&repo.main, ["rev-parse", "HEAD"]);

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args([
            "commit",
            "--dry-run",
            "--message",
            "test: preview",
            "--output",
            "json",
        ])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "success");
    assert_eq!(value["result"]["outcome"], "dry_run");
    assert_eq!(git_output(&repo.main, ["rev-parse", "HEAD"]), before);
}

fn json_command(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&serde_json::Value>,
) -> std::process::Output {
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut child_stdin = child.stdin.take().unwrap();
    if let Some(value) = stdin {
        serde_json::to_writer(&mut child_stdin, value).unwrap();
    }
    drop(child_stdin);
    child.wait_with_output().unwrap()
}

fn assert_json_pure(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn json_list_and_switch_include_commit_times_in_stable_git_order() {
    let repo = Repository::new();
    fs::write(repo.main.join("main-timestamp.txt"), "main\n").unwrap();
    git(&repo.main, ["add", "main-timestamp.txt"]);
    commit_with_dates(
        &repo.main,
        "main timestamp",
        "2030-01-01T00:00:00+0000",
        "2024-01-02T03:04:05-0500",
    );
    fs::write(repo.linked.join("feature-timestamp.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature-timestamp.txt"]);
    commit_with_dates(
        &repo.linked,
        "feature timestamp",
        "2031-01-01T00:00:00+0000",
        "2024-01-03T04:05:06+0930",
    );

    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "worktrees:\n  default-sort: branch\n",
    )
    .unwrap();

    let list = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["list", "--output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let list = assert_json_pure(&list);
    let worktrees = list["result"]["worktrees"].as_array().unwrap();
    assert_eq!(worktrees[0]["branch"], "main");
    assert_eq!(worktrees[1]["branch"], "feature");
    assert_eq!(worktrees[0]["last_commit_at"], "2024-01-02T03:04:05-05:00");
    assert_eq!(worktrees[1]["last_commit_at"], "2024-01-03T04:05:06+09:30");

    let switch = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "--output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(!switch.status.success());
    let switch = assert_json_pure(&switch);
    let choices = switch["context"]["choices"].as_array().unwrap();
    assert_eq!(choices[0]["branch"], "main");
    assert_eq!(choices[1]["branch"], "feature");
    assert_eq!(choices[0]["last_commit_at"], "2024-01-02T03:04:05-05:00");
    assert_eq!(choices[1]["last_commit_at"], "2024-01-03T04:05:06+09:30");

    for command in ["list", "switch"] {
        let help = Command::cargo_bin("worktrees")
            .unwrap()
            .args([command, "--help", "--output", "json"])
            .current_dir(&repo.main)
            .output()
            .unwrap();
        assert!(help.status.success());
        let help = assert_json_pure(&help);
        assert!(
            serde_json::to_string(&help["result"])
                .unwrap()
                .contains("last_commit_at"),
            "{help}"
        );
    }
}

#[test]
fn json_switch_argv_and_request_modes_select_existing_destination() {
    let repo = Repository::new();
    let argv = json_command(&repo.main, &["switch", "feature", "--output", "json"], None);
    assert!(argv.status.success());
    let argv = assert_json_pure(&argv);
    let request = serde_json::json!({"schema_version":1,"request_id":"switch-1","input":{"branch":"feature"}});
    let io = json_command(
        &repo.main,
        &["switch", "--input-output", "json"],
        Some(&request),
    );
    assert!(io.status.success());
    let io = assert_json_pure(&io);
    assert_eq!(argv["result"]["destination"], io["result"]["destination"]);
    assert_eq!(io["request_id"], "switch-1");
}

#[test]
fn json_switch_without_branch_returns_structured_choices_without_prompting() {
    let repo = Repository::new();
    let output = json_command(&repo.main, &["switch", "--output", "json"], None);
    assert!(!output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["error"]["code"], "switch.selection_required");
    assert_eq!(value["context"]["choices"].as_array().unwrap().len(), 2);
}

#[test]
fn json_switch_new_branch_dry_run_is_nonmutating_and_execution_requires_approval() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let before = git_output(&repo.main, ["worktree", "list", "--porcelain"]);
    let preview = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "new-topic", "--dry-run", "--output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(preview.status.success());
    let value = assert_json_pure(&preview);
    assert_eq!(value["result"]["outcome"], "creation_plan");
    assert_eq!(value["result"]["approval_required"], true);
    assert_eq!(
        git_output(&repo.main, ["worktree", "list", "--porcelain"]),
        before
    );
    assert!(!root.exists());
    let execute = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["switch", "new-topic", "--output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(!execute.status.success());
    assert_eq!(
        assert_json_pure(&execute)["error"]["code"],
        "switch.approval_required"
    );
}

#[test]
fn json_remove_dry_run_has_effects_and_does_not_remove() {
    let repo = Repository::new();
    let request =
        serde_json::json!({"schema_version":1,"input":{"branches":["feature"],"dry_run":true}});
    let output = json_command(
        &repo.main,
        &["remove", "--input-output", "json"],
        Some(&request),
    );
    assert!(output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["result"]["outcome"], "dry_run");
    assert!(
        value["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["attempted"] == false)
    );
    assert!(repo.linked.exists());
}

#[test]
fn json_remove_execution_removes_multiple_worktrees_and_retains_branches() {
    let repo = Repository::new();
    let second = repo.add_worktree("second topic", "second");
    let request = serde_json::json!({"schema_version":1,"request_id":"remove-1","input":{"branches":["feature","second"]}});
    let output = json_command(
        &repo.main,
        &["remove", "--input-output", "json"],
        Some(&request),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = assert_json_pure(&output);
    assert_eq!(value["request_id"], "remove-1");
    assert_eq!(value["result"]["outcome"], "removed");
    assert_eq!(value["result"]["targets"].as_array().unwrap().len(), 2);
    assert!(
        value["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["completed"] == true)
    );
    assert!(!repo.linked.exists());
    assert!(!second.exists());
    assert_eq!(
        git_output(&repo.main, ["branch", "--list", "feature"]),
        "feature"
    );
    assert_eq!(
        git_output(&repo.main, ["branch", "--list", "second"]),
        "second"
    );
}

#[test]
fn json_forced_remove_requires_explicit_force_and_reports_retention() {
    let repo = Repository::new();
    fs::write(repo.linked.join("dirty.txt"), "dirty\n").unwrap();
    let request = serde_json::json!({"schema_version":1,"input":{"branches":["feature"]}});
    let output = json_command(
        &repo.main,
        &["remove", "--force", "--input-output", "json"],
        Some(&request),
    );
    assert!(output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["result"]["targets"][0]["branch_retained"], true);
    assert_eq!(
        git_output(&repo.main, ["branch", "--list", "feature"]),
        "feature"
    );
}

#[test]
fn json_remove_rejects_legacy_force_in_request() {
    let repo = Repository::new();
    let request =
        serde_json::json!({"schema_version":1,"input":{"branches":["feature"],"force":true}});
    let output = json_command(
        &repo.main,
        &["remove", "--input-output", "json"],
        Some(&request),
    );
    assert!(!output.status.success());
    assert_eq!(
        assert_json_pure(&output)["error"]["code"],
        "json.invalid_request"
    );
    assert!(repo.linked.exists());
}

#[test]
fn human_remove_dry_run_uses_preflight_without_mutation() {
    let repo = Repository::new();
    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["remove", "feature", "--dry-run"])
        .current_dir(&repo.main)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("branch retained"));
    assert!(repo.linked.exists());
}

#[test]
fn json_remove_rejects_dirty_target_without_force_and_request_mixing() {
    let repo = Repository::new();
    fs::write(repo.linked.join("dirty.txt"), "dirty\n").unwrap();
    let output = json_command(&repo.main, &["remove", "feature", "--output", "json"], None);
    assert!(!output.status.success());
    assert_eq!(
        assert_json_pure(&output)["error"]["code"],
        "remove.force_required"
    );
    let request =
        serde_json::json!({"schema_version":1,"input":{"branches":["feature"],"dry_run":true}});
    let mixed = json_command(
        &repo.main,
        &["remove", "feature", "--input-output", "json"],
        Some(&request),
    );
    assert!(!mixed.status.success());
    assert_eq!(
        assert_json_pure(&mixed)["error"]["code"],
        "json.invalid_request"
    );
}

#[test]
fn json_merge_dry_run_reports_policy_and_never_mutates_refs_or_worktrees() {
    let repo = Repository::new();
    fs::write(repo.linked.join("topic.txt"), "topic\n").unwrap();
    git(&repo.linked, ["add", "topic.txt"]);
    git(&repo.linked, ["commit", "-m", "topic"]);
    fs::write(
        repo.main.join(".worktrees.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&repo.main, ["add", ".worktrees.yaml"]);
    git(&repo.main, ["commit", "-m", "configure target"]);
    let main_before = git_output(&repo.main, ["rev-parse", "HEAD"]);
    let topic_before = git_output(&repo.linked, ["rev-parse", "HEAD"]);
    let output = json_command(
        &repo.linked,
        &["merge", "--no-remove", "--dry-run", "--output", "json"],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = assert_json_pure(&output);
    assert_eq!(value["result"]["outcome"], "dry_run");
    assert_eq!(value["result"]["policy"]["no_remove"], true);
    assert!(
        value["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["attempted"] == false)
    );
    assert_eq!(git_output(&repo.main, ["rev-parse", "HEAD"]), main_before);
    assert_eq!(
        git_output(&repo.linked, ["rev-parse", "HEAD"]),
        topic_before
    );
    assert!(repo.linked.exists());
}

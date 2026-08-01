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
    sys::termios::{InputFlags, SetArg, tcgetattr, tcsetattr},
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

    let mut command = Command::cargo_bin("pando").unwrap();
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
fn list_branches_shows_attached_and_unattached_branches() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["list", "--branches"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Branches ("), "{stderr}");
    assert!(stderr.contains("feature"), "{stderr}");
    assert!(stderr.contains(repo.linked.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("untracked-branch"), "{stderr}");
    let untracked_line = stderr
        .lines()
        .find(|line| line.contains("untracked-branch"))
        .unwrap();
    assert!(
        !untracked_line.contains(repo.temp.path().to_str().unwrap()),
        "{stderr}"
    );
    assert!(stderr.contains("3 branches"), "{stderr}");
    assert!(stderr.contains("2 checked out"), "{stderr}");
}

#[test]
fn list_branches_json_reports_null_path_for_unattached_branch() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["list", "--branches", "--output", "json"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = assert_json_pure(&output);
    let branches = json["result"]["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 3);
    let names: Vec<_> = branches
        .iter()
        .map(|b| b["branch"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "expected for-each-ref (alphabetical) order");

    let untracked = branches
        .iter()
        .find(|b| b["branch"] == "untracked-branch")
        .unwrap();
    assert_eq!(untracked["path"], serde_json::Value::Null);
    assert_eq!(untracked["condition"], serde_json::Value::Null);
    assert_eq!(untracked["current"], false);

    let feature = branches.iter().find(|b| b["branch"] == "feature").unwrap();
    assert_ne!(feature["path"], serde_json::Value::Null);
    assert_ne!(feature["condition"], serde_json::Value::Null);

    assert_eq!(json["result"]["summary"]["total"], 3);
    assert_eq!(json["result"]["summary"]["checked_out"], 2);
}

#[test]
fn list_branches_json_ignores_a_configured_default_sort() {
    let repo = Repository::new();
    // Named so that last-commit order (newest first) differs from for-each-ref
    // (alphabetical) order: "z-newest" commits after "feature".
    let z_newest = repo.temp.path().join("z-newest-worktree");
    add_worktree(&repo.main, &z_newest, "z-newest");
    fs::write(z_newest.join("touch.txt"), "touch\n").unwrap();
    git(&z_newest, ["add", "touch.txt"]);
    commit_with_dates(
        &z_newest,
        "z-newest touch",
        "2032-01-01T00:00:00+0000",
        "2032-01-01T00:00:00+0000",
    );

    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "worktrees:\n  default-sort: last-commit-at\n",
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["list", "--branches", "--output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = assert_json_pure(&output);
    let branches = json["result"]["branches"].as_array().unwrap();
    let names: Vec<_> = branches
        .iter()
        .map(|b| b["branch"].as_str().unwrap())
        .collect();
    // for-each-ref order is alphabetical; last-commit order would put
    // "z-newest" first. The configured personal sort must not apply.
    assert_eq!(names, vec!["feature", "main", "z-newest"], "{names:?}");
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

    let output = Command::cargo_bin("pando")
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
    let config_dir = xdg.path().join("pando");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "worktrees:\n  default-sort: branch\n",
    )
    .unwrap();

    let global = Command::cargo_bin("pando")
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

    fs::write(repo.main.join(".git/info/exclude"), "/.pando.local.yaml\n").unwrap();
    fs::write(
        repo.main.join(".pando.local.yaml"),
        "worktrees:\n  default-sort: last-commit-at\n",
    )
    .unwrap();
    let local = Command::cargo_bin("pando")
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
    let config_dir = xdg.path().join("pando");
    let global_path = config_dir.join("config.yaml");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(&global_path, "worktrees:\n  default-sort: newest\n").unwrap();

    let invalid = Command::cargo_bin("pando")
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
    let shared_path = repo.main.join(".pando.yaml");
    fs::write(&shared_path, "worktrees:\n  default-sort: path\n").unwrap();
    let shared = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .arg("list")
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let output = run_terminal_command(command);

    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains(&forced_style(
            pando::ui::heading_style(),
            "Worktrees (Git order)"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            pando::ui::worktree_data_style().bold(),
            "main"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains(&forced_style(pando::ui::warning_style(), "*")),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            pando::ui::muted_style(),
            "2 worktrees, 1 dirty"
        )),
        "{}",
        output.stderr
    );

    let mut no_color = Command::cargo_bin("pando").unwrap();
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

    let output = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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

    let bare_output = Command::cargo_bin("pando")
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

    let listed = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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

    let human = Command::cargo_bin("pando")
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

    let json = Command::cargo_bin("pando")
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

    let get = Command::cargo_bin("pando")
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

    let list = Command::cargo_bin("pando")
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

    let mut command = Command::cargo_bin("pando").unwrap();
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

    Command::cargo_bin("pando")
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
    let mut command = Command::cargo_bin("pando").unwrap();
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
    assert!(stderr.contains("Ctrl-B branches"), "{stderr}");
    assert!(stderr.contains("LAST COMMIT AT ↓"), "{stderr}");
    assert!(stderr.contains("PATH ↑"), "{stderr}");
    assert!(!stderr.contains("branch A-Z"), "{stderr}");
    assert!(!stderr.contains("last commit newest-first"), "{stderr}");
    assert!(!stderr.contains("path A-Z"), "{stderr}");
    assert!(!stderr.contains("Git order"), "{stderr}");
}

#[test]
fn switch_branches_flag_opens_directly_in_branch_view() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "--branches"])
        .current_dir(&repo.main);
    let output = run_pty_command(command, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = console::strip_ansi_codes(&output.stderr);
    assert!(stderr.contains("Choose a branch"), "{stderr}");
    assert!(!stderr.contains("Choose a worktree"), "{stderr}");
    assert!(stderr.contains("untracked-branch"), "{stderr}");
}

#[test]
fn switch_branches_selecting_an_attached_branch_navigates_to_its_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "--branches"])
        .current_dir(&repo.main);
    let output = run_pty_command(command, b"feature\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display())
    );
    let stderr = console::strip_ansi_codes(&output.stderr);
    assert!(stderr.contains("Choose a branch"), "{stderr}");
}

#[test]
fn switch_ctrl_b_toggles_between_worktree_and_branch_view() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);

    let mut command = Command::cargo_bin("pando").unwrap();
    command.arg("switch").current_dir(&repo.main);
    // Toggle to branch view, then back to worktree view, then cancel.
    let output = run_pty_command(command, b"\x02\x02\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = console::strip_ansi_codes(&output.stderr);
    assert!(stderr.contains("Choose a worktree"), "{stderr}");
    assert!(stderr.contains("Choose a branch"), "{stderr}");
    assert!(stderr.contains("untracked-branch"), "{stderr}");
    let last_heading = stderr.rfind("Choose a").unwrap();
    assert!(
        stderr[last_heading..].starts_with("Choose a worktree"),
        "expected the picker to end back in worktree view: {stderr}"
    );
}

#[test]
fn switch_branches_selecting_an_unattached_branch_creates_its_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "untracked-branch"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "--branches"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"untracked\r");

    assert!(output.status.success(), "{}", output.stderr);
    let destination = root.join("untracked-branch");
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display())
    );
    assert!(destination.exists(), "{}", output.stderr);
}

/// Guards the PTY flow-control setup in `start_pty_command`.
///
/// Writing the keys immediately after spawn guarantees they reach the line
/// discipline before the picker enters raw mode, which is the losing side of
/// the race that used to wedge `switch_ctrl_s_preserves_selection_and_stdout_purity`
/// under CI scheduling. With IXON left enabled the child suspends on XOFF and
/// blocks in `write` forever, so this test hangs rather than fails.
#[test]
fn switch_ctrl_s_before_raw_mode_is_not_treated_as_flow_control() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .arg("switch")
        .current_dir(&repo.main)
        .env("NO_COLOR", "1");

    let output = run_pty_command_with_size(command, b"\x1b[B\x13\x13\x13\x13\r", 24, 600);

    assert!(output.status.success(), "{}", output.stderr);
    assert!(!output.stdout.is_empty());
}

#[test]
fn switch_picker_preflights_stdin_before_rendering() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    let mut command = Command::cargo_bin("pando").unwrap();
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
            pando::ui::heading_style(),
            "Choose a worktree"
        )),
        "{}",
        output.stderr
    );
    assert!(
        !output.stderr.contains("Worktree destination printed."),
        "the rail no longer restates that a path reached stdout: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains("Esc/Ctrl-C"),
        "the picker's own closing bar ends the sequence: {}",
        output.stderr
    );
    let discovery = pando::git::discover_with_metadata(&repo.main).unwrap();
    let choices: Vec<_> = discovery
        .worktrees
        .iter()
        .filter(|worktree| worktree.navigable())
        .collect();
    let rows: Vec<_> = choices
        .iter()
        .map(|worktree| pando::Row::from_worktree(worktree))
        .collect();
    let row_refs: Vec<_> = rows.iter().collect();
    let labels = pando::render::menu_labels(&row_refs);
    let current = choices
        .iter()
        .position(|worktree| worktree.current)
        .unwrap();
    let selected_label = console::strip_ansi_codes(&labels[current]);
    assert!(
        output
            .stderr
            .contains(&forced_style(pando::ui::selected_style(), selected_label)),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(&forced_style(
            pando::ui::worktree_data_style().bold(),
            "feature"
        )),
        "{}",
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains(&forced_style(pando::ui::muted_style(), "type to filter")),
        "{}",
        output.stderr
    );
    for shortcut in ["Ctrl-A then 1–9", "Shift-Tab", "Enter", "Esc/Ctrl-C"] {
        assert!(
            output
                .stderr
                .contains(&forced_style(pando::ui::shortcut_style(), shortcut)),
            "missing semantic shortcut {shortcut:?}: {}",
            output.stderr
        );
    }
}

#[test]
fn switch_picker_honors_disabled_color_without_polluting_stdout() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    let mut command = Command::cargo_bin("pando").unwrap();
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
            .contains(&forced_style(pando::ui::muted_style(), &hint)),
        "{}",
        output.stderr
    );
}

#[test]
fn switch_picker_redraws_for_a_narrower_terminal() {
    let repo = Repository::new();
    let mut command = Command::cargo_bin("pando").unwrap();
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

    Command::cargo_bin("pando")
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
    let mut remove = Command::cargo_bin("pando").unwrap();
    remove
        .args(["remove", "feature"])
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let removed = run_pty_command(remove, b"");

    assert!(removed.status.success(), "{}", removed.stderr);
    assert!(removed.stdout.is_empty());
    assert!(
        removed.stderr.contains(&forced_style(
            pando::ui::success_style(),
            "Removed 1 worktree; branches retained."
        )),
        "{}",
        removed.stderr
    );
    git(&repo.main, ["show-ref", "--verify", "refs/heads/feature"]);

    let topic = repo.add_worktree("merge-topic", "merge-topic");
    fs::write(
        topic.join(".pando.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&topic, ["add", ".pando.yaml"]);
    git(&topic, ["commit", "-m", "configure merge target"]);
    let mut merge = Command::cargo_bin("pando").unwrap();
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
            forced_style(pando::ui::success_style(), "Merged"),
            forced_style(pando::ui::worktree_data_style(), "merge-topic"),
            forced_style(pando::ui::success_style(), "into"),
            forced_style(pando::ui::worktree_data_style(), "main"),
            forced_style(pando::ui::success_style(), "; worktree retained.")
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
fn merge_renders_git_output_inside_the_terminal_ui_rail() {
    let repo = Repository::new();
    fs::write(repo.main.join("main.txt"), "main\n").unwrap();
    git(&repo.main, ["add", "main.txt"]);
    git(&repo.main, ["commit", "-m", "main advances"]);
    fs::write(repo.linked.join("feature.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature.txt"]);
    git(&repo.linked, ["commit", "-m", "feature change"]);

    let mut merge = Command::cargo_bin("pando").unwrap();
    merge
        .args(["merge", "--no-remove"])
        .current_dir(&repo.linked)
        .env("CLICOLOR_FORCE", "1");
    let merged = run_pty_command(merge, b"");

    assert!(merged.status.success(), "{}", merged.stderr);
    assert!(merged.stdout.is_empty(), "{}", merged.stdout);
    for progress in [
        "Rebasing onto main...",
        "Rebased onto main",
        "Merging into main...",
        "Merged into main",
    ] {
        assert!(
            merged
                .stderr
                .contains(&forced_style(pando::ui::heading_style(), progress)),
            "missing {progress:?} in {}",
            merged.stderr
        );
    }
    // Git's own reporting is rendered as rail steps rather than streamed raw.
    let plain = console::strip_ansi_codes(&merged.stderr);
    for line in [
        "│  Fast-forward",
        "│  feature.txt | 1 +",
        "│  1 file changed, 1 insertion(+)",
    ] {
        assert!(plain.contains(line), "missing {line:?} in {plain}");
    }
    assert!(
        merged.stderr.contains(&forced_style(
            pando::ui::worktree_data_style(),
            "feature.txt"
        )),
        "{}",
        merged.stderr
    );
    // Git redraws its counters with carriage returns, which a captured pipe
    // preserves; only the final revision of each line may survive.
    assert!(
        plain.contains("Successfully rebased and updated refs/heads/feature."),
        "{plain}"
    );
    assert!(!plain.contains("Rebasing (1/1)"), "{plain}");
}

#[test]
fn merge_reports_a_rebase_conflict_and_resumes_without_an_editor() {
    let repo = Repository::new();
    fs::write(repo.main.join("README.md"), "main version\n").unwrap();
    git(&repo.main, ["add", "README.md"]);
    git(&repo.main, ["commit", "-m", "main edits readme"]);
    fs::write(repo.linked.join("README.md"), "feature version\n").unwrap();
    git(&repo.linked, ["add", "README.md"]);
    git(&repo.linked, ["commit", "-m", "feature edits readme"]);

    let mut conflicting = Command::cargo_bin("pando").unwrap();
    conflicting
        .args(["merge", "--no-remove"])
        .current_dir(&repo.linked)
        .env("CLICOLOR_FORCE", "1");
    let conflicted = run_pty_command(conflicting, b"");

    assert!(!conflicted.status.success(), "{}", conflicted.stderr);
    assert!(conflicted.stdout.is_empty(), "{}", conflicted.stdout);
    let plain = console::strip_ansi_codes(&conflicted.stderr);
    for line in [
        "│  CONFLICT (content): Merge conflict in README.md",
        "│  hint: Resolve all conflicts manually, mark them as resolved with",
    ] {
        assert!(plain.contains(line), "missing {line:?} in {plain}");
    }

    fs::write(repo.linked.join("README.md"), "resolved\n").unwrap();
    git(&repo.linked, ["add", "README.md"]);
    // A captured continuation has no terminal for an editor. `GIT_EDITOR=false`
    // stands in for an inherited interactive editor: the run must neutralize it
    // and reuse the recorded message rather than hand it a pipe.
    let mut resuming = Command::cargo_bin("pando").unwrap();
    resuming
        .args(["merge", "--no-remove"])
        .current_dir(&repo.linked)
        .env("CLICOLOR_FORCE", "1")
        .env("GIT_EDITOR", "false");
    let resumed = run_pty_command(resuming, b"");

    assert!(resumed.status.success(), "{}", resumed.stderr);
    assert!(
        resumed.stderr.contains(&forced_style(
            pando::ui::heading_style(),
            "Continued rebase"
        )),
        "{}",
        resumed.stderr
    );
    assert_eq!(
        git_output(&repo.main, ["log", "-1", "--format=%s"]),
        "feature edits readme"
    );
    assert_eq!(
        fs::read_to_string(repo.main.join("README.md")).unwrap(),
        "resolved\n"
    );
}

#[test]
fn merge_falls_back_to_main_without_target_configuration() {
    let repo = Repository::new();
    fs::write(repo.linked.join("feature.txt"), "feature\n").unwrap();
    git(&repo.linked, ["add", "feature.txt"]);
    git(&repo.linked, ["commit", "-m", "feature change"]);

    let mut merge = Command::cargo_bin("pando").unwrap();
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
fn merge_from_the_primary_worktree_switches_it_back_to_the_target() {
    let repo = Repository::new();
    git(&repo.main, ["switch", "-c", "inline-topic"]);
    fs::write(repo.main.join("inline.txt"), "inline\n").unwrap();
    git(&repo.main, ["add", "inline.txt"]);
    git(&repo.main, ["commit", "-m", "inline change"]);

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["merge"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Nothing was removed, so the zsh wrapper gets no destination to `cd` to.
    assert!(output.stdout.is_empty());
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
    assert_eq!(
        git_output(&repo.main, ["log", "-1", "--format=%s", "main"]),
        "inline change"
    );
    // The answered design keeps the topic branch after an in-place merge.
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "inline-topic"]),
        git_output(&repo.main, ["rev-parse", "main"])
    );
}

#[test]
fn merge_from_the_primary_worktree_refuses_on_the_target_branch() {
    let repo = Repository::new();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["merge"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already on"), "{stderr}");
}

#[test]
fn merge_yolo_stages_commits_and_merges_all_changes() {
    let repo = Repository::new();
    fs::write(
        repo.linked.join(".pando.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&repo.linked, ["add", ".pando.yaml"]);
    git(&repo.linked, ["commit", "-m", "configure merge target"]);
    fs::write(repo.linked.join("yolo.txt"), "ship it\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: 'printf \"feat: yolo merge\\n\"'\n",
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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
fn pr_missing_metadata_generator_fails_before_dirty_worktree_handling() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::write(repo.linked.join("dirty.txt"), "dirty\n").unwrap();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["pr", "create"])
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(stderr.contains("pr.generator_unavailable"), "{stderr}");
    assert!(
        stderr.contains("provide both --title and --description"),
        "{stderr}"
    );
    assert!(!stderr.contains("uncommitted changes"), "{stderr}");
    assert!(!stderr.contains("Commit all changes"), "{stderr}");
    assert!(repo.linked.join("dirty.txt").is_file());
    assert_eq!(
        git_output(&repo.linked, ["status", "--porcelain"]),
        "?? dirty.txt"
    );
}

fn configure_test_forge_remote(repo: &Repository) -> PathBuf {
    git(
        &repo.main,
        [
            "remote",
            "add",
            "origin",
            "ssh://git@forge.example/alice/project.git",
        ],
    );
    let main_head = git_output(&repo.main, ["rev-parse", "main"]);
    git(
        &repo.main,
        ["update-ref", "refs/remotes/origin/main", &main_head],
    );
    git(
        &repo.main,
        ["branch", "--set-upstream-to=origin/main", "main"],
    );
    let bare = repo.temp.path().join("forge.git");
    fs::create_dir(&bare).unwrap();
    git(&bare, ["init", "--bare"]);
    git(
        &repo.main,
        ["config", "remote.origin.pushurl", bare.to_str().unwrap()],
    );
    bare
}

fn fake_tea_without_created_url(repo: &Repository) -> (PathBuf, PathBuf, PathBuf) {
    let fake_bin = repo.temp.path().join("tea-bin");
    fs::create_dir(&fake_bin).unwrap();
    let capture = repo.temp.path().join("tea-args");
    let created = repo.temp.path().join("tea-created");
    let tea = fake_bin.join("tea");
    fs::write(
        &tea,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$TEA_CAPTURE"
case "$1" in
  --version) printf 'Version: 0.15.0\n' ;;
  login) printf '[{"name":"forge","url":"https://forge.example","ssh_host":"forge.example","user":"alice","default":"true"}]\n' ;;
  pulls)
    case "$2" in
      list)
        if test -f "$TEA_CREATED"; then
          printf '[{"url":"https://forge.example/alice/project/pulls/42","base":"main","head":"feature"}]\n'
        else
          printf '[]\n'
        fi
        ;;
      create)
        touch "$TEA_CREATED"
        printf '# #42 Add tea provider (open)\n'
        ;;
      *) exit 2 ;;
    esac
    ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&tea, fs::Permissions::from_mode(0o755)).unwrap();
    (fake_bin, capture, created)
}

#[test]
fn pr_recovers_created_url_when_tea_does_not_print_it() {
    let repo = Repository::new();
    let bare = configure_test_forge_remote(&repo);
    let (fake_bin, capture, created) = fake_tea_without_created_url(&repo);
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let xdg = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args([
            "--output",
            "json",
            "pr",
            "create",
            "--title",
            "Add tea provider",
            "--description",
            "Support Gitea and Forgejo.",
        ])
        .current_dir(&repo.linked)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("PATH", path)
        .env("TEA_CAPTURE", &capture)
        .env("TEA_CREATED", &created)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response = assert_json_pure(&output);
    assert_eq!(response["status"], "success");
    assert_eq!(response["result"]["outcome"], "created");
    assert_eq!(
        response["result"]["url"],
        "https://forge.example/alice/project/pulls/42"
    );
    assert_eq!(response["result"]["provider"], "tea");
    assert_eq!(response["result"]["draft"], true);
    assert_eq!(response["effects"][0]["completed"], true);
    assert_eq!(
        git_output(&bare, ["rev-parse", "refs/heads/feature"]),
        git_output(&repo.linked, ["rev-parse", "HEAD"])
    );

    let invocations = fs::read_to_string(capture).unwrap();
    assert!(invocations.contains("--version"), "{invocations}");
    assert!(
        invocations.contains("login list --output json"),
        "{invocations}"
    );
    let list_command = "pulls list --login forge --repo alice/project --state open --fields url,base,head --output json --page 1 --limit 100";
    assert_eq!(
        invocations.matches(list_command).count(),
        2,
        "tea should be queried before creation and again to recover the missing URL: {invocations}"
    );
    assert!(
        invocations.contains(
            "pulls create --login forge --remote origin --base main --head feature --title WIP: Add tea provider --description Support Gitea and Forgejo."
        ),
        "{invocations}"
    );
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
            pando::ui::warning_style(),
            "Installation declined; no files were changed."
        )),
        "{}",
        output.stderr
    );

    assert_eq!(fs::read(&zshrc).unwrap(), b"export KEEP=yes\n");
    assert!(!xdg.path().join("pando/pando.zsh").exists());
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
    assert!(!xdg.path().join("pando/pando.zsh").exists());
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
    assert!(!xdg.path().join("pando/pando.zsh").exists());
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
            pando::ui::success_style(),
            "Installed zsh integration."
        )),
        "{}",
        installed.stderr
    );
    assert!(
        installed.stderr.contains(&forced_style(
            pando::ui::success_style(),
            "Zsh integration installed."
        )),
        "{}",
        installed.stderr
    );

    let config = xdg.path().join("pando/config.yaml");
    let generated_config = fs::read_to_string(&config).unwrap();
    assert!(generated_config.contains("#   root: ../worktrees"));
    assert!(generated_config.contains("#   target-branch: main"));
    assert!(generated_config.contains("#   provider: auto"));
    assert!(generated_config.contains("#     command: pi --no-session --no-tools"));
    let integration = xdg.path().join("pando/pando.zsh");
    let generated = fs::read_to_string(&integration).unwrap();
    assert!(generated.contains("pando() { pando_dispatch pando \"$@\"; }"));
    assert!(generated.contains("pd() { pando_dispatch pd \"$@\"; }"));
    assert!(
        !generated.contains("\n_"),
        "no integration function may use the zsh completion `_name` prefix, which \
         function-table snapshots drop: {generated}"
    );
    assert!(generated.contains("builtin cd -- \"$destination\""));
    assert!(generated.contains("command \"$executable\" \"$@\""));

    let first_zshrc = fs::read(&zshrc).unwrap();
    assert!(first_zshrc.starts_with(original));
    let first_config = fs::read(&config).unwrap();
    assert_eq!(first_config, generated_config.as_bytes());
    let first_text = String::from_utf8(first_zshrc.clone()).unwrap();
    assert_eq!(
        first_text
            .matches("# >>> pando shell integration >>>")
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
    assert_eq!(fs::read(&config).unwrap(), first_config);
    assert_eq!(fs::read_to_string(&integration).unwrap(), generated);
}

#[test]
fn installed_integration_registers_completion_for_both_names() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);

    let generated = fs::read_to_string(xdg.path().join("pando/pando.zsh")).unwrap();
    assert!(generated.contains("_PANDO_COMPLETE=zsh command pando"));
    assert!(generated.contains("compdef _clap_dynamic_completer_pando pd"));
    assert!(
        generated.contains("$+functions[compdef]"),
        "registration must be guarded so it is skipped when compinit has not run"
    );
    assert!(
        !generated.lines().any(|line| line.starts_with('_')),
        "no integration function may use the zsh completion `_name` prefix, \
         which function-table snapshots drop: {generated}"
    );
}

#[test]
fn installed_integration_registers_compdef_under_real_zsh() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    // Generate the integration by running a real install, so the test asserts
    // against the shipped bytes rather than a copy that could drift.
    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("pando/pando.zsh");

    // Put the built binary on PATH so `command pando` resolves during the eval.
    let binary = assert_cmd::cargo::cargo_bin("pando");
    let bin_dir = binary.parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());

    let script = format!(
        "autoload -Uz compinit && compinit -u -d {dump}\n\
         source {integration}\n\
         print -r -- \"registered=${{_comps[pando]}} pd=${{_comps[pd]}}\"\n",
        dump = shell_quote(&xdg.path().join("zcompdump")),
        integration = shell_quote(&integration),
    );
    let output = Command::new("zsh")
        .args(["-i", "-c", &script])
        .env("PATH", path)
        .env("HOME", home.path())
        .env("ZDOTDIR", zdot.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("registered=_clap_dynamic_completer_pando"),
        "pando was not registered: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("pd=_clap_dynamic_completer_pando"),
        "pd was not registered: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `eval "$(... command pando ...)"` alone would `eval ""` when the binary
/// is missing from PATH, which exits 0 and reports registration as successful
/// while leaving `_clap_dynamic_completer_pando` undefined. Guards against
/// that: with the binary excluded from PATH, neither `pando` nor `pd` may
/// end up registered to a completion function that was never defined.
#[test]
fn pando_register_completion_does_not_register_when_binary_is_missing_from_path() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("pando/pando.zsh");

    // Point PATH at an empty directory, simulating `.zshrc` sourcing the
    // integration before the binary's install location joins PATH. Filtering
    // the cargo build directory out of the real PATH is not enough: a
    // developer who has run `just install` also has the binary in
    // `~/.cargo/bin`, so the test would find it anyway and pass vacuously.
    // zsh is resolved to an absolute path first, because the empty PATH hides
    // the shell itself too.
    let zsh = which_zsh();
    let empty_path = tempfile::tempdir().unwrap();
    let path = empty_path.path().to_str().unwrap().to_owned();

    let script = format!(
        "autoload -Uz compinit && compinit -u -d {dump}\n\
         source {integration}\n\
         print -r -- \"registered=${{_comps[pando]}} pd=${{_comps[pd]}}\"\n",
        dump = shell_quote(&xdg.path().join("zcompdump")),
        integration = shell_quote(&integration),
    );
    let output = Command::new(&zsh)
        .args(["-i", "-c", &script])
        .env("PATH", path)
        .env("HOME", home.path())
        .env("ZDOTDIR", zdot.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("_clap_dynamic_completer_pando"),
        "neither name may be registered to a function that was never defined: \
         {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Resolves zsh to an absolute path so a test can override PATH without
/// losing the shell binary itself.
fn which_zsh() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v zsh"])
        .output()
        .unwrap();
    assert!(output.status.success(), "zsh is not on PATH");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

#[test]
fn install_preserves_existing_global_config() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();
    let config_dir = xdg.path().join("pando");
    fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.yaml");
    let original = b"worktrees:\n  root: /custom/worktrees\ncommit:\n  generation:\n    command: custom-generator\n";
    fs::write(&config, original).unwrap();

    let installed = run_install(home.path(), xdg.path(), Some(zdot.path()), b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let content = fs::read(&config).unwrap();
    assert!(content.starts_with(original));
    assert_eq!(
        content
            .windows(b"# >>> pando configuration scaffold >>>".len())
            .filter(|window| *window == b"# >>> pando configuration scaffold >>>")
            .count(),
        1
    );
    assert_eq!(
        content
            .windows(b"# <<< pando configuration scaffold <<<".len())
            .filter(|window| *window == b"# <<< pando configuration scaffold <<<")
            .count(),
        1
    );
    assert!(
        String::from_utf8(content)
            .unwrap()
            .contains("command: custom-generator")
    );
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
            .contains("# >>> pando shell integration >>>")
    );
}

#[test]
fn install_falls_back_to_home_configuration_paths() {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .arg("install")
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ZDOTDIR");

    let output = run_pty_command(command, b"y\r");
    assert!(output.status.success(), "{}", output.stderr);
    assert!(home.path().join(".config/pando/pando.zsh").is_file());
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
    let integration = xdg.path().join("pando/pando.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("pando")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let bin = tempfile::tempdir().unwrap();
    symlink(&binary, bin.path().join("pd")).unwrap();
    let script = format!(
        "source {}; pd switch || exit $?; builtin pwd -P",
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
fn installed_zsh_wt_function_enters_a_created_worktree() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }
    let repo = Repository::new();
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let installed = run_install(home.path(), xdg.path(), None, b"y\r");
    assert!(installed.status.success(), "{}", installed.stderr);
    let integration = xdg.path().join("pando/pando.zsh");
    let root = repo.temp.path().join("created");
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let binary = PathBuf::from(
        Command::cargo_bin("pando")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let bin = tempfile::tempdir().unwrap();
    symlink(&binary, bin.path().join("pd")).unwrap();
    let script = format!(
        "source {}; pd create zsh-topic || exit $?; builtin pwd -P",
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
                .args(["-f", "-i", "-c", &script])
                .current_dir(&repo.main)
                .env("PATH", path)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", xdg.path());
            command
        },
        b"",
    );

    assert!(output.status.success(), "{}", output.stderr);
    let destination = root.join("zsh-topic").canonicalize().unwrap();
    assert!(
        output
            .stdout
            .contains(&format!("{}\n", destination.display())),
        "the shell should end up inside the created worktree: {}",
        output.stdout
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
    let integration = xdg.path().join("pando/pando.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("pando")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; before=$PWD; pando merge --help; rc=$?; [[ $PWD == $before ]] || exit 99; exit $rc",
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
    assert!(output.stdout.contains("Usage: pando merge [OPTIONS]"));
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
    let integration = xdg.path().join("pando/pando.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("pando")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; before=$PWD; pando switch; rc=$?; [[ $PWD == $before ]] || exit 99; builtin pwd -P; exit $rc",
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
    let integration = xdg.path().join("pando/pando.zsh");
    let fake_bin = tempfile::tempdir().unwrap();
    let fake = fake_bin.path().join("pando");
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
            &format!("source {}; pando list --future", shell_quote(&integration)),
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

    let output = Command::cargo_bin("pando")
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
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr, "",
        "entering an existing worktree reports nothing, so no outro closes an unopened sequence"
    );
}

#[test]
fn machine_readable_commands_keep_themed_feedback_off_stdout() {
    let repo = Repository::new();
    let mut get = Command::cargo_bin("pando").unwrap();
    get.args(["get", "branch"])
        .current_dir(&repo.main)
        .env("CLICOLOR_FORCE", "1");

    let queried = run_pty_command(get, b"");

    assert!(queried.status.success(), "{}", queried.stderr);
    assert_eq!(queried.stdout, "main\n");
    assert!(!queried.stdout.contains('\u{1b}'));
    assert_eq!(
        queried.stderr, "",
        "get answers with the requested value alone, so nothing reaches stderr"
    );

    let xdg = tempfile::tempdir().unwrap();
    let mut trust = Command::cargo_bin("pando").unwrap();
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
            pando::ui::muted_style(),
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
        let output = Command::cargo_bin("pando")
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
        assert_eq!(
            output.stderr, b"",
            "{property}: get writes only the requested value"
        );
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

    let queried = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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

/// Points global configuration at `root` and returns the configuration home to pass through.
fn config_home_with_root(root: &Path) -> TempDir {
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    xdg
}

fn create_command(repo: &Repository, xdg: &TempDir, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("pando")
        .unwrap()
        .args(args)
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .output()
        .unwrap()
}

#[test]
fn create_makes_a_new_branch_without_confirmation() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    // No PTY: a confirmation prompt here would fail the interactivity preflight.
    let output = create_command(&repo, &xdg, &["create", "topic/fresh"]);
    let destination = root.join("topic/fresh");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display()).as_bytes()
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Creating branch \"topic/fresh\""),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Create this branch and worktree?"),
        "{stderr}"
    );
    assert!(destination.join(".git").exists());
    assert!(
        git_output(&repo.main, ["branch", "--list", "topic/fresh"]).contains("topic/fresh"),
        "create should leave the branch behind"
    );
}

#[test]
fn create_checks_out_an_existing_local_branch_without_announcing_a_new_one() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "existing"]);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let output = create_command(&repo, &xdg, &["create", "existing"]);
    let destination = root.join("existing");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display()).as_bytes()
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("Creating branch"), "{stderr}");
}

#[test]
fn create_refuses_a_branch_that_already_has_a_worktree() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let output = create_command(&repo, &xdg, &["create", "feature"]);

    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a refusal must not print a destination"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("already registered"), "{stderr}");
    assert!(stderr.contains("pando switch feature"), "{stderr}");
}

#[test]
fn create_dry_run_previews_a_new_branch_and_refuses_a_registered_one() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let preview = create_command(&repo, &xdg, &["create", "topic/preview", "--dry-run"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(preview.stdout.is_empty());
    assert!(
        String::from_utf8(preview.stderr)
            .unwrap()
            .contains("Would create a worktree for topic/preview")
    );
    assert!(!root.exists(), "a preview must not create the root");
    assert!(git_output(&repo.main, ["branch", "--list", "topic/preview"]).is_empty());

    let registered = create_command(&repo, &xdg, &["create", "feature", "--dry-run"]);
    assert!(!registered.status.success());
    assert!(registered.stdout.is_empty());
    assert!(
        String::from_utf8(registered.stderr)
            .unwrap()
            .contains("already registered")
    );
}

#[test]
fn create_requires_a_branch() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let output = create_command(&repo, &xdg, &["create"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("create requires a branch")
    );
}

#[test]
fn create_runs_post_create_hooks_like_switch() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: prepare\n      command: printf hook-ran > hook.txt\n",
    )
    .unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "hooked"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    // Hook approval is a separate trust boundary that `create` still prompts for.
    let output = run_pty_command(command, b"y\r");
    let destination = root.join("hooked");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display())
    );
    assert_eq!(
        fs::read_to_string(destination.join("hook.txt")).unwrap(),
        "hook-ran"
    );
}

#[test]
fn create_refuses_post_create_hooks_without_a_terminal() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: prepare\n      command: true\n",
    )
    .unwrap();

    let output = create_command(&repo, &xdg, &["create", "hooked"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!root.join("hooked").exists());
}

#[test]
fn failed_post_create_hook_preserves_destination_and_nonzero_status() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "hooked"]);
    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: prepare\n      command: printf hook-output; exit 23\n",
    )
    .unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
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
        let output = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", global_root.display()),
    )
    .unwrap();
    fs::write(repo.main.join(".gitignore"), "/.pando.local.yaml\n").unwrap();
    fs::write(
        repo.main.join(".pando.local.yaml"),
        format!("worktrees:\n  root: {}\n", local_root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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
        repo.main.join(".pando.local.yaml"),
        "worktrees:\n  root: local\n",
    )
    .unwrap();

    Command::cargo_bin("pando")
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
        repo.main.join(".pando.local.yaml"),
        "worktrees:\n  root: local\n",
    )
    .unwrap();
    git(&repo.main, ["add", ".pando.local.yaml"]);
    git(&repo.main, ["commit", "-m", "track unsafe local config"]);
    fs::write(repo.main.join(".gitignore"), "/.pando.local.yaml\n").unwrap();

    Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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

    let mut command = Command::cargo_bin("pando").unwrap();
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

    let mut command = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: shared\n      command: printf shared >> setup-order\n",
    )
    .unwrap();
    fs::write(repo.main.join(".gitignore"), "/.pando.local.yaml\n").unwrap();
    fs::write(
        repo.main.join(".pando.local.yaml"),
        "hooks:\n  post-create:\n    - name: local\n      command: printf local >> setup-order\n",
    )
    .unwrap();

    let mut first = Command::cargo_bin("pando").unwrap();
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
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: renamed only\n      command: printf shared >> setup-order\n",
    )
    .unwrap();
    let reused = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: touch should-not-exist\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
            pando::ui::warning_style(),
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: touch should-not-exist\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: exit 9\n",
    )
    .unwrap();

    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("pando").unwrap();
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
    let final_switch = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("pando").unwrap();
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
            &pando::ui::interactive(pando::ui::heading_style())
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: exit 7\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    let marker_dir = common.join("pando-state/incomplete");
    let marker = fs::read_dir(marker_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(marker, "truncated").unwrap();

    let retried = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: exit 8\n",
    )
    .unwrap();
    let mut create = Command::cargo_bin("pando").unwrap();
    create
        .args(["switch", "detached-retry"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    assert!(!run_pty_command(create, b"y\r").status.success());
    let destination = root.join("detached-retry");
    git(&destination, ["checkout", "--detach"]);
    let mut retry = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: exit 9\n",
    )
    .unwrap();
    let run = |input: &[u8]| {
        let mut command = Command::cargo_bin("pando").unwrap();
        command
            .args(["switch", "retry"])
            .current_dir(&repo.main)
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("HOME", repo.temp.path());
        run_pty_command(command, input)
    };
    assert!(!run(b"y\r").status.success());
    fs::write(
        repo.main.join(".pando.yaml"),
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: kill -INT $$\n",
    )
    .unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "interrupted"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());

    let interrupted = run_pty_command(command, b"y\r");

    assert!(!interrupted.status.success());
    assert!(interrupted.stdout.is_empty());
    assert!(root.join("interrupted").exists());
    fs::remove_file(repo.main.join(".pando.yaml")).unwrap();
    let cleared = Command::cargo_bin("pando")
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
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: exit 17\n",
    )
    .unwrap();
    let integration = xdg.path().join("pando/pando.zsh");
    let binary = PathBuf::from(
        Command::cargo_bin("pando")
            .unwrap()
            .get_program()
            .to_owned(),
    );
    let script = format!(
        "source {}; pando switch zsh-hook; rc=$?; builtin pwd -P; exit $rc",
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - command: 'true'\n",
    )
    .unwrap();
    let command = |args: &[&str]| {
        Command::cargo_bin("pando")
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

    let mut approve = Command::cargo_bin("pando").unwrap();
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
    let trust_json = fs::read(xdg.path().join("pando/trust.json")).unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(xdg.path().join("pando/trust.json"), "not-json").unwrap();

    Command::cargo_bin("pando")
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

fn branch_description(dir: &Path, branch: &str) -> Option<String> {
    let key = format!("branch.{branch}.description");
    let mut output = Command::new("git")
        .args(["config", "--null", "--get", &key])
        .current_dir(dir)
        .output()
        .unwrap();
    match output.status.code() {
        Some(0) => {
            assert_eq!(output.stdout.last(), Some(&0));
            output.stdout.pop();
            Some(String::from_utf8(output.stdout).unwrap())
        }
        Some(1) => None,
        _ => panic!("{}", String::from_utf8_lossy(&output.stderr)),
    }
}

fn run_install(home: &Path, xdg: &Path, zdotdir: Option<&Path>, input: &[u8]) -> PtyOutput {
    let mut command = install_command(home, xdg, zdotdir);
    command.env("CLICOLOR_FORCE", "1");
    run_pty_command(command, input)
}

fn install_command(home: &Path, xdg: &Path, zdotdir: Option<&Path>) -> Command {
    let mut command = Command::cargo_bin("pando").unwrap();
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
    let mut command = Command::cargo_bin("pando").unwrap();
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
    // A fresh PTY enables IXON, so the line discipline treats Ctrl-S (`\x13`,
    // XOFF) as flow control and suspends output until it sees XON. Tests that
    // drive Ctrl-S never send XON, so any byte landing while the child is in
    // cooked mode wedges it forever in `write`. `console` only clears IXON for
    // the duration of a single `read_key` and restores cooked mode in between,
    // so that window reopens after every keystroke. Disable flow control up
    // front and Ctrl-S is delivered to the picker as an ordinary byte.
    let mut termios = tcgetattr(&pty.slave).unwrap();
    termios
        .input_flags
        .remove(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IXANY);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &termios).unwrap();
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

    let output = Command::cargo_bin("pando")
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
        repo.main.join(".pando.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["commit", "--stage-all"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path());

    let output = run_pty_command(command, b"\x1b");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.contains("run pando trust commit-approve"),
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
        repo.main.join(".pando.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let xdg = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("pando")
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
        stderr.contains("run pando trust commit-approve"),
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: \"printf 'feat: generated\\n\\n- first change\\n- second change\\n'\"\n",
    ).unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
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
            stderr.contains(&forced_style(pando::ui::heading_style(), heading)),
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
            pando::ui::worktree_data_style(),
            "generated.txt"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&forced_style(
            pando::ui::worktree_data_style().bold(),
            "feat: generated"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&forced_style(
            pando::ui::success_style(),
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
        stderr.contains(&forced_style(pando::ui::muted_style(), hash)),
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: \"sleep 2; printf 'feat: generated\\n\\n- first change\\n- second change\\n'\"\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: sleep 2; exit 23\n",
    )
    .unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();
    let mut command = Command::cargo_bin("pando").unwrap();
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
    let absent = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: printf\n",
    )
    .unwrap();
    let controlled = Command::cargo_bin("pando")
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

    let reset = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "commit:\n  generation:\n    command: 'cat > \"$CAPTURE\"; printf \"chore: generated\\n\\n- one\\n- two\\n\"'\n    template: |\n      repo={{ repo }} branch={{ branch }}\n      {% for subject in recent_commits %}history={{ subject }}\n      {% endfor %}{{ git_diff_stat }}\n      {{ git_diff }}\n",
    ).unwrap();
    fs::write(repo.main.join("custom.txt"), "content\n").unwrap();

    let output = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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

    let output = Command::cargo_bin("pando")
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
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_json_command(command, stdin)
}

fn json_create_request(
    repo: &Repository,
    xdg: &TempDir,
    request: &serde_json::Value,
) -> std::process::Output {
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "--input-output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_json_command(command, Some(request))
}

fn run_json_command(
    mut command: Command,
    stdin: Option<&serde_json::Value>,
) -> std::process::Output {
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        "worktrees:\n  default-sort: branch\n",
    )
    .unwrap();

    let list = Command::cargo_bin("pando")
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

    let switch = Command::cargo_bin("pando")
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
        let help = Command::cargo_bin("pando")
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
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();
    let before = git_output(&repo.main, ["worktree", "list", "--porcelain"]);
    let preview = Command::cargo_bin("pando")
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
    let execute = Command::cargo_bin("pando")
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
fn json_create_makes_a_new_branch_and_reports_both_effects() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let head = git_output(&repo.main, ["rev-parse", "HEAD"]);

    let preview = create_command(
        &repo,
        &xdg,
        &["create", "new-topic", "--dry-run", "--output", "json"],
    );
    assert!(preview.status.success());
    let value = assert_json_pure(&preview);
    assert_eq!(value["command"], "create");
    assert_eq!(value["result"]["outcome"], "creation_plan");
    assert_eq!(value["result"]["kind"], "new");
    assert_eq!(value["result"]["start_point"], head);
    assert_eq!(value["effects"][0]["action"], "create_branch");
    assert_eq!(value["effects"][0]["attempted"], false);
    assert!(!root.exists());

    let execute = create_command(&repo, &xdg, &["create", "new-topic", "--output", "json"]);
    assert!(execute.status.success());
    let value = assert_json_pure(&execute);
    assert_eq!(value["result"]["outcome"], "created");
    assert_eq!(value["result"]["start_point"], head);
    assert_eq!(value["effects"].as_array().unwrap().len(), 2);
    assert_eq!(value["effects"][0]["action"], "create_branch");
    assert_eq!(value["effects"][0]["completed"], true);
    assert_eq!(value["effects"][1]["action"], "create_worktree");
    assert!(root.join("new-topic/.git").exists());
    assert!(git_output(&repo.main, ["branch", "--list", "new-topic"]).contains("new-topic"));
}

#[test]
fn json_create_refuses_a_registered_branch_and_points_at_switch() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);

    let output = create_command(&repo, &xdg, &["create", "feature", "--output", "json"]);

    assert!(!output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["error"]["code"], "create.branch_registered");
    assert_eq!(value["next_steps"][0]["action"], "switch");
    assert!(value["effects"].as_array().unwrap().is_empty());
}

#[test]
fn json_create_request_mode_requires_a_branch() {
    let repo = Repository::new();
    let request = serde_json::json!({"schema_version":1,"request_id":"create-1","input":{}});
    let output = json_command(
        &repo.main,
        &["create", "--input-output", "json"],
        Some(&request),
    );

    assert!(!output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["command"], "create");
    assert_eq!(value["request_id"], "create-1");
    assert_eq!(value["error"]["code"], "create.branch_required");
}

#[test]
fn json_create_request_sets_the_exact_branch_description() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let description = "Replace lifecycle guidance with\nnative Pando commands.";
    let request = serde_json::json!({
        "schema_version": 1,
        "request_id": "create-description-1",
        "input": {
            "branch": "topic/described",
            "description": description
        }
    });

    let output = json_create_request(&repo, &xdg, &request);

    assert!(output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["request_id"], "create-description-1");
    assert_eq!(value["result"]["outcome"], "created");
    assert_eq!(value["effects"][2]["action"], "set_branch_description");
    assert_eq!(value["effects"][2]["attempted"], true);
    assert_eq!(value["effects"][2]["completed"], true);
    assert_eq!(value["effects"][2]["details"]["description"], description);
    assert_eq!(
        branch_description(&repo.main, "topic/described").as_deref(),
        Some(description)
    );
}

#[test]
fn json_create_description_dry_run_reports_without_mutating() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let request = serde_json::json!({
        "schema_version": 1,
        "input": {
            "branch": "topic/planned",
            "description": "Planned description",
            "dry_run": true
        }
    });

    let output = json_create_request(&repo, &xdg, &request);

    assert!(output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["result"]["outcome"], "creation_plan");
    assert_eq!(value["effects"][2]["action"], "set_branch_description");
    assert_eq!(value["effects"][2]["attempted"], false);
    assert_eq!(value["effects"][2]["completed"], false);
    assert!(!root.exists());
    assert_eq!(branch_description(&repo.main, "topic/planned"), None);
}

#[test]
fn json_create_description_overwrites_an_existing_local_branch_description() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "topic/existing"]);
    git(
        &repo.main,
        [
            "config",
            "branch.topic/existing.description",
            "Old description",
        ],
    );
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let request = serde_json::json!({
        "schema_version": 1,
        "input": {
            "branch": "topic/existing",
            "description": "New description"
        }
    });

    let output = json_create_request(&repo, &xdg, &request);

    assert!(output.status.success());
    assert_eq!(
        branch_description(&repo.main, "topic/existing").as_deref(),
        Some("New description")
    );
}

#[test]
fn json_create_description_does_not_modify_a_registered_branch() {
    let repo = Repository::new();
    git(
        &repo.main,
        ["config", "branch.feature.description", "Keep this"],
    );
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let request = serde_json::json!({
        "schema_version": 1,
        "input": {
            "branch": "feature",
            "description": "Do not write this"
        }
    });

    let output = json_create_request(&repo, &xdg, &request);

    assert!(!output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["error"]["code"], "create.branch_registered");
    assert!(value["effects"].as_array().unwrap().is_empty());
    assert_eq!(
        branch_description(&repo.main, "feature").as_deref(),
        Some("Keep this")
    );
}

#[test]
fn json_create_description_failure_reports_partial_creation_and_recovery() {
    let repo = Repository::new();
    let root = repo.temp.path().join("topics");
    let xdg = config_home_with_root(&root);
    let fake_bin = repo.temp.path().join("description-failure-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nif [ \"$1\" = config ] && [ \"$2\" = --local ] && [ \"$3\" = --replace-all ]; then echo 'description write failed' >&2; exit 71; fi\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let request = serde_json::json!({
        "schema_version": 1,
        "input": {
            "branch": "topic/partial",
            "description": "Requested description"
        }
    });
    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "--input-output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .env("PATH", &fake_bin)
        .env("REAL_GIT", find_executable("git"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = run_json_command(command, Some(&request));

    assert!(!output.status.success());
    let value = assert_json_pure(&output);
    assert_eq!(value["error"]["code"], "create.description_failed");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("description write failed")
    );
    assert_eq!(value["effects"][0]["completed"], true);
    assert_eq!(value["effects"][1]["completed"], true);
    assert_eq!(value["effects"][2]["action"], "set_branch_description");
    assert_eq!(value["effects"][2]["attempted"], true);
    assert_eq!(value["effects"][2]["completed"], false);
    assert_eq!(
        value["next_steps"][0]["action"],
        "git.set_branch_description"
    );
    assert_eq!(
        value["next_steps"][0]["invocation"]["argv"][4],
        "branch.topic/partial.description"
    );
    assert!(root.join("topic/partial/.git").exists());
    assert_eq!(branch_description(&repo.main, "topic/partial"), None);
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
    let output = Command::cargo_bin("pando")
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
        repo.main.join(".pando.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    git(&repo.main, ["add", ".pando.yaml"]);
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

/// Adds a local bare repository as `origin`, publishes `main`, and records
/// `origin/HEAD`, so remote-tracking refs are observable without a network.
fn add_local_origin(repo: &Repository) -> PathBuf {
    let origin = repo.temp.path().join("origin.git");
    let output = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(
        &repo.main,
        ["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo.main, ["push", "origin", "main"]);
    git(&repo.main, ["remote", "set-head", "origin", "-a"]);
    origin
}

/// Commits an unpublished change so `HEAD` and `origin/main` name different commits.
fn advance_local_head(repo: &Repository) -> String {
    fs::write(repo.main.join("local-only.txt"), "local\n").unwrap();
    git(&repo.main, ["add", "local-only.txt"]);
    git(&repo.main, ["commit", "-m", "local only"]);
    git_output(&repo.main, ["rev-parse", "HEAD"])
}

/// Publishes a commit through a second clone, leaving this clone's tracking ref stale.
fn advance_origin(repo: &Repository, origin: &Path) -> String {
    let publisher = repo.temp.path().join("publisher");
    let output = Command::new("git")
        .arg("clone")
        .arg(origin)
        .arg(&publisher)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(&publisher, ["config", "user.email", "test@example.com"]);
    git(&publisher, ["config", "user.name", "Test User"]);
    fs::write(publisher.join("published.txt"), "published\n").unwrap();
    git(&publisher, ["add", "published.txt"]);
    git(&publisher, ["commit", "-m", "published"]);
    git(&publisher, ["push", "origin", "main"]);
    git_output(&publisher, ["rev-parse", "HEAD"])
}

/// Points global configuration at `root` and appends extra `worktrees:` keys.
fn config_home_with(root: &Path, extra: &str) -> TempDir {
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n{extra}", root.display()),
    )
    .unwrap();
    xdg
}

fn write_ignored_local_config(repo: &Repository, contents: &str) {
    fs::write(repo.main.join(".gitignore"), "/.pando.local.yaml\n").unwrap();
    fs::write(repo.main.join(".pando.local.yaml"), contents).unwrap();
}

/// Returns the rail's closing beat: the message after its final closing bar.
///
/// The spinner redraws itself with cursor-movement escapes rather than newlines,
/// so the rail is read as one styled stream instead of physical lines.
fn closing_beat(stderr: &str) -> String {
    let stripped = console::strip_ansi_codes(stderr).into_owned();
    let trimmed = stripped.trim_end();
    let bar = trimmed
        .rfind('\u{2514}')
        .unwrap_or_else(|| panic!("the rail must close with a bar: {trimmed}"));
    trimmed[bar..]
        .trim_start_matches('\u{2514}')
        .trim()
        .to_owned()
}

#[test]
fn create_closes_the_rail_with_the_created_worktree_outro() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "topic/rail"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"");
    let destination = root.join("topic/rail");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", destination.canonicalize().unwrap().display())
    );
    let stderr = console::strip_ansi_codes(&output.stderr).into_owned();
    assert!(
        !stderr.contains("Worktree destination printed."),
        "the rail must not restate the stdout plumbing: {stderr}"
    );
    let closing = closing_beat(&output.stderr);
    assert!(
        closing.starts_with("Created worktree ") && closing.ends_with('s'),
        "the outro reports the outcome and keeps its elapsed suffix: {closing:?}"
    );
    assert_eq!(
        stderr.matches("Created worktree").count(),
        1,
        "the timed run still owns exactly one terminal state: {stderr}"
    );
}

#[test]
fn switch_creation_closes_the_rail_like_create() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "topic/switched"]);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "topic/switched"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"");

    assert!(output.status.success(), "{}", output.stderr);
    let stderr = console::strip_ansi_codes(&output.stderr).into_owned();
    assert!(
        !stderr.contains("Worktree destination printed."),
        "{stderr}"
    );
    let closing = closing_beat(&output.stderr);
    assert!(
        closing.starts_with("Created worktree "),
        "switch ends its creation path exactly like create: {closing:?}"
    );
    assert_eq!(stderr.matches("Created worktree").count(), 1, "{stderr}");
}

#[test]
fn create_with_post_create_commands_keeps_creation_as_a_step() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);
    fs::write(
        repo.main.join(".pando.yaml"),
        "hooks:\n  post-create:\n    - name: prepare\n      command: echo hook-marker\n",
    )
    .unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "hooked"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"y\r");

    assert!(output.status.success(), "{}", output.stderr);
    let stderr = console::strip_ansi_codes(&output.stderr).into_owned();
    let created = stderr.find("Created worktree").expect(&stderr);
    let hook = stderr.rfind("hook-marker").expect(&stderr);
    assert!(
        created < hook,
        "creation stays a mid-rail step before hook output: {stderr}"
    );
    assert_eq!(stderr.matches("Created worktree").count(), 1, "{stderr}");
    assert_eq!(
        closing_beat(&output.stderr),
        "Post-create setup complete",
        "the setup outro closes the sequence: {stderr}"
    );
    assert!(
        !stderr.contains("Worktree destination printed."),
        "{stderr}"
    );
}

#[test]
fn create_defaults_to_the_invoking_head() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let head = advance_local_head(&repo);
    let root = repo.temp.path().join("created");

    for extra in ["", "  base: head\n"] {
        let branch = format!("topic/head{}", extra.len());
        let xdg = config_home_with(&root, extra);
        let output = create_command(&repo, &xdg, &["create", &branch]);

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(git_output(&repo.main, ["rev-parse", &branch]), head);
    }
}

#[test]
fn create_fresh_uses_the_configured_target_branch_tracking_ref() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    let head = advance_local_head(&repo);
    assert_ne!(published, head);
    fs::write(
        repo.main.join(".pando.yaml"),
        "worktrees:\n  target-branch: main\n",
    )
    .unwrap();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/fresh"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/fresh"]),
        published
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("from branch \"origin/main\" at {published}")),
        "{stderr}"
    );
}

#[test]
fn create_fresh_falls_back_to_origin_head() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/inferred"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/inferred"]),
        published
    );
}

#[test]
fn create_base_resolves_local_over_shared_over_global() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    let head = advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");
    fs::write(repo.main.join(".pando.yaml"), "worktrees:\n  base: head\n").unwrap();

    let shared_wins = create_command(&repo, &xdg, &["create", "topic/shared"]);
    assert!(
        shared_wins.status.success(),
        "{}",
        String::from_utf8_lossy(&shared_wins.stderr)
    );
    assert_eq!(git_output(&repo.main, ["rev-parse", "topic/shared"]), head);

    write_ignored_local_config(&repo, "worktrees:\n  base: fresh\n");
    let local_wins = create_command(&repo, &xdg, &["create", "topic/local"]);
    assert!(
        local_wins.status.success(),
        "{}",
        String::from_utf8_lossy(&local_wins.stderr)
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/local"]),
        published
    );
}

#[test]
fn create_rejects_an_unknown_base_value() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: stale\n");

    let output = create_command(&repo, &xdg, &["create", "topic/invalid"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("pando/config.yaml"), "{stderr}");
    assert!(stderr.contains("stale"), "{stderr}");
}

#[test]
fn create_fresh_without_a_resolvable_base_fails_with_guidance() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/unresolvable"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("worktrees.target-branch"), "{stderr}");
    assert!(stderr.contains("git remote set-head origin -a"), "{stderr}");
    assert!(git_output(&repo.main, ["branch", "--list", "topic/unresolvable"]).is_empty());
}

#[test]
fn create_fresh_with_an_unfetched_tracking_ref_fails_with_guidance() {
    let repo = Repository::new();
    add_local_origin(&repo);
    fs::write(
        repo.main.join(".pando.yaml"),
        "worktrees:\n  target-branch: release\n",
    )
    .unwrap();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/unfetched"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("origin/release"), "{stderr}");
    assert!(stderr.contains("--fetch"), "{stderr}");
}

#[test]
fn create_fresh_retains_the_dirty_source_warning() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");
    fs::write(repo.main.join("dirty.txt"), "dirty\n").unwrap();

    let output = create_command(&repo, &xdg, &["create", "topic/dirty"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("remain in the source worktree")
    );
}

#[test]
fn switch_confirmation_names_the_fresh_base() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["switch", "topic/confirmed"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path());
    let output = run_pty_command(command, b"y\r");

    assert!(output.status.success(), "{}", output.stderr);
    let stderr = console::strip_ansi_codes(&output.stderr).into_owned();
    assert!(
        stderr.contains(&format!("from branch \"origin/main\" at {published}")),
        "{stderr}"
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/confirmed"]),
        published
    );
}

#[test]
fn create_dry_run_reflects_the_configured_base() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/planned", "--dry-run"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("from branch \"origin/main\" at {published}")),
        "{stderr}"
    );
    assert!(git_output(&repo.main, ["branch", "--list", "topic/planned"]).is_empty());
}

#[test]
fn fetch_refreshes_a_stale_base_ref_before_branching() {
    let repo = Repository::new();
    let origin = add_local_origin(&repo);
    let stale = git_output(&repo.main, ["rev-parse", "origin/main"]);
    let advanced = advance_origin(&repo, &origin);
    assert_ne!(stale, advanced);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    // Without the flag, fresh mode uses the tracking ref exactly as it stands.
    let offline = create_command(&repo, &xdg, &["create", "topic/offline"]);
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/offline"]),
        stale
    );

    let fetched = create_command(&repo, &xdg, &["create", "topic/fetched", "--fetch"]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/fetched"]),
        advanced
    );
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "origin/main"]),
        advanced,
        "only the resolved base ref is refreshed"
    );
}

#[test]
fn fetch_is_rejected_when_it_would_do_nothing() {
    let repo = Repository::new();
    add_local_origin(&repo);
    git(&repo.main, ["branch", "topic/local"]);
    git(&repo.main, ["push", "origin", "main:published"]);
    git(&repo.main, ["fetch", "origin"]);
    let root = repo.temp.path().join("created");
    let fresh = config_home_with(&root, "  base: fresh\n");
    let head = config_home_with(&root, "  base: head\n");

    let head_mode = create_command(&repo, &head, &["create", "topic/new", "--fetch"]);
    assert!(!head_mode.status.success());
    assert!(
        String::from_utf8(head_mode.stderr)
            .unwrap()
            .contains("worktrees.base is 'head'")
    );

    for (branch, reason) in [
        ("topic/local", "already exists locally"),
        ("published", "already has a remote-tracking ref"),
        ("feature", "already has a registered worktree"),
    ] {
        let output = create_command(&repo, &fresh, &["create", branch, "--fetch"]);
        assert!(!output.status.success(), "{branch}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(reason), "{branch}: {stderr}");
        assert!(output.stdout.is_empty(), "{branch}");
    }
}

#[test]
fn create_dry_run_reports_the_fetch_as_unattempted() {
    let repo = Repository::new();
    let origin = add_local_origin(&repo);
    let stale = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_origin(&repo, &origin);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(
        &repo,
        &xdg,
        &["create", "topic/planned", "--dry-run", "--fetch"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Would fetch origin/main"), "{stderr}");
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "origin/main"]),
        stale,
        "a dry run must not refresh the tracking ref"
    );
}

#[test]
fn json_create_bases_a_new_branch_on_the_fresh_tracking_ref() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/json", "--output", "json"]);

    let value = assert_json_pure(&output);
    assert!(output.status.success());
    assert_eq!(value["result"]["outcome"], "created");
    assert_eq!(value["result"]["kind"], "new");
    assert_eq!(value["result"]["start_point"], published);
    assert_eq!(value["result"]["base_ref"], "origin/main");
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/json"]),
        published
    );
}

#[test]
fn json_switch_dry_run_reports_the_fresh_base_and_an_unattempted_fetch() {
    let repo = Repository::new();
    let origin = add_local_origin(&repo);
    let stale = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_origin(&repo, &origin);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(
        &repo,
        &xdg,
        &[
            "switch",
            "topic/json",
            "--dry-run",
            "--fetch",
            "--output",
            "json",
        ],
    );

    let value = assert_json_pure(&output);
    assert!(output.status.success());
    assert_eq!(value["result"]["outcome"], "creation_plan");
    assert_eq!(value["result"]["approval_required"], true);
    assert_eq!(value["result"]["start_point"], stale);
    assert_eq!(value["result"]["base_ref"], "origin/main");
    let fetch = value["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|effect| effect["action"] == "fetch_base_ref")
        .expect("the plan names the fetch it would run");
    assert_eq!(fetch["attempted"], false);
    assert_eq!(fetch["completed"], false);
    assert_eq!(fetch["details"]["ref"], "origin/main");
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "origin/main"]),
        stale,
        "a dry run must not refresh the tracking ref"
    );
}

#[test]
fn json_reports_an_inapplicable_fetch_as_a_typed_error() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: head\n");

    let output = create_command(
        &repo,
        &xdg,
        &["create", "topic/json", "--fetch", "--output", "json"],
    );

    let value = assert_json_pure(&output);
    assert!(!output.status.success());
    assert_eq!(value["error"]["code"], "create.fetch_not_applicable");
    assert!(git_output(&repo.main, ["branch", "--list", "topic/json"]).is_empty());
}

#[test]
fn json_request_mode_create_accepts_the_fetch_option() {
    let repo = Repository::new();
    let origin = add_local_origin(&repo);
    let advanced = advance_origin(&repo, &origin);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let mut command = Command::cargo_bin("pando").unwrap();
    command
        .args(["create", "--input-output", "json"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", repo.temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let request = serde_json::json!({
        "schema_version": 1,
        "request_id": "create-fetch-1",
        "input": {"branch": "topic/requested", "fetch": true},
    });
    let mut stdin = child.stdin.take().unwrap();
    serde_json::to_writer(&mut stdin, &request).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();

    let value = assert_json_pure(&output);
    assert!(output.status.success());
    assert_eq!(value["request_id"], "create-fetch-1");
    assert_eq!(value["result"]["start_point"], advanced);
    assert_eq!(value["result"]["base_ref"], "origin/main");
    let fetch = value["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|effect| effect["action"] == "fetch_base_ref")
        .expect("the response reports the fetch it ran");
    assert_eq!(fetch["attempted"], true);
    assert_eq!(fetch["completed"], true);
    assert_eq!(
        git_output(&repo.main, ["rev-parse", "topic/requested"]),
        advanced
    );
}

#[test]
fn json_reports_an_unresolvable_fresh_base_as_a_typed_error() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["create", "topic/json", "--output", "json"]);

    let value = assert_json_pure(&output);
    assert!(!output.status.success());
    assert_eq!(value["error"]["code"], "create.base_unavailable");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("worktrees.target-branch"), "{message}");
    assert!(
        message.contains("git remote set-head origin -a"),
        "{message}"
    );
    assert!(git_output(&repo.main, ["branch", "--list", "topic/json"]).is_empty());
}

#[test]
fn dry_run_previews_a_multi_remote_branch_without_prompting() {
    let repo = Repository::new();
    let root = repo.temp.path().join("created");
    let xdg = config_home_with_root(&root);
    for remote in ["alpha", "beta"] {
        let bare = repo.temp.path().join(format!("{remote}.git"));
        let output = Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        assert!(output.status.success());
        git(
            &repo.main,
            ["remote", "add", remote, bare.to_str().unwrap()],
        );
        git(&repo.main, ["push", remote, "main:shared"]);
    }
    git(&repo.main, ["fetch", "--all"]);

    // No PTY: resolving the remote choice here would fail the interactivity
    // preflight, and a preview has no reason to make that choice at all.
    let output = create_command(&repo, &xdg, &["switch", "shared", "--dry-run"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Would create a worktree for shared")
    );
}

#[test]
fn switch_dry_run_names_the_fresh_base_on_the_human_rail() {
    let repo = Repository::new();
    add_local_origin(&repo);
    let published = git_output(&repo.main, ["rev-parse", "origin/main"]);
    advance_local_head(&repo);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    let output = create_command(&repo, &xdg, &["switch", "topic/preview", "--dry-run"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("from branch \"origin/main\" at {published}")),
        "{stderr}"
    );
}

#[test]
fn json_rejects_a_fetch_for_a_branch_that_is_not_genuinely_new() {
    let repo = Repository::new();
    add_local_origin(&repo);
    git(&repo.main, ["branch", "topic/local"]);
    let root = repo.temp.path().join("created");
    let xdg = config_home_with(&root, "  base: fresh\n");

    for (branch, reason) in [
        ("topic/local", "already exists locally"),
        ("feature", "already has a registered worktree"),
    ] {
        let output = create_command(
            &repo,
            &xdg,
            &["create", branch, "--fetch", "--output", "json"],
        );
        let value = assert_json_pure(&output);
        assert!(!output.status.success(), "{branch}");
        assert_eq!(value["error"]["code"], "create.fetch_not_applicable");
        assert!(
            value["error"]["message"].as_str().unwrap().contains(reason),
            "{branch}: {value}"
        );
    }
}

#[test]
fn json_help_exposes_create_description_and_shared_base_inputs() {
    let repo = Repository::new();

    for command in ["switch", "create"] {
        let output = json_command(&repo.main, &[command, "--help", "--output", "json"], None);
        let value = assert_json_pure(&output);
        let schema = &value["result"]["request_schema"];
        let input_type = schema["properties"]["input"]["$ref"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap();
        let properties = schema["definitions"][input_type]["properties"]
            .as_object()
            .unwrap();
        assert!(properties.contains_key("fetch"), "{command}: {schema}");
        assert!(properties.contains_key("branch"), "{command}: {schema}");
        assert!(properties.contains_key("dry_run"), "{command}: {schema}");
        assert_eq!(properties.contains_key("description"), command == "create");
        let errors = serde_json::to_string(&value["result"]["error_codes"]).unwrap();
        assert!(
            errors.contains(&format!("{command}.fetch_not_applicable")),
            "{command}: {errors}"
        );
        assert!(
            errors.contains(&format!("{command}.base_unavailable")),
            "{command}: {errors}"
        );
        let actions = serde_json::to_string(&value["result"]["actions"]).unwrap();
        assert_eq!(
            actions.contains("set_branch_description"),
            command == "create"
        );
        assert_eq!(
            errors.contains("create.description_failed"),
            command == "create"
        );
    }
}

#[test]
fn json_switch_rejects_the_create_only_description_field() {
    let repo = Repository::new();
    let request = serde_json::json!({
        "schema_version": 1,
        "input": {"branch": "feature", "description": "Not valid for switch"}
    });

    let output = json_command(
        &repo.main,
        &["switch", "--input-output", "json"],
        Some(&request),
    );

    assert!(!output.status.success());
    assert_eq!(
        assert_json_pure(&output)["error"]["code"],
        "json.invalid_request"
    );
}

#[test]
fn complete_env_emits_a_zsh_registration_script() {
    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command.env("_PANDO_COMPLETE", "zsh").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("#compdef pando"));
    assert!(stdout.contains("compdef _clap_dynamic_completer_pando pando"));
}

#[test]
fn complete_env_completes_subcommands() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", ""]);

    assert!(candidates.iter().any(|value| value == "switch"));
    assert!(candidates.iter().any(|value| value == "create"));
    assert!(candidates.iter().any(|value| value == "remove"));
}

#[test]
fn complete_env_completes_flags() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", "--out"]);

    assert!(candidates.iter().any(|value| value == "--output"));
}

#[test]
fn complete_env_completes_get_properties() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", "get", ""]);

    assert!(candidates.iter().any(|value| value == "branch"));
    assert!(candidates.iter().any(|value| value == "port"));
    assert!(candidates.iter().any(|value| value == "worktree-path"));
}

/// `COMPLETE` is generic enough to be set in a shell for unrelated reasons.
/// `clap_complete`'s default trigger variable name is exactly that, so the
/// binary is configured to listen on `_PANDO_COMPLETE` instead. A stray
/// `COMPLETE` in the environment must leave `pando list` behaving
/// normally rather than emitting a completion script on stdout, which the
/// installed zsh dispatcher would otherwise `cd` into for `switch`.
#[test]
fn stray_complete_env_var_does_not_trigger_completion() {
    let repo = Repository::new();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["list"])
        .current_dir(&repo.main)
        .env("COMPLETE", "bash")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("complete") && !stdout.contains("compdef"),
        "a stray COMPLETE env var must not emit a completion script: {stdout}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Worktrees"));
}

/// Drives the `_PANDO_COMPLETE=zsh` protocol and returns candidate values
/// with any `:help` suffix stripped. The final entry of `words` is the word
/// being completed, matching how zsh passes `${words[@]}`.
fn complete(dir: &Path, words: &[&str]) -> Vec<String> {
    let index = words.len() - 1;
    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command
        .env("_PANDO_COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .arg("--")
        .args(words)
        .current_dir(dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "completion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once(':')
                .map_or(line, |(value, _)| value)
                .to_owned()
        })
        .collect()
}

#[test]
fn switch_completes_local_and_remote_branches() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);
    // A remote-tracking ref without a matching local branch. Created directly
    // so the test needs no network and no second repository.
    git(
        &repo.main,
        ["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
    );

    let candidates = complete(&repo.main, &["pando", "switch", ""]);

    assert!(candidates.iter().any(|value| value == "main"));
    assert!(candidates.iter().any(|value| value == "feature"));
    assert!(candidates.iter().any(|value| value == "solo"));
    // The candidate value is the short branch name, not `origin/remote-only`:
    // `smart::resolve_and_switch` probes `refs/remotes/origin/{branch}`, so a
    // remote-qualified value would never match and would instead create a new
    // local branch literally named `origin/remote-only`.
    assert!(candidates.iter().any(|value| value == "remote-only"));
    assert!(!candidates.iter().any(|value| value == "origin/remote-only"));
}

#[test]
fn switch_hides_remote_refs_that_shadow_a_local_branch() {
    let repo = Repository::new();
    git(
        &repo.main,
        ["update-ref", "refs/remotes/origin/main", "HEAD"],
    );

    let candidates = complete(&repo.main, &["pando", "switch", ""]);

    assert!(candidates.iter().any(|value| value == "main"));
    assert!(
        !candidates.iter().any(|value| value == "origin/main"),
        "a remote ref shadowing a local branch is redundant: {candidates:?}"
    );
}

#[test]
fn switch_deduplicates_a_short_name_offered_by_multiple_remotes() {
    let repo = Repository::new();
    git(
        &repo.main,
        ["update-ref", "refs/remotes/origin/shared", "HEAD"],
    );
    git(
        &repo.main,
        ["update-ref", "refs/remotes/upstream/shared", "HEAD"],
    );

    let candidates = complete(&repo.main, &["pando", "switch", ""]);

    assert_eq!(
        candidates.iter().filter(|value| *value == "shared").count(),
        1,
        "a branch offered by two remotes must appear once: {candidates:?}"
    );
}

/// Proves the completed value actually resolves, not just that it looks
/// right: a repo whose only remote ref is `refs/remotes/origin/remote-only`
/// must let `pando create remote-only` track that remote branch, rather
/// than creating a literal `origin/remote-only` local branch with no upstream.
#[test]
fn create_resolves_the_completed_remote_branch_value_to_its_remote() {
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
    git(&repo.main, ["branch", "remote-only"]);
    git(&repo.main, ["push", "origin", "remote-only"]);
    git(&repo.main, ["branch", "-D", "remote-only"]);

    let candidates = complete(&repo.main, &["pando", "switch", ""]);
    let value = candidates
        .iter()
        .find(|value| value.as_str() != "main" && value.as_str() != "feature")
        .expect("the remote branch should be offered as a candidate");
    assert_eq!(value, "remote-only");

    let xdg = tempfile::tempdir().unwrap();
    let root = repo.temp.path().join("created");
    fs::create_dir_all(xdg.path().join("pando")).unwrap();
    fs::write(
        xdg.path().join("pando/config.yaml"),
        format!("worktrees:\n  root: {}\n", root.display()),
    )
    .unwrap();

    let output = Command::cargo_bin("pando")
        .unwrap()
        .args(["create", value])
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

    let destination = root.join("remote-only");
    assert!(destination.exists());
    assert_eq!(
        git_output(&destination, ["rev-parse", "--abbrev-ref", "HEAD"]),
        "remote-only"
    );
    assert_eq!(
        git_output(
            &destination,
            [
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}"
            ]
        ),
        "origin/remote-only"
    );
}

#[test]
fn create_excludes_branches_that_already_have_a_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);

    let candidates = complete(&repo.main, &["pando", "create", ""]);

    assert!(candidates.iter().any(|value| value == "solo"));
    assert!(
        !candidates.iter().any(|value| value == "feature"),
        "feature already has a worktree: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|value| value == "main"),
        "main is the primary worktree: {candidates:?}"
    );
}

#[test]
fn remove_offers_only_branches_with_a_topic_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);

    let candidates = complete(&repo.main, &["pando", "remove", ""]);

    assert!(candidates.iter().any(|value| value == "feature"));
    assert!(
        !candidates.iter().any(|value| value == "solo"),
        "solo has no worktree to remove: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|value| value == "main"),
        "the primary worktree is not removable: {candidates:?}"
    );
}

#[test]
fn branch_completion_filters_by_prefix() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "feat-a"]);
    git(&repo.main, ["branch", "other"]);

    let candidates = complete(&repo.main, &["pando", "switch", "feat"]);

    assert!(candidates.iter().any(|value| value == "feat-a"));
    assert!(!candidates.iter().any(|value| value == "other"));
}

#[test]
fn branch_completion_outside_a_repository_is_silent() {
    let outside = tempfile::tempdir().unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command
        .env("_PANDO_COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .arg("--")
        .args(["pando", "switch", ""])
        .current_dir(outside.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "a completion widget must never print diagnostics: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.lines().any(|line| line.contains("error")),
        "no error text may reach the completion line: {stdout}"
    );
}

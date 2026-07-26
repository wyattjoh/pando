use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use nix::{pty::openpty, unistd::dup};

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

#[test]
fn list_shows_current_repository_worktrees_from_nested_directory() {
    let repo = Repository::new();
    let nested = repo.main.join("nested");
    fs::create_dir(&nested).unwrap();

    let mut command = Command::cargo_bin("worktrees").unwrap();
    let output = command.arg("list").current_dir(&nested).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().next().unwrap().contains("BRANCH"));
    assert!(stdout.lines().next().unwrap().contains("STATE"));
    assert!(stdout.lines().next().unwrap().contains("PATH"));
    assert!(stdout.contains("* main"));
    assert!(stdout.contains(repo.main.to_str().unwrap()));
    assert!(stdout.contains("feature"));
    assert!(stdout.contains(repo.linked.to_str().unwrap()));
    assert!(
        stdout.find(repo.main.to_str().unwrap()).unwrap()
            < stdout.find(repo.linked.to_str().unwrap()).unwrap(),
        "{stdout}"
    );
    assert!(!stdout.contains("clean"));
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("* trailing-branch")),
        "{stdout}"
    );

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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("dirty").count(), 3, "{stdout}");
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("(detached)"), "{stdout}");
    assert!(stdout.contains("locked: maintenance"), "{stdout}");
    assert!(stdout.contains("missing"), "{stdout}");
    assert!(stdout.contains("prunable"), "{stdout}");

    let bare_output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("list")
        .current_dir(&bare_linked)
        .output()
        .unwrap();
    let bare_stdout = String::from_utf8(bare_output.stdout).unwrap();
    assert!(bare_stdout.contains("(bare)"), "{bare_stdout}");
    assert!(bare_stdout.contains("bare"), "{bare_stdout}");
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
    let stdout = String::from_utf8(listed.stdout).unwrap();
    assert!(stdout.contains("inaccessible"), "{stdout}");
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("unknown").count(), 2, "{stdout}");
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
    assert!(output.stderr.contains("main"), "{}", output.stderr);
    assert!(output.stderr.contains("feature"), "{}", output.stderr);
}

#[test]
fn switch_moves_with_arrow_keys_and_enter() {
    let repo = Repository::new();

    let output = run_switch(&repo.main, b"\x1b[B\r");

    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display())
    );
}

#[test]
fn switch_omits_missing_and_bare_records() {
    let repo = Repository::new();
    let missing = repo.add_worktree("missing-switch", "missing-switch-branch");
    fs::remove_dir_all(&missing).unwrap();

    let output = run_switch(&repo.main, b"\x1b[B\r");
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
fn install_decline_makes_no_filesystem_changes() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();
    let zshrc = zdot.path().join(".zshrc");
    fs::write(&zshrc, b"export KEEP=yes\n").unwrap();

    let output = run_install(home.path(), xdg.path(), Some(zdot.path()), b"n\r");
    assert!(output.status.success(), "{}", output.stderr);
    assert!(output.stdout.contains("cancelled"), "{}", output.stdout);

    assert_eq!(fs::read(&zshrc).unwrap(), b"export KEEP=yes\n");
    assert!(!xdg.path().join("worktrees/worktrees.zsh").exists());
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
    assert!(
        installed.stdout.contains("Installed zsh integration"),
        "{}",
        installed.stdout
    );

    let integration = xdg.path().join("worktrees/worktrees.zsh");
    let generated = fs::read_to_string(&integration).unwrap();
    assert!(generated.contains("worktrees()"));
    assert!(generated.contains("builtin cd -- \"$destination\""));
    assert!(generated.contains("command worktrees \"$@\""));
    assert!(generated.contains("# wt()"));
    assert!(!generated.lines().any(|line| line.starts_with("wt()")));

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
    assert!(
        String::from_utf8(current.stdout)
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
fn installed_zsh_function_changes_the_invoking_shell_directory() {
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
        "source {}; worktrees switch || exit $?; builtin pwd -P",
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
        b"\x1b[B\r",
    );
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("{}\n", repo.linked.canonicalize().unwrap().display())
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
    assert!(output.stderr.is_empty());
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
            "main-worktree-path",
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
        assert!(output.stderr.is_empty(), "{property}");
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
fn picker_branch_action_uses_the_shared_resolver_and_escape_cancels() {
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

    let created = run(b"\x1b[B\x1b[B\rpicker-existing\r");
    assert!(created.status.success(), "{}", created.stderr);
    assert!(root.join("picker-existing").exists());

    let cancelled = run(b"\x1b[B\x1b[B\x1b[B\r\x1b");
    assert!(!cancelled.status.success());
    assert!(cancelled.stdout.is_empty());
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

    let retried = run_pty_command(retry, b"\x1b[B\r\r");

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
            "{}\n",
            root.join("zsh-hook").canonicalize().unwrap().display()
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
    assert!(
        String::from_utf8(untrusted.stdout)
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
    assert!(
        String::from_utf8(trusted.stdout)
            .unwrap()
            .contains("are trusted")
    );

    let reset = command(&["trust", "reset"]);
    assert!(String::from_utf8(reset.stdout).unwrap().contains("Reset"));
    let reset_again = command(&["trust", "reset"]);
    assert!(
        String::from_utf8(reset_again.stdout)
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
    run_pty_command(install_command(home, xdg, zdotdir), input)
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

fn run_switch(cwd: &Path, input: &[u8]) -> PtyOutput {
    let mut command = Command::cargo_bin("worktrees").unwrap();
    command.arg("switch").current_dir(cwd);
    run_pty_command(command, input)
}

fn run_pty_command(mut command: Command, input: &[u8]) -> PtyOutput {
    let pty = openpty(None, None).unwrap();
    let stdin_fd = dup(&pty.slave).unwrap();
    let stderr_fd = dup(&pty.slave).unwrap();
    let mut master_writer = fs::File::from(dup(&pty.master).unwrap());
    let mut master_reader = fs::File::from(pty.master);

    let mut child = command
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_fd))
        .spawn()
        .unwrap();
    drop(command);
    drop(pty.slave);

    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = master_reader.read_to_end(&mut bytes);
        bytes
    });
    master_writer.write_all(input).unwrap();
    master_writer.flush().unwrap();
    drop(master_writer);

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
        .args(["commit", "-m", "feat: commit every change"])
        .current_dir(&repo.main)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("feat: commit every change"));
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
fn commit_generates_message_from_global_configuration() {
    let repo = Repository::new();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("worktrees")).unwrap();
    fs::write(
        xdg.path().join("worktrees/config.yaml"),
        "commit:\n  generation:\n    command: \"printf 'feat: generated\\n\\n- first change\\n- second change\\n'\"\n",
    ).unwrap();
    fs::write(repo.main.join("generated.txt"), "content\n").unwrap();

    let output = Command::cargo_bin("worktrees")
        .unwrap()
        .arg("commit")
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Staging changes…"), "{stderr}");
    assert!(stderr.contains("Generating commit message…"), "{stderr}");
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
    assert_eq!(
        String::from_utf8(absent.stdout).unwrap(),
        "No commit generator is configured.\n"
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
    assert_eq!(
        String::from_utf8(controlled.stdout).unwrap(),
        "The effective commit generator is user-controlled.\n"
    );

    let reset = Command::cargo_bin("worktrees")
        .unwrap()
        .args(["trust", "commit-reset"])
        .current_dir(&repo.main)
        .env("XDG_CONFIG_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(reset.status.success());
    assert_eq!(
        String::from_utf8(reset.stdout).unwrap(),
        "No saved commit generator trust existed for this repository.\n"
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
        .arg("commit")
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

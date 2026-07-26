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

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

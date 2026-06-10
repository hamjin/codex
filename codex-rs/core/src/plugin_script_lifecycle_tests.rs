use std::fs;
use std::path::PathBuf;

use codex_core_skills::SkillMetadata;
use codex_protocol::protocol::SkillScope;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn command_path(parts: &[&str]) -> String {
    let mut path = PathBuf::new();
    path.extend(parts);
    path.to_string_lossy().into_owned()
}

fn fixture() -> (TempDir, AbsolutePathBuf, Vec<FirstPartyPluginRoot>) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = AbsolutePathBuf::try_from(temp.path().join("plugin")).expect("absolute path");
    fs::create_dir_all(root.join("skills/demo/scripts")).expect("create scripts directory");
    fs::write(root.join("skills/demo/SKILL.md"), "# Demo").expect("write skill");
    fs::write(root.join("skills/demo/scripts/run.py"), "print('ok')").expect("write script");
    let roots = vec![FirstPartyPluginRoot {
        plugin_id: "openai/demo".to_string(),
        plugin_root: root.clone(),
    }];
    (temp, root, roots)
}

fn skill_outcome(root: &AbsolutePathBuf) -> SkillLoadOutcome {
    let mut outcome = SkillLoadOutcome::default();
    outcome.skills = vec![SkillMetadata {
        name: "demo".to_string(),
        description: String::new(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: root.join("skills/demo/SKILL.md"),
        scope: SkillScope::User,
        plugin_id: Some("openai/demo".to_string()),
    }];
    outcome
}

fn resolve(
    roots: &[FirstPartyPluginRoot],
    skills: &SkillLoadOutcome,
    command: &str,
    cwd: &AbsolutePathBuf,
) -> Option<ResolvedPluginScript> {
    resolve_plugin_script(roots, skills, command, cwd, ShellType::Bash)
}

#[test]
fn resolves_interpreter_script_to_plugin_relative_path_and_skill() {
    let (_temp, root, roots) = fixture();
    let script = command_path(&["skills", "demo", "scripts", "run.py"]);
    let resolved = resolve(
        &roots,
        &skill_outcome(&root),
        &format!("python {script} --secret argument"),
        &root,
    )
    .expect("plugin script");

    assert_eq!(resolved.plugin_id, "openai/demo");
    assert_eq!(resolved.script_path, "skills/demo/scripts/run.py");
    assert_eq!(resolved.skill.expect("skill").skill_name, "demo");
}

#[test]
fn resolves_direct_executable_without_a_known_extension() {
    let (_temp, root, roots) = fixture();
    fs::create_dir_all(root.join("bin")).expect("create bin");
    fs::write(root.join("bin/run"), "#!/bin/sh\n").expect("write executable");

    let command = command_path(&[".", "bin", "run"]);
    let resolved =
        resolve(&roots, &SkillLoadOutcome::default(), &command, &root).expect("plugin script");

    assert_eq!(resolved.script_path, "bin/run");
    assert!(resolved.skill.is_none());
}

#[test]
fn rejects_non_plugin_and_symlink_escape_paths() {
    let (temp, root, roots) = fixture();
    let outside = AbsolutePathBuf::try_from(temp.path().join("outside.py")).expect("absolute path");
    fs::write(&outside, "print('outside')").expect("write outside script");

    assert!(
        resolve(
            &roots,
            &SkillLoadOutcome::default(),
            outside.to_string_lossy().as_ref(),
            &root,
        )
        .is_none()
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("escape.py")).expect("create symlink");
        assert!(
            resolve(
                &roots,
                &SkillLoadOutcome::default(),
                "python escape.py",
                &root,
            )
            .is_none()
        );
    }
}

#[test]
fn resolves_node_and_shell_scripts() {
    let (_temp, root, roots) = fixture();
    fs::write(root.join("skills/demo/scripts/run.js"), "console.log('ok')")
        .expect("write node script");
    fs::write(root.join("skills/demo/scripts/run.sh"), "echo ok").expect("write shell script");

    for (command, expected) in [
        (
            "node skills/demo/scripts/run.js",
            "skills/demo/scripts/run.js",
        ),
        (
            "sh skills/demo/scripts/run.sh",
            "skills/demo/scripts/run.sh",
        ),
    ] {
        let resolved =
            resolve(&roots, &SkillLoadOutcome::default(), command, &root).expect("plugin script");
        assert_eq!(resolved.script_path, expected);
    }
}

#[test]
fn rejects_compound_commands_and_runner_options() {
    let (_temp, root, roots) = fixture();

    for command in [
        "python skills/demo/scripts/run.py && python skills/demo/scripts/run.py",
        "python -c skills/demo/scripts/run.py",
        "python --help skills/demo/scripts/run.py",
        "node --loader skills/demo/scripts/loader.js skills/demo/scripts/run.js",
        "env -C skills/demo python scripts/run.py",
    ] {
        assert!(
            resolve(&roots, &SkillLoadOutcome::default(), command, &root).is_none(),
            "unexpected lifecycle attribution for {command}"
        );
    }
}

#[test]
#[cfg(not(windows))]
fn direct_executable_matching_interpreter_name_is_case_sensitive() {
    let (_temp, root, roots) = fixture();
    fs::write(root.join("Python"), "#!/bin/sh\n").expect("write case-sensitive executable");

    let resolved = resolve(&roots, &SkillLoadOutcome::default(), "./Python", &root)
        .expect("case-sensitive direct executable");
    assert_eq!(resolved.script_path, "Python");
}

#[test]
fn powershell_split_preserves_paths_and_rejects_compounds() {
    assert_eq!(
        command_tokens(
            r#"pwsh.exe -File C:\Users\me\plugin\scripts\run.ps1"#,
            ShellType::PowerShell,
        ),
        Some(vec![
            "pwsh.exe".to_string(),
            "-File".to_string(),
            r#"C:\Users\me\plugin\scripts\run.ps1"#.to_string(),
        ])
    );
    assert_eq!(
        command_tokens(
            r#"& 'C:\Program Files\plugin\scripts\run.ps1'"#,
            ShellType::PowerShell,
        ),
        Some(vec![
            r#"C:\Program Files\plugin\scripts\run.ps1"#.to_string()
        ])
    );
    assert!(command_tokens("python a.py; python b.py", ShellType::PowerShell).is_none());
}

use std::fs;

use codex_core_skills::SkillMetadata;
use codex_protocol::protocol::SkillScope;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

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

#[test]
fn resolves_interpreter_script_to_plugin_relative_path_and_skill() {
    let (_temp, root, roots) = fixture();
    let resolved = resolve_plugin_script(
        &roots,
        &skill_outcome(&root),
        "python skills/demo/scripts/run.py --secret argument",
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

    let resolved = resolve_plugin_script(&roots, &SkillLoadOutcome::default(), "./bin/run", &root)
        .expect("plugin script");

    assert_eq!(resolved.script_path, "bin/run");
    assert!(resolved.skill.is_none());
}

#[test]
fn rejects_non_plugin_and_symlink_escape_paths() {
    let (temp, root, roots) = fixture();
    let outside = AbsolutePathBuf::try_from(temp.path().join("outside.py")).expect("absolute path");
    fs::write(&outside, "print('outside')").expect("write outside script");

    assert!(
        resolve_plugin_script(
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
            resolve_plugin_script(
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
fn rejects_compound_shell_commands() {
    let (_temp, root, roots) = fixture();

    assert!(
        resolve_plugin_script(
            &roots,
            &SkillLoadOutcome::default(),
            "python skills/demo/scripts/run.py && python skills/demo/scripts/run.py",
            &root,
        )
        .is_none()
    );
}

#[test]
fn resolves_single_script_with_environment_and_redirection() {
    let (_temp, root, roots) = fixture();
    let resolved = resolve_plugin_script(
        &roots,
        &SkillLoadOutcome::default(),
        "PLUGIN_MODE=test python skills/demo/scripts/run.py > output.log",
        &root,
    )
    .expect("plugin script");

    assert_eq!(resolved.script_path, "skills/demo/scripts/run.py");
    let env_resolved = resolve_plugin_script(
        &roots,
        &SkillLoadOutcome::default(),
        "env PLUGIN_MODE=test python skills/demo/scripts/run.py",
        &root,
    )
    .expect("env-prefixed plugin script");
    assert_eq!(env_resolved.script_path, "skills/demo/scripts/run.py");
    assert!(!has_unquoted_compound_operator(
        "python skills/demo/scripts/run.py 'literal;argument'"
    ));
}

#[test]
fn parses_runner_subcommands_and_option_arguments_without_false_attribution() {
    let (_temp, root, roots) = fixture();
    fs::write(root.join("skills/demo/scripts/run.ts"), "console.log('ok')")
        .expect("write deno script");
    fs::write(root.join("skills/demo/scripts/loader.js"), "export {}").expect("write node loader");
    fs::write(root.join("skills/demo/scripts/run.js"), "console.log('ok')")
        .expect("write node script");

    let deno = resolve_plugin_script(
        &roots,
        &SkillLoadOutcome::default(),
        "deno run --allow-read skills/demo/scripts/run.ts",
        &root,
    )
    .expect("deno plugin script");
    assert_eq!(deno.script_path, "skills/demo/scripts/run.ts");

    let node = resolve_plugin_script(
        &roots,
        &SkillLoadOutcome::default(),
        "node --loader skills/demo/scripts/loader.js skills/demo/scripts/run.js",
        &root,
    )
    .expect("node plugin script");
    assert_eq!(node.script_path, "skills/demo/scripts/run.js");

    assert!(
        resolve_plugin_script(
            &roots,
            &SkillLoadOutcome::default(),
            "python -c skills/demo/scripts/run.py",
            &root,
        )
        .is_none()
    );

    for command in [
        "python --help skills/demo/scripts/run.py",
        "python - skills/demo/scripts/run.py",
        "bash -s skills/demo/scripts/run.py",
        "env -S 'python -c' skills/demo/scripts/run.py",
        "env -C skills/demo python scripts/run.py",
        "pwsh -WorkingDirectory skills/demo -File scripts/run.ps1",
    ] {
        assert!(
            resolve_plugin_script(&roots, &SkillLoadOutcome::default(), command, &root,).is_none(),
            "unexpected lifecycle attribution for {command}"
        );
    }

    #[cfg(not(windows))]
    {
        fs::write(root.join("Python"), "#!/bin/sh\n").expect("write case-sensitive executable");
        let direct = resolve_plugin_script(&roots, &SkillLoadOutcome::default(), "./Python", &root)
            .expect("case-sensitive direct executable");
        assert_eq!(direct.script_path, "Python");
    }
}

#[test]
fn windows_command_split_preserves_paths_and_rejects_compounds() {
    assert_eq!(
        split_windows_command(r#"pwsh.exe -File C:\Users\me\plugin\scripts\run.ps1"#),
        Some(vec![
            "pwsh.exe".to_string(),
            "-File".to_string(),
            r#"C:\Users\me\plugin\scripts\run.ps1"#.to_string(),
        ])
    );
    assert_eq!(
        split_windows_command(r#"& 'C:\Program Files\plugin\scripts\run.ps1'"#),
        Some(vec![
            r#"C:\Program Files\plugin\scripts\run.ps1"#.to_string()
        ])
    );
    assert!(split_windows_command("python a.py; python b.py").is_none());
}

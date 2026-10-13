use super::*;
use serde_json::Value;

fn opts(bin: &str) -> Options {
    Options {
        dry_run: false,
        uninstall: false,
        only: None,
        bin: bin.into(),
        no_skill: true,
        force: false,
        codex_home: None,
    }
}

#[test]
fn installs_into_a_detected_agent_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    // Pretend Cursor is installed, with a pre-existing unrelated server.
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::write(
        home.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"other":{"command":"x"}}}"#,
    )
    .unwrap();

    let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
    let cursor = acts.iter().find(|a| a.agent == "cursor").unwrap();
    assert_eq!(cursor.action, "installed", "{:?}", cursor.note);

    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap())
            .unwrap();
    // codeq added, the existing server preserved.
    assert_eq!(cfg["mcpServers"]["codeq"]["command"], "/usr/bin/codeq");
    assert_eq!(cfg["mcpServers"]["codeq"]["args"][0], "mcp");
    assert_eq!(cfg["mcpServers"]["other"]["command"], "x");

    // Re-running is idempotent.
    let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
    assert_eq!(
        acts.iter().find(|a| a.agent == "cursor").unwrap().action,
        "already-current"
    );
}

#[test]
fn skips_undetected_agents_and_uninstall_removes_only_codeq() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    // Nothing installed: every agent is skipped, nothing is written.
    let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
    assert!(acts.iter().all(|a| a.action == "skipped"), "{acts:?}");
    assert!(
        !home.join(".cursor/mcp.json").exists(),
        "must not create config for undetected agent"
    );

    // Target claude-code explicitly (creates its config), then uninstall.
    std::fs::write(home.join(".claude.json"), "{}").unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("claude-code".into());
    apply(&home, &o).unwrap();
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(cfg["mcpServers"]["codeq"]["command"], "/usr/bin/codeq");

    o.uninstall = true;
    let acts = apply(&home, &o).unwrap();
    assert_eq!(acts[0].action, "removed");
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert!(
        cfg["mcpServers"].get("codeq").is_none(),
        "codeq should be gone"
    );
}

#[test]
fn installs_the_skill_beside_the_mcp_entry_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let dest = home.join(".claude/skills/codeq/SKILL.md");

    let mut o = opts("/usr/bin/codeq");
    o.no_skill = false;
    let acts = apply(&home, &o).unwrap();
    let skill = acts.iter().find(|a| a.what == "skill").unwrap();
    assert_eq!(skill.action, "installed", "{:?}", skill.note);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), SKILL);

    // Re-running changes nothing.
    let acts = apply(&home, &o).unwrap();
    assert_eq!(
        acts.iter().find(|a| a.what == "skill").unwrap().action,
        "already-current"
    );

    // An out-of-date copy is refreshed, so the guide tracks the binary.
    std::fs::write(&dest, "stale").unwrap();
    o.force = true;
    let acts = apply(&home, &o).unwrap();
    assert_eq!(
        acts.iter().find(|a| a.what == "skill").unwrap().action,
        "updated"
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), SKILL);

    // --no-skill leaves it out of the run entirely.
    o.no_skill = true;
    assert!(apply(&home, &o).unwrap().iter().all(|a| a.what == "mcp"));
}

#[cfg(unix)]
#[test]
fn a_symlinked_skill_is_never_written_through_or_deleted() {
    // Linking `skills/codeq` at the *directory* is how a codeq developer
    // wires up their checkout. Writing the embedded copy to
    // `skills/codeq/SKILL.md` would follow that link and edit the
    // repository; removing it would delete the repository's file.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let checkout = home.join("checkout/.claude/skills/codeq");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join("SKILL.md"), "my working copy").unwrap();
    std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&checkout, home.join(".claude/skills/codeq")).unwrap();

    let mut o = opts("/usr/bin/codeq");
    o.no_skill = false;
    let acts = apply(&home, &o).unwrap();
    let skill = acts.iter().find(|a| a.what == "skill").unwrap();
    assert_eq!(skill.action, "preserved", "{skill:?}");
    assert!(
        skill.note.as_deref().is_some_and(|n| n.contains("symlink")),
        "{skill:?}"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("SKILL.md")).unwrap(),
        "my working copy",
        "install wrote through the symlink into the checkout"
    );

    o.uninstall = true;
    let acts = apply(&home, &o).unwrap();
    assert_eq!(
        acts.iter().find(|a| a.what == "skill").unwrap().action,
        "preserved"
    );
    assert!(
        checkout.join("SKILL.md").exists(),
        "uninstall deleted the checkout's file through the symlink"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(home.join(".gemini/settings.json"), "{}").unwrap();

    let mut o = opts("/usr/bin/codeq");
    o.dry_run = true;
    let acts = apply(&home, &o).unwrap();
    assert_eq!(
        acts.iter().find(|a| a.agent == "gemini").unwrap().action,
        "would-install"
    );
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap())
            .unwrap();
    assert!(cfg.get("mcpServers").is_none(), "dry-run must not write");
}

fn successful(actions: &[Action]) {
    assert!(!actions.iter().any(Action::failed), "{actions:?}");
}

#[test]
fn toml_install_is_idempotent_and_preserves_comments_and_other_servers() {
    let home = tempfile::tempdir().unwrap();
    let custom = home.path().join("custom-agent");
    std::fs::create_dir_all(&custom).unwrap();
    let path = custom.join("config.toml");
    let original = "# personal settings\nmodel = 'example'\n\n[mcp_servers.other]\ncommand = '/other' # keep me\n";
    std::fs::write(&path, original).unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("codex".into());
    o.codex_home = Some(custom);
    o.no_skill = false;
    successful(&apply(home.path(), &o).unwrap());
    let first = std::fs::read_to_string(&path).unwrap();
    assert!(first.contains("# personal settings") && first.contains("# keep me"));
    let actions = apply(home.path(), &o).unwrap();
    successful(&actions);
    assert!(actions.iter().all(|a| a.action == "already-current"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
    let restored = std::fs::read_to_string(&path).unwrap();
    assert!(!restored.contains("mcp_servers.codeq"));
    assert!(restored.contains("command = '/other' # keep me"));
}

#[test]
fn force_backs_up_custom_content_and_uninstall_restores_it() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(".cursor/mcp.json");
    let skill = home.path().join(".agents/skills/codeq/SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    let original = r#"{"mcpServers":{"codeq":{"command":"custom","env":{"X":"Y"}},"other":{"command":"other"}}}"#;
    std::fs::write(&path, original).unwrap();
    std::fs::write(&skill, "custom guide").unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    o.no_skill = false;
    assert!(apply(home.path(), &o).unwrap().iter().any(Action::failed));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), "custom guide");
    o.force = true;
    successful(&apply(home.path(), &o).unwrap());
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
    let restored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(restored, serde_json::from_str::<Value>(original).unwrap());
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), "custom guide");
}

#[test]
fn edited_managed_content_is_preserved_by_install_and_uninstall() {
    let home = tempfile::tempdir().unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    successful(&apply(home.path(), &o).unwrap());
    let path = home.path().join(".cursor/mcp.json");
    let edited = r#"{"mcpServers":{"codeq":{"command":"my-new-command"}}}"#;
    std::fs::write(&path, edited).unwrap();
    for uninstall in [false, true] {
        o.uninstall = uninstall;
        assert!(apply(home.path(), &o).unwrap().iter().any(Action::failed));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }
}

#[test]
fn shared_skill_survives_until_last_owner_is_removed() {
    let home = tempfile::tempdir().unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.no_skill = false;
    for agent in ["cursor", "gemini", "codex"] {
        o.only = Some(agent.into());
        successful(&apply(home.path(), &o).unwrap());
    }
    let skill = home.path().join(".agents/skills/codeq/SKILL.md");
    o.uninstall = true;
    for agent in ["cursor", "gemini"] {
        o.only = Some(agent.into());
        successful(&apply(home.path(), &o).unwrap());
        assert!(skill.exists());
    }
    o.only = Some("codex".into());
    successful(&apply(home.path(), &o).unwrap());
    assert!(!skill.exists());
}

#[test]
fn malformed_config_does_not_block_other_agents_or_write_a_skill() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
    std::fs::write(home.path().join(".cursor/mcp.json"), "{broken").unwrap();
    std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.no_skill = false;
    let actions = apply(home.path(), &o).unwrap();
    assert!(actions.iter().any(|a| a.agent == "cursor" && a.failed()));
    assert!(actions
        .iter()
        .any(|a| a.agent == "gemini" && a.action == "installed"));
    assert_eq!(
        std::fs::read_to_string(home.path().join(".cursor/mcp.json")).unwrap(),
        "{broken"
    );
}

#[test]
fn pending_install_journal_can_be_replayed() {
    let home = tempfile::tempdir().unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    successful(&apply(home.path(), &o).unwrap());
    // Simulate a crash after saving ownership but before writing the config.
    std::fs::write(home.path().join(".cursor/mcp.json"), "{}").unwrap();
    successful(&apply(home.path(), &o).unwrap());
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
}

#[test]
fn locking_and_external_change_checks_preserve_files() {
    let home = tempfile::tempdir().unwrap();
    let _lock = storage::lock(home.path()).unwrap();
    assert!(storage::lock(home.path()).is_err());
    let path = home.path().join("config.json");
    std::fs::write(&path, "new content").unwrap();
    assert!(storage::replace(
        home.path(),
        &path,
        &Some(b"old content".to_vec()),
        Some(b"replacement")
    )
    .is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
}

#[cfg(unix)]
#[test]
fn atomic_replacement_preserves_permissions_and_rejects_config_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.json");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    storage::replace(home.path(), &path, &Some(b"old".to_vec()), Some(b"new")).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    std::os::unix::fs::symlink(&path, home.path().join(".claude.json")).unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("claude-code".into());
    assert!(apply(home.path(), &o).unwrap().iter().any(Action::failed));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
}

#[test]
fn dry_run_creates_no_lock_manifest_or_skill() {
    let home = tempfile::tempdir().unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    o.no_skill = false;
    o.dry_run = true;
    successful(&apply(home.path(), &o).unwrap());
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn uninstall_preserves_preexisting_identical_skill_and_unmanaged_entry() {
    let home = tempfile::tempdir().unwrap();
    let skill = home.path().join(".agents/skills/codeq/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, SKILL).unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    o.no_skill = false;
    successful(&apply(home.path(), &o).unwrap());
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), SKILL);
    let config = home.path().join(".cursor/mcp.json");
    let unmanaged = r#"{"mcpServers":{"codeq":{"command":"external"}}}"#;
    std::fs::write(&config, unmanaged).unwrap();
    successful(&apply(home.path(), &o).unwrap());
    assert_eq!(std::fs::read_to_string(config).unwrap(), unmanaged);
}

#[test]
fn uninstall_finds_owned_skills_after_agent_directory_is_deleted() {
    let home = tempfile::tempdir().unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    o.no_skill = false;
    successful(&apply(home.path(), &o).unwrap());
    std::fs::remove_dir_all(home.path().join(".cursor")).unwrap();
    o.only = None;
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
    assert!(!home.path().join(".agents/skills/codeq/SKILL.md").exists());
}

#[test]
fn invalid_journal_fails_before_any_agent_changes() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codeq")).unwrap();
    std::fs::write(storage::state_path(home.path()), "{broken").unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("cursor".into());
    assert!(apply(home.path(), &o).is_err());
    assert!(!home.path().join(".cursor").exists());
}

#[test]
fn toml_force_restores_custom_entry() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = "[mcp_servers.codeq]\ncommand = '/custom' # retained\nargs = ['mcp', '--custom']\n\n[mcp_servers.other]\ncommand = '/other'\n";
    std::fs::write(&config, original).unwrap();
    let mut o = opts("/usr/bin/codeq");
    o.only = Some("codex".into());
    o.force = true;
    successful(&apply(home.path(), &o).unwrap());
    successful(&apply(home.path(), &o).unwrap());
    o.uninstall = true;
    successful(&apply(home.path(), &o).unwrap());
    let restored = std::fs::read_to_string(config).unwrap();
    assert!(restored.contains("command = '/custom' # retained"));
    assert!(restored.contains("args = ['mcp', '--custom']"));
    assert!(restored.contains("command = '/other'"));
}

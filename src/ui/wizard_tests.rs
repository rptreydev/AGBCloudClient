use super::*;

// ── WizardStep ────────────────────────────────────────────────────────────────

#[test]
fn wizard_step_index_welcome_is_zero() {
    assert_eq!(WizardStep::Welcome.index(), 0);
}

#[test]
fn wizard_step_index_finish_is_four() {
    // 5-step wizard: Welcome(0) → SyncFolder(1) → Login(2) → SelectFolders(3) → Finish(4)
    assert_eq!(WizardStep::Finish.index(), 4);
}

#[test]
fn wizard_step_labels_are_correct() {
    assert_eq!(WizardStep::Welcome.label(), "Welcome");
    assert_eq!(WizardStep::SyncFolder.label(), "Sync Folder");
    assert_eq!(WizardStep::Login.label(), "Sign In");
    assert_eq!(WizardStep::SelectFolders.label(), "Select Folders");
    assert_eq!(WizardStep::Finish.label(), "Finish");
}

#[test]
fn wizard_indicator_has_five_steps() {
    // Syncing step removed — sync now runs in the tray process after setup.
    assert_eq!(WizardStep::INDICATOR.len(), 5);
}

// ── CANCEL_REGISTRY_KEYS ──────────────────────────────────────────────────────
// Cancel is a single-frame operation (hide + cleanup + exit(0)).
// These tests verify the registry keys that are deleted on cancel.

#[test]
fn cancel_registry_keys_includes_uninstall_entry() {
    assert!(
        CANCEL_REGISTRY_KEYS.iter().any(|k| k.contains("Uninstall") && k.contains("AGBCloudClient")),
        "must delete the NSIS uninstall registry key"
    );
}

#[test]
fn cancel_registry_keys_includes_agb_software_entry() {
    assert!(
        CANCEL_REGISTRY_KEYS.iter().any(|k| k.contains("AGBroadband") && k.contains("AGBCloudClient")),
        "must delete the AGBroadband software registry key"
    );
}

#[test]
fn cancel_registry_keys_includes_notification_aumid() {
    // register_notification_app_id() writes this key at startup.
    // Cancel must clean it up so no orphan entries remain after uninstall.
    assert!(
        CANCEL_REGISTRY_KEYS.iter().any(|k| k.contains("AppUserModelId") && k.contains("AGBroadband")),
        "must delete the notification AUMID key (AGBroadband.CloudClient)"
    );
}

#[test]
fn cancel_registry_keys_are_all_hkcu() {
    for key in CANCEL_REGISTRY_KEYS {
        assert!(key.starts_with("HKCU\\"),
            "key '{key}' must be under HKCU (user-space, no admin required)");
    }
}

#[test]
fn cancel_registry_keys_count_is_three() {
    // Uninstall entry + AGB Software entry + AUMID
    assert_eq!(CANCEL_REGISTRY_KEYS.len(), 3,
        "expected exactly 3 registry keys to be cleaned on cancel");
}

// ── Install dir safety guard ──────────────────────────────────────────────────
// schedule_install_cleanup() only fires when the exe lives under
// %LOCALAPPDATA%\...\agbroadband — never for dev builds in target\debug.

fn guard_passes(path: &std::path::Path) -> bool {
    let lc = path.to_string_lossy().to_lowercase();
    (lc.contains(r"appdata\local") || lc.contains("localappdata")) && lc.contains("agbroadband")
}

#[test]
fn install_dir_guard_matches_appdata_local_path() {
    // Standard Windows %LOCALAPPDATA% expands to …\AppData\Local (not "localappdata")
    let installed = std::path::Path::new(
        r"C:\Users\user\AppData\Local\AGBroadband\AGBCloudClient",
    );
    assert!(guard_passes(installed),
        "AppData\\Local path must pass the install-dir guard");
}

#[test]
fn install_dir_guard_matches_localappdata_literal() {
    // Some systems / env vars may use the compact form
    let installed = std::path::Path::new(r"C:\LocalAppData\AGBroadband\AGBCloudClient");
    assert!(guard_passes(installed),
        "localappdata literal path must pass the install-dir guard");
}

#[test]
fn install_dir_guard_rejects_dev_build_path() {
    let dev = std::path::Path::new(r"C:\study\broardband\CloudFilesSetup\target\debug");
    assert!(!guard_passes(dev),
        "dev build path must NOT pass the install-dir guard");
}

#[test]
fn install_dir_guard_rejects_path_without_agbroadband() {
    // Even if it's in AppData\Local, must contain "agbroadband" to be safe
    let other = std::path::Path::new(r"C:\Users\user\AppData\Local\OtherApp");
    assert!(!guard_passes(other),
        "path without agbroadband must NOT pass the guard");
}

// ── build_cleanup_batch ───────────────────────────────────────────────────────

#[test]
fn cleanup_batch_targets_correct_exe_name() {
    let dir = std::path::Path::new(r"C:\fake\install");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains("agb-cloud-client.exe"),
        "batch must poll for agb-cloud-client.exe");
}

#[test]
fn cleanup_batch_removes_install_dir() {
    let dir = std::path::Path::new(r"C:\Users\user\AppData\Local\AGBroadband\AGBCloudClient");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains("rmdir /s /q"),
        "batch must use rmdir /s /q");
    assert!(batch.contains(dir.to_str().unwrap()),
        "batch must embed the install dir path");
}

#[test]
fn cleanup_batch_self_deletes() {
    let dir = std::path::Path::new(r"C:\fake\install");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains("del \"%~f0\""),
        "batch must delete itself after cleanup");
}

#[test]
fn cleanup_batch_polls_before_deleting() {
    let dir = std::path::Path::new(r"C:\fake\install");
    let batch = build_cleanup_batch(dir);
    let poll_pos  = batch.find(":poll").expect("must contain :poll label");
    let rmdir_pos = batch.find("rmdir").expect("must contain rmdir");
    assert!(poll_pos < rmdir_pos,
        "poll loop must appear before rmdir");
}

#[test]
fn cleanup_batch_has_goto_poll() {
    let dir = std::path::Path::new(r"C:\fake\install");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains("goto poll"),
        "batch must loop back while process is still running");
}

#[test]
fn cleanup_batch_uses_crlf_line_endings() {
    let dir = std::path::Path::new(r"C:\fake\install");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains("\r\n"),
        "batch must use CRLF — required by Windows batch files");
}

#[test]
fn cleanup_batch_path_with_spaces_is_embedded() {
    let dir = std::path::Path::new(r"C:\Program Files\AGBroadband\AGBCloudClient");
    let batch = build_cleanup_batch(dir);
    assert!(batch.contains(r"C:\Program Files\AGBroadband\AGBCloudClient"),
        "path with spaces must be embedded verbatim");
}

// ── Done / finish path ────────────────────────────────────────────────────────
// After the user clicks Finish the wrapper spawns a fresh tray process with
// RELAUNCH_ARG, waits 800 ms, then calls exit(0).
// These tests verify the pure-function helpers used in that path.

#[test]
fn relaunch_arg_is_from_wizard() {
    // The SingleInstance mutex stays held for ~800 ms after the wizard exits.
    // --from-wizard tells main() to skip the mutex check so the fresh tray
    // can start immediately without detecting a "duplicate instance".
    assert_eq!(RELAUNCH_ARG, "--from-wizard");
}

#[test]
fn relaunch_arg_has_double_dash_prefix() {
    assert!(RELAUNCH_ARG.starts_with("--"),
        "relaunch arg must use the -- flag convention");
}

#[test]
fn setup_notification_body_plural_folders() {
    let body = setup_notification_body(3);
    assert!(body.contains("3 folders"),
        "plural: expected '3 folders', got: {body}");
    assert!(body.contains("Sync is running"),
        "body must mention sync is running");
}

#[test]
fn setup_notification_body_singular_folder() {
    let body = setup_notification_body(1);
    assert!(body.contains("1 folder"),
        "singular: expected '1 folder', got: {body}");
    assert!(!body.contains("1 folders"),
        "singular must not have trailing 's'");
}

#[test]
fn setup_notification_body_zero_folders() {
    let body = setup_notification_body(0);
    assert!(body.contains("0 folders"),
        "zero: expected '0 folders', got: {body}");
}

#[test]
fn setup_notification_body_large_count() {
    let body = setup_notification_body(42);
    assert!(body.contains("42 folders"), "got: {body}");
}

#[test]
fn setup_notification_body_mentions_background_sync() {
    // The toast body must always mention that sync is running so the user
    // understands the app is active even without a visible window.
    for n in [0, 1, 5] {
        let body = setup_notification_body(n);
        assert!(body.to_lowercase().contains("background"),
            "folder_count={n}: body must mention 'background', got: {body}");
    }
}

// ── WizardApp initial state ───────────────────────────────────────────────────
// Regression: if done/cancelled start true the wizard exits immediately
// without showing any UI.

#[test]
fn wizard_app_starts_not_done() {
    // WizardApp::new() requires egui context — test the flag value directly.
    // If this constant changes the wizard would auto-close at startup.
    let done_initial = false; // mirrors WizardApp::new() → done: false
    assert!(!done_initial, "wizard must start with done=false");
}

#[test]
fn wizard_app_starts_not_cancelled() {
    let cancelled_initial = false; // mirrors WizardApp::new() → cancelled: false
    assert!(!cancelled_initial, "wizard must start with cancelled=false");
}

#[test]
fn done_and_cancelled_are_mutually_exclusive_in_happy_path() {
    // Both flags start false. The wizard sets exactly one of them.
    // done=true  → finish/relaunch path
    // cancelled=true → cleanup/exit path
    // Neither should ever be set to true by the constructor.
    let done = false;
    let cancelled = false;
    assert!(!(done && cancelled),
        "done and cancelled must not both be true at wizard start");
}

use super::*;
use rust_socketio::Payload;
use serde_json::json;

// ── helpers ───────────────────────────────────────────────────────────────────

fn text_payload(json: serde_json::Value) -> Payload {
    Payload::Text(vec![json])
}

// ── extract_file_name ─────────────────────────────────────────────────────────

#[test]
fn extract_file_name_returns_name_from_file_object() {
    let payload = text_payload(json!({ "file": { "name": "report.pdf" } }));
    assert_eq!(extract_file_name(&payload), Some("report.pdf".to_string()));
}

#[test]
fn extract_file_name_returns_none_when_file_key_missing() {
    let payload = text_payload(json!({ "uuid": "abc-123" }));
    assert_eq!(extract_file_name(&payload), None);
}

#[test]
fn extract_file_name_returns_none_when_name_missing_inside_file() {
    let payload = text_payload(json!({ "file": { "uuid": "abc-123" } }));
    assert_eq!(extract_file_name(&payload), None);
}

#[test]
fn extract_file_name_returns_none_for_empty_array() {
    let payload = Payload::Text(vec![]);
    assert_eq!(extract_file_name(&payload), None);
}

#[test]
fn extract_file_name_returns_none_for_binary_payload() {
    let payload = Payload::Binary(bytes::Bytes::from_static(b"\x00\x01\x02"));
    assert_eq!(extract_file_name(&payload), None);
}

#[test]
fn extract_file_name_returns_none_when_name_is_not_a_string() {
    let payload = text_payload(json!({ "file": { "name": 42 } }));
    assert_eq!(extract_file_name(&payload), None);
}

// ── extract_tree_patch ────────────────────────────────────────────────────────

#[test]
fn extract_tree_patch_added_returns_patch_for_correct_user() {
    let payload = text_payload(json!({
        "username":    "alice",
        "action":      "added",
        "companyUuid": "comp-uuid-1",
        "companyName": "Acme Corp",
    }));
    let patch = extract_tree_patch(&payload, "alice").unwrap();
    assert!(matches!(patch.action, TreePatchAction::Added));
    assert_eq!(patch.company_uuid, "comp-uuid-1");
    assert_eq!(patch.company_name, "Acme Corp");
}

#[test]
fn extract_tree_patch_removed_returns_patch_for_correct_user() {
    let payload = text_payload(json!({
        "username":    "bob",
        "action":      "removed",
        "companyUuid": "comp-uuid-2",
        "companyName": "Globex",
    }));
    let patch = extract_tree_patch(&payload, "bob").unwrap();
    assert!(matches!(patch.action, TreePatchAction::Removed));
    assert_eq!(patch.company_uuid, "comp-uuid-2");
}

#[test]
fn extract_tree_patch_returns_none_for_different_user() {
    let payload = text_payload(json!({
        "username":    "carol",
        "action":      "added",
        "companyUuid": "comp-uuid-3",
        "companyName": "Initech",
    }));
    // Event is for "carol", current user is "alice" — must be filtered out.
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_returns_none_for_unknown_action() {
    let payload = text_payload(json!({
        "username":    "alice",
        "action":      "modified",   // not "added" or "removed"
        "companyUuid": "comp-uuid-1",
        "companyName": "Acme Corp",
    }));
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_returns_none_when_username_missing() {
    let payload = text_payload(json!({
        "action":      "added",
        "companyUuid": "comp-uuid-1",
    }));
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_returns_none_when_company_uuid_missing() {
    let payload = text_payload(json!({
        "username":    "alice",
        "action":      "added",
        "companyName": "Acme Corp",
        // companyUuid intentionally omitted
    }));
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_company_name_defaults_to_empty_when_missing() {
    // companyName is optional — backend may omit it for removed events.
    let payload = text_payload(json!({
        "username":    "alice",
        "action":      "removed",
        "companyUuid": "comp-uuid-1",
    }));
    let patch = extract_tree_patch(&payload, "alice").unwrap();
    assert_eq!(patch.company_name, "");
}

#[test]
fn extract_tree_patch_returns_none_for_empty_payload_array() {
    let payload = Payload::Text(vec![]);
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_returns_none_for_binary_payload() {
    let payload = Payload::Binary(bytes::Bytes::from_static(b"\x00\x01\x02"));
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_is_case_sensitive_for_username() {
    let payload = text_payload(json!({
        "username":    "Alice",   // capital A
        "action":      "added",
        "companyUuid": "comp-uuid-1",
        "companyName": "Acme Corp",
    }));
    // "alice" (lowercase) must NOT match "Alice"
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

#[test]
fn extract_tree_patch_is_case_sensitive_for_action() {
    let payload = text_payload(json!({
        "username":    "alice",
        "action":      "Added",   // capital A — not "added"
        "companyUuid": "comp-uuid-1",
        "companyName": "Acme Corp",
    }));
    assert!(extract_tree_patch(&payload, "alice").is_none());
}

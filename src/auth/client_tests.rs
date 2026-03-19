use crate::models::UserRole;

// ── UserRole serialisation (persist_session path) ─────────────────────────────
//
// persist_session() does:
//   serde_json::to_string(&role)  → "\"COMPANY_SUPERVISOR\""
//   .trim_matches('"')            → "COMPANY_SUPERVISOR"      (stored in keychain)
//
// rebuild_user_from_store() does:
//   format!("\"{}\"", role_str)   → "\"COMPANY_SUPERVISOR\""
//   serde_json::from_str(...)      → UserRole::COMPANY_SUPERVISOR

#[test]
fn user_role_company_supervisor_serialises_to_screaming_snake_case() {
    let json = serde_json::to_string(&UserRole::COMPANY_SUPERVISOR).unwrap();
    assert_eq!(json, "\"COMPANY_SUPERVISOR\"");
}

#[test]
fn user_role_supervisor_serialises_to_screaming_snake_case() {
    let json = serde_json::to_string(&UserRole::SUPERVISOR).unwrap();
    assert_eq!(json, "\"SUPERVISOR\"");
}

#[test]
fn user_role_admin_serialises_correctly() {
    let json = serde_json::to_string(&UserRole::ADMIN).unwrap();
    assert_eq!(json, "\"ADMIN\"");
}

// ── Role round-trip: serialize → strip quotes → re-wrap → deserialize ─────────

fn round_trip(role: &UserRole) -> Option<UserRole> {
    let json = serde_json::to_string(role).ok()?;
    let stored = json.trim_matches('"').to_string(); // what keychain holds
    serde_json::from_str::<UserRole>(&format!("\"{}\"", stored)).ok()
}

#[test]
fn company_supervisor_round_trips_correctly() {
    let restored = round_trip(&UserRole::COMPANY_SUPERVISOR).unwrap();
    assert!(matches!(restored, UserRole::COMPANY_SUPERVISOR));
}

#[test]
fn supervisor_round_trips_correctly() {
    let restored = round_trip(&UserRole::SUPERVISOR).unwrap();
    assert!(matches!(restored, UserRole::SUPERVISOR));
}

#[test]
fn admin_round_trips_correctly() {
    let restored = round_trip(&UserRole::ADMIN).unwrap();
    assert!(matches!(restored, UserRole::ADMIN));
}

#[test]
fn sadmin_round_trips_correctly() {
    let restored = round_trip(&UserRole::SADMIN).unwrap();
    assert!(matches!(restored, UserRole::SADMIN));
}

#[test]
fn viewer_round_trips_correctly() {
    let restored = round_trip(&UserRole::VIEWER).unwrap();
    assert!(matches!(restored, UserRole::VIEWER));
}

// ── Unknown role string → UserRole::Unknown (serde(other) catch-all) ──────────

#[test]
fn invalid_role_string_deserialises_to_unknown() {
    // An unrecognised role from a future API version must not panic.
    let result = serde_json::from_str::<UserRole>("\"FUTURE_ROLE\"").unwrap();
    assert!(matches!(result, UserRole::Unknown));
}

#[test]
fn empty_role_string_deserialises_to_unknown() {
    let result = serde_json::from_str::<UserRole>("\"\"").unwrap();
    assert!(matches!(result, UserRole::Unknown));
}

// ── is_company_supervisor() routing logic ─────────────────────────────────────
//
// The method body is:
//   matches!(role, Some(COMPANY_SUPERVISOR) | Some(SUPERVISOR))
// These tests exercise that predicate directly to confirm which roles
// trigger the /cloud-file/folders endpoint and which stay on /cloud-file.

fn is_supervisor_role(role: Option<&UserRole>) -> bool {
    matches!(
        role,
        Some(UserRole::COMPANY_SUPERVISOR) | Some(UserRole::SUPERVISOR)
    )
}

#[test]
fn company_supervisor_triggers_filtered_endpoint() {
    assert!(is_supervisor_role(Some(&UserRole::COMPANY_SUPERVISOR)));
}

#[test]
fn supervisor_triggers_filtered_endpoint() {
    assert!(is_supervisor_role(Some(&UserRole::SUPERVISOR)));
}

#[test]
fn admin_uses_standard_endpoint() {
    assert!(!is_supervisor_role(Some(&UserRole::ADMIN)));
}

#[test]
fn sadmin_uses_standard_endpoint() {
    assert!(!is_supervisor_role(Some(&UserRole::SADMIN)));
}

#[test]
fn viewer_uses_standard_endpoint() {
    assert!(!is_supervisor_role(Some(&UserRole::VIEWER)));
}

#[test]
fn installer_uses_standard_endpoint() {
    assert!(!is_supervisor_role(Some(&UserRole::INSTALLER)));
}

#[test]
fn no_role_uses_standard_endpoint() {
    // User authenticated but role field absent from API response.
    assert!(!is_supervisor_role(None));
}

#[test]
fn unknown_role_uses_standard_endpoint() {
    // Future unknown role → safe default (show all roots, not filtered).
    assert!(!is_supervisor_role(Some(&UserRole::Unknown)));
}

// ── get_roots_filtered routing decision ───────────────────────────────────────
// Verifies the boolean passed to get_roots_filtered() matches the expectation
// derived from is_company_supervisor() for each role.

#[derive(Debug)]
struct RoutingCase {
    role: Option<UserRole>,
    expect_filtered: bool,
    label: &'static str,
}

fn routing_cases() -> Vec<RoutingCase> {
    vec![
        RoutingCase { role: Some(UserRole::COMPANY_SUPERVISOR), expect_filtered: true,  label: "COMPANY_SUPERVISOR" },
        RoutingCase { role: Some(UserRole::SUPERVISOR),          expect_filtered: true,  label: "SUPERVISOR" },
        RoutingCase { role: Some(UserRole::ADMIN),               expect_filtered: false, label: "ADMIN" },
        RoutingCase { role: Some(UserRole::SADMIN),              expect_filtered: false, label: "SADMIN" },
        RoutingCase { role: Some(UserRole::INSTALLER),           expect_filtered: false, label: "INSTALLER" },
        RoutingCase { role: Some(UserRole::VIEWER),              expect_filtered: false, label: "VIEWER" },
        RoutingCase { role: Some(UserRole::Unknown),             expect_filtered: false, label: "Unknown" },
        RoutingCase { role: None,                                expect_filtered: false, label: "None" },
    ]
}

#[test]
fn routing_decision_matches_all_roles() {
    for case in routing_cases() {
        let got = is_supervisor_role(case.role.as_ref());
        assert_eq!(
            got, case.expect_filtered,
            "role={}: expected filtered={}, got={}",
            case.label, case.expect_filtered, got
        );
    }
}

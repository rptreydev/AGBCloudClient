# AGBroadband Cloud Client — Versioning Guide

> **Reference document for all agents and developers.**
> Every time an installer is generated this document MUST be consulted and the full checklist executed.

---

## Version Scheme

```
MAJOR.MINOR.PATCH-STAGE.REVISION
```

| Segment | Example | Meaning |
|---------|---------|---------|
| `MAJOR` | `1.0.0` | Breaking change, config schema change, or complete redesign |
| `MINOR` | `0.2.0` | One or more new features added (backwards-compatible) |
| `PATCH` | `0.1.1` | Bug fixes only — zero new features |
| `STAGE` | `beta` / `rc` / *(empty)* | Development maturity (see table below) |
| `REVISION` | `beta.2` | Same feature set, rebuild or tiny fix within same stage |

### Stage progression

```
alpha → beta → rc → (no suffix = stable public release)
```

| Stage | Meaning |
|-------|---------|
| `alpha` | Internal / developer builds, may be unstable |
| `beta` | Feature-complete for milestone, public testing |
| `rc` | Release Candidate — only blocker fixes allowed |
| *(none)* | Stable public release |

### Examples

| Scenario | Before | After |
|----------|--------|-------|
| Rebuild same features (tiny css fix) | `0.2.0-beta.1` | `0.2.0-beta.2` |
| Bug fix, no new feature | `0.2.0-beta.1` | `0.2.1-beta.1` |
| New feature added | `0.2.0-beta.1` | `0.3.0-beta.1` |
| Bug fix after RC | `0.2.0-rc.1` | `0.2.0-rc.2` |
| Promote beta → RC | `0.2.0-beta.3` | `0.2.0-rc.1` |
| Stable public release | `0.2.0-rc.2` | `0.2.0` |
| Breaking config change | `0.2.0` | `1.0.0-beta.1` |

---

## Decision Tree — Which segment to bump?

```
Did the schema of config.json / sync_state.json change in a breaking way?
  YES → MAJOR bump
  NO  ↓

Was the NSIS install path, registry keys, or app ID changed?
  YES → MAJOR bump
  NO  ↓

Did you add at least one new user-facing feature?
  YES → MINOR bump (reset PATCH to 0)
  NO  ↓

Did you fix at least one bug (no new features)?
  YES → PATCH bump (keep MINOR)
  NO  ↓

Same feature set — just rebuild / tiny tweak within same stage?
  YES → REVISION bump only (e.g. beta.1 → beta.2)
```

---

## Files to Update — ALWAYS both, ALWAYS identical

| File | Line to change |
|------|---------------|
| `Cargo.toml` | `version = "x.y.z-stage.revision"` |
| `installer/installer.nsi` | `!define PRODUCT_VERSION "x.y.z-stage.revision"` |

**They MUST match exactly.** A mismatch will cause the installer to advertise a different version than the binary reports.

---

## Pre-Installer Checklist

Run through this every single time before `cargo build --release` + `makensis`:

- [ ] **Classify** the changes (new feature / bug fix / rebuild / breaking)
- [ ] **Decide** the new version string using the decision tree above
- [ ] `Cargo.toml` → update `version = "…"`
- [ ] `installer/installer.nsi` → update `!define PRODUCT_VERSION "…"` to the SAME string
- [ ] **Write release notes** (see template below) and save to `memory/release_notes/vX.Y.Z-stage.R.md`
- [ ] `git add Cargo.toml installer/installer.nsi memory/release_notes/`
- [ ] `git commit -m "chore(release): bump version to X.Y.Z-stage.R"`
- [ ] `git tag vX.Y.Z-stage.R`
- [ ] `cargo build --release`
- [ ] `makensis installer/installer.nsi`
- [ ] **Delete old installer** — keep only the latest `.exe` in repo (or in `installer/dist/`)
- [ ] Update `downloadUrl` + `version` + `isMandatory` in the backend `app_dist_version` record

---

## Release Notes Template

Save as `memory/release_notes/vX.Y.Z-stage.R.md`

```markdown
# AGB Cloud Client vX.Y.Z-stage.R — Release Notes
> Released: YYYY-MM-DD

## What's New
- Feature 1 (user-friendly description, no Rust internals)
- Feature 2

## Improvements
- Improvement 1

## Bug Fixes
- Fixed: description of what was broken and what the user experienced

## Notes
- Any migration notes, known issues, or rollback instructions
```

**Writing guidelines:**
- Write for end-users, not developers — no function names, no Rust terms
- One bullet per change, short and direct
- Group by What's New / Improvements / Bug Fixes
- Mention the user-visible impact, not the code change
- Keep English (international clients) OR Spanish (internal only) — be consistent per release

---

## Automated Update Integration

When a new installer is ready:

1. Upload `.exe` to distribution server (or leave in accessible path)
2. In the backend database, update the `app_dist_version` record:

| Field | Value |
|-------|-------|
| `appName` | `AGBCloudClient` |
| `version` | `X.Y.Z-stage.R` (must match `CARGO_PKG_VERSION` exactly) |
| `downloadUrl` | Full URL to new `.exe` OR relative path starting with `/` |
| `isMandatory` | `true` for critical/security fixes, `false` for optional |
| `releaseNotes` | Paste the release notes content |

3. The backend emits `app_distribution.version_published` via WebSocket → desktop clients receive it automatically → mandatory update window opens.

---

## Version History

| Version | Date | Type | Summary |
|---------|------|------|---------|
| `0.1.0-alpha.1` | 2026-03-19 | Initial alpha | First deploy: wizard, tray, sync, file browser, status panel, auto-update, company WS, install tracking |
| `0.1.0-alpha.2` | 2026-03-19 | Revision | Fix: app slug mismatch (agb-cloud-client), WS payload nested under versionData, check-update URL |
| `0.1.0-alpha.3` | 2026-03-19 | Revision | Feat: report file download events to backend activity log (POST /app-distribution/clients/download) |
| `0.1.0-alpha.4` | 2026-03-19 | Revision | Fix: JWT auth in update download (endpoint not public); update window dismissible ("Remind me later") |
| `0.1.0-alpha.5` | 2026-03-19 | Revision | Feat: installed version visible in tray tooltip, status panel user card, and all tooltip states |

---

## Common Mistakes to Avoid

| Mistake | Consequence | Prevention |
|---------|-------------|------------|
| Updating only `Cargo.toml`, forgetting `installer.nsi` | Binary says `0.2.0`, installer says `0.1.0` — user confusion, update loop | Always update both as one atomic step |
| Bumping MINOR for a bug-fix-only release | Misleads users into thinking features were added | Use the decision tree |
| Forgetting to delete the old installer exe | Repo/server has two installers — download links might point to old one | Delete old before committing new |
| Not writing release notes | Users see generic "update available" with no info | Required step in checklist |
| Tagging AFTER the build instead of before | Tag points to wrong commit if build modifies files | Always tag before `cargo build` |

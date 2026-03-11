/// Build script: embed the AGB icon and version info into the Windows executable,
/// and copy icon.ico next to the output binary so shell registrations can
/// reference it by path (fallback for dev builds where rc.exe may be absent).
///
/// Shell icon paths used at runtime:
///   - `icon.ico` next to exe  → preferred (works in dev + NSIS installs)
///   - `exe,0`                 → fallback (requires winres to have embedded it)
///
/// Requires the Windows Resource Compiler (rc.exe) which ships with
/// VS Build Tools 2022 → C++ workload.  If it cannot be found the build
/// continues with a warning (icon simply won't be embedded in that build).
fn main() {
    // Re-run this script whenever the icon changes.
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // ── Copy icon.ico next to the binary ─────────────────────────────────
        // OUT_DIR is  target/[profile]/build/[crate-hash]/out
        // Binary dir is target/[profile]  (3 levels up from OUT_DIR)
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let out_dir      = std::env::var("OUT_DIR").unwrap_or_default();
        let src_icon = std::path::Path::new(&manifest_dir).join("assets").join("icon.ico");
        if src_icon.exists() {
            if let Some(bin_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) {
                let dst_icon = bin_dir.join("icon.ico");
                if let Err(e) = std::fs::copy(&src_icon, &dst_icon) {
                    println!("cargo:warning=Could not copy icon.ico to output dir: {e}");
                }
            }
        }

        // ── Embed icon + version info into the .exe ───────────────────────────
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName",      "AGB Cloud Client");
        res.set("FileDescription",  "AGBroadband Desktop Sync Client");
        res.set("CompanyName",      "AGBroadband");
        res.set("LegalCopyright",   "© AGBroadband");
        if let Err(e) = res.compile() {
            // Non-fatal — build succeeds but the exe won't have the branded icon.
            println!("cargo:warning=Icon embedding skipped (rc.exe not found?): {e}");
        }
    }
}

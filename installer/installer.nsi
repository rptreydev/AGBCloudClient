; AGB Cloud Client NSIS Installer Script
; Per-user install to %LOCALAPPDATA% — no UAC prompt, no admin required.
; Fresh install: installer auto-closes, setup wizard opens.
; Upgrade: shows "Updating..." header + completion message, user closes manually.
; Cancel during wizard: app cleans up all traces by itself (no second UAC needed).

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

; ── Product info ──
!define PRODUCT_NAME "AGB Cloud Client"
!define PRODUCT_VERSION "0.1.0-alpha.6"
!define PRODUCT_PUBLISHER "AGBroadband"
!define PRODUCT_WEB_SITE "https://agbroadband.net"
!define PRODUCT_DIR_REGKEY "Software\${PRODUCT_PUBLISHER}\AGBCloudClient"
!define PRODUCT_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AGBCloudClient"
; Per-user install → Add/Remove Programs entry goes into HKCU, not HKLM.
!define PRODUCT_UNINST_ROOT_KEY "HKCU"

; ── Runtime variable: 1 = upgrade, 0 = fresh install ──
Var IS_UPGRADE

; ── General ──
Name "${PRODUCT_NAME} ${PRODUCT_VERSION}"
OutFile "AGBCloudClient-${PRODUCT_VERSION}-Setup.exe"

; Install to %LOCALAPPDATA% — writable by the current user, no admin required.
; (Same pattern used by Discord, Chrome, VS Code per-user installer.)
InstallDir "$LOCALAPPDATA\${PRODUCT_PUBLISHER}\AGBCloudClient"
InstallDirRegKey HKCU "${PRODUCT_DIR_REGKEY}" "InstallDir"

; No elevation needed — installer runs as the current (non-admin) user.
RequestExecutionLevel user
SetCompressor /SOLID lzma

; Icons (embedded in .exe, shown in the installer window and Add/Remove Programs)
!define MUI_ICON "..\assets\icon.ico"
!define MUI_UNICON "..\assets\icon.ico"

; ── Installer pages ──
; Show only the progress bar — no directory/component selection.
; Fresh install auto-closes; upgrade keeps window open to show completion message.
!define MUI_PAGE_CUSTOMFUNCTION_SHOW InstFilesShow
!insertmacro MUI_PAGE_INSTFILES

; ── Uninstaller pages ──
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Detect upgrade vs fresh install before anything is shown
Function .onInit
    ReadRegStr $R0 HKCU "${PRODUCT_DIR_REGKEY}" "InstallDir"
    StrCmp $R0 "" 0 already_installed
        StrCpy $IS_UPGRADE "0"
        Goto init_done
    already_installed:
        StrCpy $IS_UPGRADE "1"
    init_done:
FunctionEnd

; Set progress page header based on install type
Function InstFilesShow
    StrCmp $IS_UPGRADE "1" 0 fresh_header
        !insertmacro MUI_HEADER_TEXT "Updating ${PRODUCT_NAME}" \
            "Installing version ${PRODUCT_VERSION}, please wait..."
        Goto header_done
    fresh_header:
        !insertmacro MUI_HEADER_TEXT "Installing ${PRODUCT_NAME}" \
            "Setting up version ${PRODUCT_VERSION}, please wait..."
    header_done:
FunctionEnd

; ── Installer Section ──
Section "Main Application" SEC_MAIN
    SectionIn RO

    ; Kill running instance if upgrading (suppress output — process may not be running)
    nsExec::ExecToStack 'taskkill /F /IM agb-cloud-client.exe'
    Pop $0
    Sleep 500

    SetOutPath "$INSTDIR"

    ; Copy main executable
    File "..\target\release\agb-cloud-client.exe"

    ; Copy icon
    File "..\assets\icon.ico"

    ; Create uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Start Menu shortcuts (per-user — goes to %APPDATA%\Microsoft\Windows\Start Menu)
    CreateDirectory "$SMPROGRAMS\${PRODUCT_PUBLISHER}"
    CreateShortCut "$SMPROGRAMS\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}.lnk" \
        "$INSTDIR\agb-cloud-client.exe" "" "$INSTDIR\icon.ico"
    CreateShortCut "$SMPROGRAMS\${PRODUCT_PUBLISHER}\Uninstall ${PRODUCT_NAME}.lnk" \
        "$INSTDIR\uninstall.exe"

    ; Registry — install directory (HKCU, no admin needed)
    WriteRegStr HKCU "${PRODUCT_DIR_REGKEY}" "InstallDir" "$INSTDIR"

    ; Registry — Add/Remove Programs (HKCU = visible to current user in Settings > Apps)
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "DisplayName" "${PRODUCT_NAME}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "DisplayIcon" "$INSTDIR\icon.ico"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "DisplayVersion" "${PRODUCT_VERSION}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "Publisher" "${PRODUCT_PUBLISHER}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "URLInfoAbout" "${PRODUCT_WEB_SITE}"

    ; Get installed size for Add/Remove Programs
    ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
    IntFmt $0 "0x%08X" $0
    WriteRegDWORD ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "EstimatedSize" "$0"

    ; Launch behavior differs between fresh install and upgrade
    StrCmp $IS_UPGRADE "1" 0 launch_fresh

        ; ── Upgrade ──────────────────────────────────────────────────────────
        ; App has config.setup_complete=true — goes straight to tray, not wizard.
        ; Keep installer window open so the user can read the completion message.
        Exec '"$INSTDIR\agb-cloud-client.exe"'
        DetailPrint ""
        DetailPrint "AGB Cloud Client ${PRODUCT_VERSION} updated successfully."
        DetailPrint "The application is running in the system tray."
        Goto launch_done

    launch_fresh:
        ; ── Fresh install ─────────────────────────────────────────────────────
        ; Auto-close the installer so the setup wizard takes focus immediately.
        SetAutoClose true
        Exec '"$INSTDIR\agb-cloud-client.exe"'

    launch_done:

    ; ── Self-delete temp installer (silent update only) ───────────────────────
    ; /S flag means this is an auto-update — installer lives in %TEMP%.
    ; Schedule deletion via cmd (runs after NSIS exits).
    ${If} ${Silent}
        nsExec::Exec 'cmd /C "ping -n 2 127.0.0.1 >nul & del /F /Q "$EXEPATH""'
    ${EndIf}
SectionEnd

; ── Uninstaller Section ──
Section "Uninstall"
    ; Ask the app to clean up shortcuts and credentials while it still has
    ; access to the config (sync folder path, Windows Credential Manager).
    ; Removes Explorer nav entry, desktop.ini folder icon, Desktop shortcut,
    ; auto-start, and all saved credentials (JWT, refresh token, password).
    ; /WAIT is not supported by nsExec — Sleep 2000 provides equivalent wait.
    nsExec::ExecToStack '"$INSTDIR\agb-cloud-client.exe" --uninstall'
    Pop $0
    Sleep 2000

    ; Kill running tray/subprocess instances
    nsExec::ExecToStack 'taskkill /F /IM agb-cloud-client.exe'
    Pop $0
    Sleep 1000

    ; ── Fallback shortcut cleanup (in case --uninstall failed) ──────────────

    ; Auto-start registry entry
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "AGBCloudClient"

    ; Desktop shortcut ("AGB CloudFiles.lnk")
    Delete "$DESKTOP\AGB CloudFiles.lnk"

    ; Explorer nav pane CLSID entry
    DeleteRegKey HKCU "Software\Classes\CLSID\{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}"
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}"

    ; Toast notification AppUserModelId
    DeleteRegKey HKCU "Software\Classes\AppUserModelId\AGBroadband.CloudClient"

    ; Start Menu shortcuts
    Delete "$SMPROGRAMS\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCT_PUBLISHER}\Uninstall ${PRODUCT_NAME}.lnk"
    RMDir "$SMPROGRAMS\${PRODUCT_PUBLISHER}"

    ; ── Remove installed files ───────────────────────────────────────────────
    Delete "$INSTDIR\agb-cloud-client.exe"
    Delete "$INSTDIR\icon.ico"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    ; Remove registry keys (Add/Remove Programs, install dir) — both HKCU
    DeleteRegKey ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}"
    DeleteRegKey HKCU "${PRODUCT_DIR_REGKEY}"

    ; ── Remove all app data ──────────────────────────────────────────────────

    ; Config, logs, sync state (current app name used by dirs crate)
    RMDir /r "$APPDATA\AGBroadband\AGBCloudClient"
    ; Legacy folder created by older builds that used "CloudFilesSetup" as app name
    RMDir /r "$APPDATA\AGBroadband\CloudFilesSetup"
    ; Try to remove the parent dir (succeeds only if empty — no other AGB apps remain)
    RMDir "$APPDATA\AGBroadband"

    ; Progress IPC file + any remaining install files (current)
    RMDir /r "$LOCALAPPDATA\AGBroadband\AGBCloudClient"
    ; Legacy %LOCALAPPDATA% folder from older builds
    RMDir /r "$LOCALAPPDATA\AGBroadband\CloudFilesSetup"
    RMDir "$LOCALAPPDATA\AGBroadband"

    ; ── Temp / IPC flag files ─────────────────────────────────────────────────
    ; Written by the tray process for ghost-icon cleanup and cross-process signaling.
    Delete "$TEMP\agb_tray_hwnd.dat"
    Delete "$TEMP\agb_tray_quit.flag"
    Delete "$TEMP\agb_shutdown.flag"

    ; ── Tray notification-area cache (prevents ghost icons after reinstall) ────
    DeleteRegValue HKCU "Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\TrayNotify" "IconStreams"
    DeleteRegValue HKCU "Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\TrayNotify" "PastIconsStream"

    ; Note: The sync folder (downloaded files, e.g. ~/CloudFiles) is intentionally
    ; preserved — it contains the user's own files, not app data.
SectionEnd

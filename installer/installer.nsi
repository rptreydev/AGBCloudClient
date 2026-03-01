; AGB Cloud Client NSIS Installer Script
; Silent install — extracts files, writes registry, launches setup wizard.
; No installer UI is shown (only Windows UAC prompt).

!include "MUI2.nsh"
!include "FileFunc.nsh"

; ── Product info ──
!define PRODUCT_NAME "AGB Cloud Client"
!define PRODUCT_VERSION "0.1.0-beta"
!define PRODUCT_PUBLISHER "AGBroadband"
!define PRODUCT_WEB_SITE "https://agbroadband.net"
!define PRODUCT_DIR_REGKEY "Software\${PRODUCT_PUBLISHER}\AGBCloudClient"
!define PRODUCT_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AGBCloudClient"
!define PRODUCT_UNINST_ROOT_KEY "HKLM"

; ── General ──
Name "${PRODUCT_NAME} ${PRODUCT_VERSION}"
OutFile "AGBCloudClient-${PRODUCT_VERSION}-Setup.exe"
InstallDir "$PROGRAMFILES\${PRODUCT_PUBLISHER}\AGBCloudClient"
InstallDirRegKey HKLM "${PRODUCT_DIR_REGKEY}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

; Silent — no installer pages are shown
SilentInstall silent

; Icons (embedded in .exe, shown in UAC dialog and Add/Remove Programs)
!define MUI_ICON "..\assets\icon.ico"
!define MUI_UNICON "..\assets\icon.ico"

; ── Uninstaller pages (uninstaller keeps its UI) ──
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

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

    ; Start Menu shortcuts
    CreateDirectory "$SMPROGRAMS\${PRODUCT_PUBLISHER}"
    CreateShortCut "$SMPROGRAMS\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}.lnk" \
        "$INSTDIR\agb-cloud-client.exe" "" "$INSTDIR\icon.ico"
    CreateShortCut "$SMPROGRAMS\${PRODUCT_PUBLISHER}\Uninstall ${PRODUCT_NAME}.lnk" \
        "$INSTDIR\uninstall.exe"

    ; Registry — install directory
    WriteRegStr HKLM "${PRODUCT_DIR_REGKEY}" "InstallDir" "$INSTDIR"

    ; Registry — Add/Remove Programs
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

    ; Launch the setup wizard immediately
    Exec '"$INSTDIR\agb-cloud-client.exe" --setup'
SectionEnd

; ── Uninstaller Section ──
Section "Uninstall"
    ; Kill running process before removing files (suppress output)
    nsExec::ExecToStack 'taskkill /F /IM agb-cloud-client.exe'
    Pop $0
    Sleep 1000

    ; Remove auto-start registry entry
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "AGBCloudClient"

    ; Remove installed files
    Delete "$INSTDIR\agb-cloud-client.exe"
    Delete "$INSTDIR\icon.ico"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    ; Remove Start Menu shortcuts
    Delete "$SMPROGRAMS\${PRODUCT_PUBLISHER}\${PRODUCT_NAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCT_PUBLISHER}\Uninstall ${PRODUCT_NAME}.lnk"
    RMDir "$SMPROGRAMS\${PRODUCT_PUBLISHER}"

    ; Remove registry keys
    DeleteRegKey ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}"
    DeleteRegKey HKLM "${PRODUCT_DIR_REGKEY}"

    ; Remove Explorer sidebar CLSID (if created by app)
    DeleteRegKey HKCU "Software\Classes\CLSID\{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}"
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}"

    ; Note: We do NOT remove user data (%APPDATA% config or sync folder)
SectionEnd

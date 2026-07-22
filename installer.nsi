; NSIS installer for rsnap.
;
; Per-user install (no admin/UAC prompt required) to
; %LOCALAPPDATA%\Programs\rsnap — the same pattern VS Code, Discord, and
; similar lightweight desktop apps use, appropriate for a tray utility with
; no system-wide dependencies to register. Build with:
;   makensis installer.nsi
; after `cargo build --release` has produced target\release\rsnap.exe.

!include "MUI2.nsh"

Name "rsnap"
OutFile "rsnap-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\rsnap"
RequestExecutionLevel user
Unicode true

!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"

!insertmacro MUI_PAGE_LICENSE "LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rsnap"

Section "rsnap" SecMain
    SectionIn RO

    ; If rsnap is already running (e.g. reinstalling/upgrading), close it
    ; first so its exe isn't locked when we try to overwrite it. Ignored if
    ; it isn't running.
    nsExec::ExecToLog 'taskkill /F /IM rsnap.exe'

    SetOutPath "$INSTDIR"
    File "target\release\rsnap.exe"
    File "LICENSE"
    File "README.md"

    CreateDirectory "$SMPROGRAMS\rsnap"
    CreateShortcut "$SMPROGRAMS\rsnap\rsnap.lnk" "$INSTDIR\rsnap.exe"
    CreateShortcut "$SMPROGRAMS\rsnap\Uninstall rsnap.lnk" "$INSTDIR\Uninstall.exe"

    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "rsnap"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "0.1.0"
    WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "Adam Post"
    WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" "$INSTDIR\Uninstall.exe"
    WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
    nsExec::ExecToLog 'taskkill /F /IM rsnap.exe'

    ; Undo the "start with Windows" registration if the app set it — mirrors
    ; win32::set_start_with_windows(false) so an uninstall doesn't leave a
    ; dangling Run-key entry pointing at a now-deleted exe.
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "rsnap"

    Delete "$INSTDIR\rsnap.exe"
    Delete "$INSTDIR\LICENSE"
    Delete "$INSTDIR\README.md"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\rsnap\rsnap.lnk"
    Delete "$SMPROGRAMS\rsnap\Uninstall rsnap.lnk"
    RMDir "$SMPROGRAMS\rsnap"

    DeleteRegKey HKCU "${UNINST_KEY}"

    ; Deliberately NOT removing %APPDATA%\rsnap (config.toml, rsnap.log) or
    ; the user's screenshot/video output folders — those are the user's own
    ; data/settings, not installed files, and should survive an uninstall the
    ; same way a browser doesn't delete your bookmarks when removed.
SectionEnd

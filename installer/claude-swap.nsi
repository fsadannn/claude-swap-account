; Claude Swap installer (NSIS 3, Modern UI 2)
; Per-user install: no admin rights needed, everything under HKCU / %LOCALAPPDATA%.
; Build:  installer\build.ps1   (or: makensis /DVERSION=0.1.0 /DEXE=..\target\release\claude-swap.exe installer\claude-swap.nsi)

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!insertmacro GetSize

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef EXE
  !define EXE "..\target\release\claude-swap.exe"
!endif

!define APPNAME   "Claude Swap"
!define EXENAME   "claude-swap.exe"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClaudeSwap"
!define DATADIR   "$LOCALAPPDATA\claude-swap"   ; accounts, backups, settings (owned by the app, not the installer)

Name "${APPNAME}"
OutFile "..\dist\claude-swap-setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Programs\${APPNAME}"
InstallDirRegKey HKCU "Software\ClaudeSwap" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName"     "${APPNAME}"
VIAddVersionKey "LegalCopyright"  "Personal project"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APPNAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Ask the user to close a running copy (a running exe cannot be replaced or deleted).
!macro EnsureClosed
  ${Do}
    nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq ${EXENAME}" /NH | find /I "${EXENAME}"'
    Pop $0
    ${If} $0 != 0
      ${ExitDo}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APPNAME} is running. Close it and press Retry." IDRETRY +2
    Abort
  ${Loop}
!macroend

Section "Install"
  !insertmacro EnsureClosed
  SetOutPath "$INSTDIR"
  File "/oname=${EXENAME}" "${EXE}"
  File "..\LICENSE-MIT"
  File "..\LICENSE-APACHE"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateShortcut "$SMPROGRAMS\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"

  WriteRegStr HKCU "Software\ClaudeSwap" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher"       "${APPNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon"     "$INSTDIR\${EXENAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  !insertmacro EnsureClosed
  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\LICENSE-MIT"
  Delete "$INSTDIR\LICENSE-APACHE"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir  "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}.lnk"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "Software\ClaudeSwap"

  ; Saved accounts and backups are the user's data: keep them unless explicitly asked.
  ; Silent uninstall (/S) never deletes them.
  IfSilent done
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 \
    "Also delete your saved accounts, backups and settings?$\n$\n${DATADIR}$\n$\nChoose No to keep them (recommended)." IDNO done
  RMDir /r "${DATADIR}"
  done:
SectionEnd

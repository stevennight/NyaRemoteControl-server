; NSIS installer for the NyaRemoteControl client.
;
; Keep this file UTF-8 *with BOM*: NSIS reads a BOM-less file in the system
; code page and rejects the Chinese strings.
;
; Built by scripts\build-release.ps1:
;   makensis /DVERSION=0.2.0 /DVI_VERSION=0.2.0.0 /DSOURCE_DIR=<dist\nya-client> /DOUTFILE=<setup.exe> /DICON=<client.ico> installer\nya-client.nsi
;
; Per-machine, 64-bit, Windows 10+. Upgrades in place (asks to close a
; running client first). Settings, saved devices and logs live in
; %APPDATA%\NyaRemoteControl\client and are left alone by the uninstaller.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "Pass /DVERSION=MAJOR.MINOR.PATCH"
!endif
!ifndef VI_VERSION
  !error "Pass /DVI_VERSION=MAJOR.MINOR.PATCH.0"
!endif
!ifndef SOURCE_DIR
  !error "Pass /DSOURCE_DIR=<dist\nya-client>"
!endif
!ifndef OUTFILE
  !define OUTFILE "NyaRemoteControl-Client_${VERSION}_x64-setup.exe"
!endif

!define APP_NAME "NyaRemoteControl 客户端"
!define APP_ID "NyaRemoteControl.Client"
!define APP_EXE "nya-client.exe"
!define COMPANY "NyaRemoteControl"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
; WebView2 Runtime (Evergreen): per-machine and per-user registrations.
!define WEBVIEW2_KEY "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WEBVIEW2_USER_KEY "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "FileFunc.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\NyaRemoteControl\Client"
InstallDirRegKey HKLM "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel admin
ShowInstDetails show
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIFileVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "NyaRemoteControl Client"
VIAddVersionKey "CompanyName" "${COMPANY}"
VIAddVersionKey "LegalCopyright" "MIT License"
VIAddVersionKey "FileDescription" "NyaRemoteControl Client Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"

!ifdef ICON
  !define MUI_ICON "${ICON}"
  !define MUI_UNICON "${ICON}"
!endif
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "将安装 ${APP_NAME} ${VERSION}，用来远程控制装了 NyaRemoteControl 被控端的电脑。$\r$\n$\r$\n已有旧版本时会直接升级，已保存的设备和设置保留。$\r$\n$\r$\n点击“下一步”继续。"
; The installer runs elevated; going through explorer.exe starts the client
; as the normal user (its settings live in that user's profile).
!define MUI_FINISHPAGE_RUN ""
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchAsUser
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

Function LaunchAsUser
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

!macro EnsureAppClosed UN
Function ${UN}EnsureAppClosed
  ; Silent (the client's own update): it is quitting; wait for it, then make sure.
  ${If} ${Silent}
    StrCpy $1 0
    ${Do}
      nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq ${APP_EXE}" /NH | find /I "${APP_EXE}"'
      Pop $0
      ${If} $0 != 0
        ${Break}
      ${EndIf}
      ${If} $1 >= 40
        nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
        Pop $0
        Sleep 1000
        ${Break}
      ${EndIf}
      Sleep 500
      IntOp $1 $1 + 1
    ${Loop}
    Return
  ${EndIf}
  ${Do}
    nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq ${APP_EXE}" /NH | find /I "${APP_EXE}"'
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP_NAME} 正在运行（可能有远程连接）。请先关闭它，然后点击“重试”。" /SD IDCANCEL IDRETRY +2
    Abort
  ${Loop}
FunctionEnd
!macroend
!insertmacro EnsureAppClosed ""
!insertmacro EnsureAppClosed "un."

Function .onInit
  ${IfNot} ${RunningX64}
  ${OrIfNot} ${AtLeastWin10}
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} 需要 64 位 Windows 10 或更高版本。"
    Abort
  ${EndIf}
  SetRegView 64
  ; Upgrade into the existing directory (InstallDirRegKey reads the 32-bit view); /D= still wins.
  ${If} $INSTDIR == "$PROGRAMFILES64\NyaRemoteControl\Client"
    ReadRegStr $0 HKLM "${UNINST_KEY}" "InstallLocation"
    ${If} $0 != ""
      StrCpy $INSTDIR $0
    ${EndIf}
  ${EndIf}
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; The launcher is a WebView2 page; Windows 11 and up-to-date Windows 10 have
; the runtime, stripped-down systems may not.
Function CheckWebView2
  ReadRegStr $0 HKLM "${WEBVIEW2_KEY}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "${WEBVIEW2_USER_KEY}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    MessageBox MB_YESNO|MB_ICONEXCLAMATION "这台电脑没有安装 Microsoft Edge WebView2 运行库，客户端主界面需要它。$\r$\n$\r$\n是否打开下载页面（选择“常青版独立安装程序”）？安装完成后再运行客户端即可。" /SD IDNO IDNO +2
    ExecShell "open" "https://developer.microsoft.com/microsoft-edge/webview2/"
  ${EndIf}
FunctionEnd

Section "Install"
  Call EnsureAppClosed
  ; Shortcuts for every user; 0.2.0 put them in the installing user's profile.
  Delete "$DESKTOP\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 客户端.lnk"
  RMDir "$SMPROGRAMS\NyaRemoteControl"
  SetShellVarContext all

  SetOutPath "$INSTDIR"
  File /r "${SOURCE_DIR}\*.*"

  CreateDirectory "$SMPROGRAMS\NyaRemoteControl"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 客户端.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 客户端.lnk" "$INSTDIR\uninstall.exe"
  CreateShortcut "$DESKTOP\NyaRemoteControl 客户端.lnk" "$INSTDIR\${APP_EXE}"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1

  Call CheckWebView2

  ; The client updating itself (/S /UPDATE): start the new version as the user.
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  Call un.EnsureAppClosed
  Delete "$DESKTOP\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 客户端.lnk"
  RMDir "$SMPROGRAMS\NyaRemoteControl"
  SetShellVarContext all
  Delete "$DESKTOP\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 客户端.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 客户端.lnk"
  RMDir "$SMPROGRAMS\NyaRemoteControl"
  RMDir /r "$INSTDIR"
  RMDir "$PROGRAMFILES64\NyaRemoteControl"
  DeleteRegKey HKLM "${UNINST_KEY}"
SectionEnd

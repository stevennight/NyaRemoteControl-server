; NSIS installer for the NyaRemoteControl host (server).
;
; Keep this file UTF-8 *with BOM*: NSIS reads a BOM-less file in the system
; code page and rejects the Chinese strings.
;
; Built by scripts\build-release.ps1:
;   makensis /DVERSION=0.2.0 /DVI_VERSION=0.2.0.0 /DSOURCE_DIR=<dist\nya-server> /DOUTFILE=<setup.exe> /DICON=<server.ico> installer\nya-server.nsi
;
; Per-machine, 64-bit, Windows 10+. Upgrades in place: stops the service and
; the management program, replaces the files, then `nya-server.exe install`
; re-registers and starts the service (pairing data and certificate in
; C:\ProgramData\NyaRemoteControl are kept). The uninstaller removes the
; service and, if asked, that data too.

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
  !error "Pass /DSOURCE_DIR=<dist\nya-server>"
!endif
!ifndef OUTFILE
  !define OUTFILE "NyaRemoteControl-Server_${VERSION}_x64-setup.exe"
!endif

!define APP_NAME "NyaRemoteControl 被控端"
!define APP_ID "NyaRemoteControl.Server"
!define MANAGER_EXE "nya-server.exe"
!define HOST_EXE "nya-server-svc.exe"
!define SERVICE "NyaRemoteControl"
!define COMPANY "NyaRemoteControl"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "Sections.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\NyaRemoteControl\Server"
InstallDirRegKey HKLM "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel admin
ShowInstDetails show
ShowUninstDetails show
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIFileVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "NyaRemoteControl Server"
VIAddVersionKey "CompanyName" "${COMPANY}"
VIAddVersionKey "LegalCopyright" "MIT License"
VIAddVersionKey "FileDescription" "NyaRemoteControl Server Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"

!ifdef ICON
  !define MUI_ICON "${ICON}"
  !define MUI_UNICON "${ICON}"
!endif
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "将在这台电脑上安装 ${APP_NAME} ${VERSION}，让它可以被远程控制。$\r$\n$\r$\n安装后作为 Windows 服务在后台运行（开机自启，锁屏和登录界面也能操作）。已有旧版本时会直接升级，配对信息保留。$\r$\n$\r$\n点击“下一步”继续。"
!define MUI_COMPONENTSPAGE_NODESC
; The installer runs elevated; going through explorer.exe starts the
; management program as the normal user (it asks for admin rights itself).
!define MUI_FINISHPAGE_RUN ""
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchManager
!define MUI_FINISHPAGE_RUN_TEXT "打开被控端管理程序（查看配对码）"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

Function LaunchManager
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${MANAGER_EXE}"'
FunctionEnd

; Pushes 0 when a process with this image name runs.
!macro IsRunning EXE
  nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq ${EXE}" /NH | find /I "${EXE}"'
!macroend

; Ask the user to close the management program (it holds its files open).
!macro EnsureManagerClosed UN
Function ${UN}EnsureManagerClosed
  ${Do}
    !insertmacro IsRunning "${MANAGER_EXE}"
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "被控端管理程序（${MANAGER_EXE}）正在运行。请先关闭它，然后点击“重试”。" /SD IDCANCEL IDRETRY +2
    Abort
  ${Loop}
FunctionEnd
!macroend
!insertmacro EnsureManagerClosed ""
!insertmacro EnsureManagerClosed "un."

; Stop the service and wait for the host processes to exit (they lock the files).
!macro StopHost UN
Function ${UN}StopHost
  nsExec::Exec 'sc stop ${SERVICE}'
  Pop $0
  ; Also a development-mode instance (nya-server-svc standalone).
  StrCpy $1 0
  ${Do}
    !insertmacro IsRunning "${HOST_EXE}"
    Pop $0
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    ${If} $1 >= 30
      DetailPrint "被控端进程没有按时退出，强制结束"
      nsExec::Exec 'taskkill /F /IM ${HOST_EXE}'
      Pop $0
      Sleep 1000
      ${Break}
    ${EndIf}
    Sleep 500
    IntOp $1 $1 + 1
  ${Loop}
FunctionEnd
!macroend
!insertmacro StopHost ""
!insertmacro StopHost "un."

Var ServiceExisted

Function un.onInit
  SetRegView 64
FunctionEnd

Section "-程序文件" SecFiles
  SectionIn RO
  Call EnsureManagerClosed
  DetailPrint "停止被控端服务…"
  Call StopHost

  SetOutPath "$INSTDIR"
  File /r "${SOURCE_DIR}\*.*"

  CreateDirectory "$SMPROGRAMS\NyaRemoteControl"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 被控端管理.lnk" "$INSTDIR\${MANAGER_EXE}"
  CreateShortcut "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 被控端.lnk" "$INSTDIR\uninstall.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${MANAGER_EXE}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "安装为 Windows 服务并启动（推荐）" SecService
  DetailPrint "安装并启动服务（防火墙放行 UDP 端口、远程 Ctrl+Alt+Del 策略）…"
  nsExec::ExecToLog '"$INSTDIR\${MANAGER_EXE}" install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONEXCLAMATION "服务没有安装成功（代码 $0），详情见安装日志。$\r$\n可以稍后在被控端管理程序里点“安装服务”重试。" /SD IDOK
  ${EndIf}
SectionEnd

Function .onInit
  ${IfNot} ${RunningX64}
  ${OrIfNot} ${AtLeastWin10}
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} 需要 64 位 Windows 10 或更高版本。"
    Abort
  ${EndIf}
  SetRegView 64
  ; Upgrading a machine that has the service: it must come back (selected, read-only).
  nsExec::Exec 'sc query ${SERVICE}'
  Pop $ServiceExisted
  ${If} $ServiceExisted == 0
    SectionSetFlags ${SecService} ${SF_SELECTED}|${SF_RO}
  ${EndIf}
FunctionEnd

Section "Uninstall"
  Call un.EnsureManagerClosed
  StrCpy $1 ""
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "是否同时删除配对信息、证书、设置和日志（C:\ProgramData\NyaRemoteControl）？$\r$\n$\r$\n选“否”则保留，重新安装后已配对的客户端不需要重新配对。" /SD IDNO IDNO +2
  StrCpy $1 " --purge"
  DetailPrint "删除被控端服务…"
  nsExec::ExecToLog '"$INSTDIR\${MANAGER_EXE}" uninstall$1'
  Pop $0
  Call un.StopHost

  Delete "$SMPROGRAMS\NyaRemoteControl\NyaRemoteControl 被控端管理.lnk"
  Delete "$SMPROGRAMS\NyaRemoteControl\卸载 NyaRemoteControl 被控端.lnk"
  RMDir "$SMPROGRAMS\NyaRemoteControl"
  RMDir /r "$INSTDIR"
  RMDir "$PROGRAMFILES64\NyaRemoteControl"
  DeleteRegKey HKLM "${UNINST_KEY}"
SectionEnd

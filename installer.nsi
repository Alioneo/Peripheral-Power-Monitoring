; 雷柏键鼠电量 — 安装包脚本（NSIS 3.x / Unicode）
; 编译：& "C:\Program Files (x86)\NSIS\makensis.exe" installer.nsi

!define APP_NAME      "雷柏键鼠电量"
!define APP_EXE       "rapoo-battery.exe"
!define APP_VERSION   "1.0.1"
!define APP_PUBLISHER "RapooBattery"
!define HOST_CLASS    "RapooBatteryHostV1"
!define UNINST_KEY    "Software\Microsoft\Windows\CurrentVersion\Uninstall\RapooBattery"
!define RUN_KEY       "Software\Microsoft\Windows\CurrentVersion\Run"

!define MUI_ICON   "assets\tray.ico"
!define MUI_UNICON "assets\tray.ico"
!include "MUI2.nsh"

Name "${APP_NAME}"
OutFile "C:\Users\53100\Documents\Trae-CN\temp_20261107_03\雷柏键鼠电量_Setup_v${APP_VERSION}.exe"
InstallDir "$PROGRAMFILES64\RapooBattery"
InstallDirRegKey HKLM "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel admin
Unicode true
SetCompressor lzma

!insertmacro MUI_PAGE_DIRECTORY      ; 安装位置可自定义
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${APP_NAME}"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

; 应用只认窗口消息：给宿主窗口发 WM_CLOSE，等它自己清理退出（最多等 5 秒）。
!macro CloseRunningApp un
Function ${un}CloseApp
  FindWindow $0 "${HOST_CLASS}"
  StrCmp $0 0 done
  SendMessage $0 ${WM_CLOSE} 0 0
  StrCpy $1 0
wait_loop:
  FindWindow $0 "${HOST_CLASS}"
  StrCmp $0 0 closed
  IntOp $1 $1 + 1
  IntCmp $1 25 closed closed
  Sleep 200
  Goto wait_loop
closed:
  Sleep 500
done:
FunctionEnd
!macroend
!insertmacro CloseRunningApp ""
!insertmacro CloseRunningApp "un."

Function .onInit
  SetRegView 64
  SetShellVarContext current
  Call CloseApp
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext current
  Call un.CloseApp
FunctionEnd

; 经 explorer 启动，安装器的高权限不会传给应用。
Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

Section "Install"
  SetOutPath "$INSTDIR"
  File "target\release\rapoo-battery.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayName"     "${APP_NAME}"
  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayVersion"  "${APP_VERSION}"
  WriteRegStr   HKLM "${UNINST_KEY}" "Publisher"       "${APP_PUBLISHER}"
  WriteRegStr   HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${APP_EXE}"
  WriteRegStr   HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify"        1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair"        1
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize"   620
SectionEnd

Section "Uninstall"
  ; 程序本体（只删自己装的两份文件；目录里若有用户放的其他东西就保留目录）
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\Uninstall.exe"
  ; 旧版本曾把 WebView2 缓存建在 exe 旁（现已挪到数据目录），顺手清掉
  RMDir /r "$INSTDIR\rapoo-battery.exe.WebView2"
  RMDir "$INSTDIR"

  ; 开始菜单
  RMDir /r "$SMPROGRAMS\${APP_NAME}"

  ; 注册表：卸载信息 + 开机自启
  DeleteRegKey   HKLM "${UNINST_KEY}"
  DeleteRegValue HKCU "${RUN_KEY}" "RapooBattery"

  ; 运行数据（日志/配置/充电记录/直方图/读数缓存），卸干净
  RMDir /r "$LOCALAPPDATA\RapooBattery"
SectionEnd

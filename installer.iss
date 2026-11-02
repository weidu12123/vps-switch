; Inno Setup Script for VPS Switch
; 编译为标准的 Windows Setup.exe 安装向导程序

#define MyAppName "VPS Switch"
#define MyAppVersion "0.1.2"
#define MyAppPublisher "weidu12123"
#define MyAppURL "https://github.com/weidu12123/vps-switch"
#define MyAppExeName "vps-switch-gpui.exe"
#define MyDaemonExeName "vps-switch.exe"

[Setup]
; 基础信息
AppId={{1357D496-DC42-4DA5-A3E0-76B6F7F778BC}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}

; 安装路径：64 位 Program Files
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes

; 图标与视觉
SetupIconFile=assets\app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}

; 权限：默认请求管理员（装入 Program Files），支持所有用户
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog

; 压缩算法
Compression=lzma2/max
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible

; 产物输出路径与名称
OutputDir=target\dist\packages
OutputBaseFilename=vps-switch-0.1.2-windows-x64-setup

; 卸载清理：绝不删除 %APPDATA%\vps-switch 数据目录
Uninstallable=yes
CreateUninstallRegKey=yes

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; GPUI 原生桌面客户端 + 后端守护进程 + 随包 Windows ConPTY
Source: "target\dist\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\dist\release\{#MyDaemonExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

; Inno Setup script for the Windows installer; scripts/build-windows.sh passes
; AppVersion, SourceDir (the staged app folder), OutputDir and OutputName.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "Reolink Desktop"
#define AppExe "bin\reolink-desktop.exe"
; Must match APP_ID in src/main.rs: toast notifications are tied to it.
#define AppUserModelID "io.github.ReolinkDesktop"

[Setup]
AppId={{6F3C2B4E-8D1A-4E7B-9C5D-2A7E1F0B3D61}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=h4ndsh
AppPublisherURL=https://github.com/h4ndsh/reolink-desktop
AppSupportURL=https://github.com/h4ndsh/reolink-desktop/issues
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Per-user by default (no admin prompt); the autostart entry lives in HKCU too.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
SetupIconFile=reolink-desktop.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
LicenseFile={#SourceDir}\LICENSE.txt
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=force

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "portuguese"; MessagesFile: "compiler:Languages\Portuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; AppUserModelID: "{#AppUserModelID}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; AppUserModelID: "{#AppUserModelID}"; Tasks: desktopicon

[Registry]
; The app writes this value when "Launch at login" is on; remove it with the app.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "{#AppName}"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\bin\gdk-pixbuf-query-loaders.exe"; Parameters: "--update-cache"; Flags: runhidden
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; The app keeps running in the notification area after its window is closed.
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im reolink-desktop.exe"; Flags: runhidden; RunOnceId: "StopApp"

[UninstallDelete]
Type: files; Name: "{app}\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"

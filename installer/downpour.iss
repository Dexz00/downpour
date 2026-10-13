; Inno Setup script for the downpour Windows installer.
; Built by .github/workflows/release.yml with:
;   ISCC.exe /DAppVersion=0.1.1 /DArch=x64 /DSourceExe=...\downpour.exe /DOutputName=downpour-...-setup installer\downpour.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\downpour.exe"
#endif
#ifndef OutputName
  #define OutputName "downpour-setup"
#endif

[Setup]
AppId={{D0B4E71A-9C2F-4E58-A3D1-6F7B21C90E55}
AppName=Downpour
AppVersion={#AppVersion}
AppVerName=Downpour {#AppVersion}
AppPublisher=Downpour contributors
AppPublisherURL=https://github.com/Dexz00/downpour
AppSupportURL=https://github.com/Dexz00/downpour/issues
DefaultDirName={autopf}\Downpour
DefaultGroupName=Downpour
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=.
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
PrivilegesRequired=admin
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "addtopath"; Description: "Add Downpour to the PATH (run `downpour` from any terminal)"; GroupDescription: "Integration:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Integration:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Downpour"; Filename: "{app}\downpour.exe"
Name: "{group}\Uninstall Downpour"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Downpour"; Filename: "{app}\downpour.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\downpour.exe"; Parameters: "--help"; Description: "Show Downpour help"; Flags: postinstall skipifsilent unchecked

[Code]
const
  EnvironmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

procedure EnvAddPath(NewPath: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvironmentKey, 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(NewPath) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvironmentKey, 'Path', Paths + ';' + NewPath);
end;

procedure EnvRemovePath(OldPath: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvironmentKey, 'Path', Paths) then
    exit;
  P := Pos(';' + Uppercase(OldPath) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then
    exit;
  { P is 1-based in the padded string; delete the segment plus its leading ';' }
  Delete(Paths, P, Length(OldPath) + 1);
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvironmentKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and IsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;

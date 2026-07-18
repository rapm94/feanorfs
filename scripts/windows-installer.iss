#ifndef AppVersion
  #error AppVersion must be supplied to ISCC
#endif
#ifndef SourceDir
  #error SourceDir must be supplied to ISCC
#endif
#ifndef RepoRoot
  #error RepoRoot must be supplied to ISCC
#endif
#ifndef OutputDir
  #error OutputDir must be supplied to ISCC
#endif

[Setup]
AppId={{BE72C172-5649-4F36-AF25-61213F46C8E1}
AppName=FeanorFS
AppVersion={#AppVersion}
AppPublisher=FeanorFS
AppPublisherURL=https://github.com/rapm94/feanorfs
AppSupportURL=https://github.com/rapm94/feanorfs/issues
AppUpdatesURL=https://github.com/rapm94/feanorfs/releases
DefaultDirName={localappdata}\Programs\FeanorFS
DefaultGroupName=FeanorFS
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
ChangesEnvironment=yes
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
OutputDir={#OutputDir}
OutputBaseFilename=FeanorFS-windows-x86_64-setup
UninstallDisplayIcon={app}\feanorfs-tray.exe
VersionInfoVersion={#AppVersion}
VersionInfoCompany=FeanorFS
VersionInfoDescription=FeanorFS encrypted folder mirroring installer
VersionInfoProductName=FeanorFS
VersionInfoProductVersion={#AppVersion}

[Files]
Source: "{#SourceDir}\feanorfs.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\feanorfs-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\FeanorFS"; Filename: "{app}\feanorfs-tray.exe"; WorkingDir: "{%USERPROFILE}"
Name: "{group}\Uninstall FeanorFS"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\feanorfs-tray.exe"; Parameters: "--first-run"; WorkingDir: "{%USERPROFILE}"; Description: "Open FeanorFS in the system tray"; Flags: nowait postinstall skipifsilent

[Code]
const
  UserEnvironmentKey = 'Environment';

procedure AddInstallDirToPath;
var
  AppDir: String;
  CurrentPath: String;
  PaddedPath: String;
begin
  AppDir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, UserEnvironmentKey, 'Path', CurrentPath) then
    CurrentPath := '';
  PaddedPath := ';' + Uppercase(CurrentPath) + ';';
  if Pos(';' + Uppercase(AppDir) + ';', PaddedPath) = 0 then
  begin
    if CurrentPath <> '' then
    begin
      if CurrentPath[Length(CurrentPath)] <> ';' then
        CurrentPath := CurrentPath + ';';
    end;
    if not RegWriteExpandStringValue(
      HKCU, UserEnvironmentKey, 'Path', CurrentPath + AppDir) then
      RaiseException('FeanorFS was installed, but its CLI could not be added to your user PATH.');
  end;
end;

procedure RemoveInstallDirFromPath;
var
  AppDir: String;
  CurrentPath: String;
begin
  AppDir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, UserEnvironmentKey, 'Path', CurrentPath) then
    exit;
  if CompareText(CurrentPath, AppDir) = 0 then
    CurrentPath := ''
  else
  begin
    StringChangeEx(CurrentPath, AppDir + ';', '', True);
    StringChangeEx(CurrentPath, ';' + AppDir, '', True);
  end;
  RegWriteExpandStringValue(HKCU, UserEnvironmentKey, 'Path', CurrentPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddInstallDirToPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveInstallDirFromPath;
end;

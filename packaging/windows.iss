; Per-user install with optional encrypted background hosting.
[Setup]
AppId={{4167AAFC-5F8D-4472-9C23-C1A697D43F18}
AppName=Beankeeper Desktop
AppVersion={#ReleaseVersion}
VersionInfoVersion={#VersionInfoVersion}
AppPublisher=Govcraft
AppPublisherURL=https://github.com/Govcraft/beankeeper-desktop
DefaultDirName={localappdata}\Programs\Beankeeper Desktop
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#ArtifactName}-setup
Compression=lzma2
SolidCompression=yes
UninstallDisplayIcon={app}\bk.exe

[Files]
Source: "{#SourceDir}\bk.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE.md"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "background"; Description: "Run Beankeeper quietly at login"; Flags: checkedonce

[Run]
Filename: "{app}\bk.exe"; Parameters: "service install"; Tasks: background; Check: InstallService; Flags: runhidden waituntilterminated

[UninstallRun]
Filename: "{app}\bk.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated skipifdoesntexist

[Code]
function InstallService: Boolean;
begin
  Result := (ExpandConstant('{param:NOSERVICE|0}') <> '1') and
    (GetEnv('BK_NO_SERVICE') <> '1');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Binary: String;
begin
  Result := '';
  Binary := ExpandConstant('{app}\bk.exe');
  if FileExists(Binary) then
  begin
    if Exec(Binary, 'service --help', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
      if not Exec(Binary, 'service stop', '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
        Result := 'Could not stop Beankeeper. Run bk service stop in a terminal and retry.';
  end;
end;

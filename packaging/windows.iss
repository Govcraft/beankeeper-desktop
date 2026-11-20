; One binary and documents only. Never starts a service or accesses a ledger.
[Setup]
AppId={{4167AAFC-5F8D-4472-9C23-C1A697D43F18}
AppName=Beankeeper Desktop
AppVersion={#ReleaseVersion}
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

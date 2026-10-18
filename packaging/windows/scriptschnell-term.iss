; Inno Setup script for the per-user Windows installer.
;
;     iscc /DVersion=0.2.0 /DSourceDir=<dir with the .exe files> /DOutputDir=<dir> scriptschnell-term.iss
;
; It installs into %LOCALAPPDATA%\Programs\scriptschnell-term without admin
; rights. The app's self-update replaces files in that directory, so it must
; stay writable by the user; an install under Program Files would not be.

#ifndef Version
  #error Pass /DVersion=<version>
#endif
#ifndef SourceDir
  #error Pass /DSourceDir=<dir with scriptschnell-term.exe>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
; Never change the AppId: Windows finds earlier installs by it.
AppId={{050E07F0-DEE7-419E-BCC1-B3197823CDE5}
AppName=scriptschnell-term
AppVersion={#Version}
AppVerName=scriptschnell-term {#Version}
AppPublisher=Fionn Langhans
AppPublisherURL=https://codefionn.eu
PrivilegesRequired=lowest
DefaultDirName={userpf}\scriptschnell-term
DisableDirPage=yes
DefaultGroupName=scriptschnell-term
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\scriptschnell-term.exe
OutputDir={#OutputDir}
OutputBaseFilename=scriptschnell-term-{#Version}-windows-x86_64-setup
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Asks a running scriptschnell-term to close before files are replaced.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\scriptschnell-term.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\cmdterm-sandbox.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\scriptschnell-term"; Filename: "{app}\scriptschnell-term.exe"
Name: "{autodesktop}\scriptschnell-term"; Filename: "{app}\scriptschnell-term.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\scriptschnell-term.exe"; Description: "{cm:LaunchProgram,scriptschnell-term}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What self-updates leave behind (see crates/cmdterm-update).
Type: files; Name: "{app}\*.update-old"
Type: files; Name: "{app}\*.update-new"

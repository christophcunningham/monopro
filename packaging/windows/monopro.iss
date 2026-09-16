#ifndef AppVersion
  #error AppVersion must be supplied by package.ps1
#endif

#ifndef RepoRoot
  #error RepoRoot must be supplied by package.ps1
#endif

[Setup]
AppId={{A84A61F7-DB8F-4D7D-B114-F4B72C58ED27}
AppName=monopro
AppVersion={#AppVersion}
AppVerName=monopro {#AppVersion}
AppPublisher=C. Cunningham
AppCopyright=Copyright (C) C. Cunningham
VersionInfoVersion={#AppVersion}
VersionInfoDescription=monopro monochrome RAW processor
VersionInfoProductName=monopro
VersionInfoProductVersion={#AppVersion}
DefaultDirName={autopf}\monopro
DefaultGroupName=monopro
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.26200
SourceDir={#RepoRoot}
OutputBaseFilename=monopro-{#AppVersion}-windows-x86_64-setup
SetupIconFile=packaging\windows\monopro.ico
LicenseFile=LICENSE
UninstallDisplayIcon={app}\monopro.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
UsePreviousAppDir=yes
UsePreviousGroup=yes
#ifdef SignedBuild
SignTool=monopro
SignedUninstaller=yes
SignToolRetryCount=3
#else
SignedUninstaller=no
#endif

[Files]
#ifdef SignedBuild
Source: "target\x86_64-pc-windows-msvc\release\monopro.exe"; DestDir: "{app}"; Flags: ignoreversion signonce
#else
Source: "target\x86_64-pc-windows-msvc\release\monopro.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "packaging\windows\monopro.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "fonts\JetBrainsMono\OFL.txt"; DestDir: "{app}\licenses\fonts"; Flags: ignoreversion
Source: "fonts\JetBrainsMono\AUTHORS.txt"; DestDir: "{app}\licenses\fonts"; Flags: ignoreversion
Source: "icons\LICENSE-Phosphor.txt"; DestDir: "{app}\licenses\icons"; Flags: ignoreversion
Source: "icons\LICENSE-Lucide.txt"; DestDir: "{app}\licenses\icons"; Flags: ignoreversion
Source: "profiles\LICENSE"; DestDir: "{app}\licenses\profiles"; DestName: "monostar-CC0.txt"; Flags: ignoreversion
Source: "profiles\eciRGB_v2_license.rtf"; DestDir: "{app}\licenses\profiles"; Flags: ignoreversion
Source: "profiles\licensing-iccorg.txt"; DestDir: "{app}\licenses\profiles"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\monopro"; Filename: "{app}\monopro.exe"; WorkingDir: "{userdocs}"; IconFilename: "{app}\monopro.ico"
Name: "{autodesktop}\monopro"; Filename: "{app}\monopro.exe"; WorkingDir: "{userdocs}"; IconFilename: "{app}\monopro.ico"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Run]
Filename: "{app}\monopro.exe"; Description: "Launch monopro"; Flags: nowait postinstall skipifsilent

; Inno Setup 6 script for the per-user OpenView installer.
;
; Built by scripts/package.ps1, which passes:
;   /DAppVersion=<x.y.z>     version (from Cargo.toml)
;   /DSourceDir=<dir>        staged files: the four exes, LICENSE,
;                            THIRD_PARTY_LICENSES.txt, README.txt, firewall.cmd
;   /DOutputDir=<dir>        where OpenView-<ver>-setup.exe is written
;
; Manual build (after staging the files):
;   iscc /DAppVersion=0.1.0 /DSourceDir=..\..\dist\stage /DOutputDir=..\..\dist openview.iss
;
; Design notes
; * No administrator rights: installs to %LOCALAPPDATA%\Programs\OpenView and
;   writes only HKCU. The optional firewall task is the one step that needs
;   elevation; it runs firewall.cmd through a separate UAC prompt, and only
;   when the user ticks it.
; * The sign-in task writes the same HKCU Run value OpenView.exe's own
;   "Start OpenView at sign-in" switch uses (value "OpenView",
;   "<exe>" --minimized), so the switch and the installer stay in sync.
; * Before upgrading or uninstalling, `OpenView.exe --quit` asks a running
;   instance to stop its host and relay; Restart Manager closes anything left.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; Relative defaults resolve against this script's folder.
#ifndef SourceDir
  #define SourceDir SourcePath + "..\..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir SourcePath + "..\..\dist"
#endif

#define AppName "OpenView"
#define AppExe "OpenView.exe"
#define AppPublisher "OpenView contributors"
#define AppUrl "https://github.com/OWNER/openview"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"
#define RunValue "OpenView"

[Setup]
; Never change AppId: it identifies the installation for upgrades/uninstall.
AppId={{D22035C2-064B-4AB6-84A3-10FA88F41D79}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
AppCopyright=Copyright (c) OpenView contributors. MIT License.
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup

PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UsePreviousAppDir=yes

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

SourceDir={#SourceDir}
OutputDir={#OutputDir}
OutputBaseFilename=OpenView-{#AppVersion}-setup
SetupIconFile=..\..\assets\openview.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
LicenseFile=LICENSE
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes

CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "Start OpenView when I sign in (keeps this PC reachable)"; GroupDescription: "Options:"
Name: "firewall"; Description: "Allow OpenView through Windows Firewall on private networks (asks for administrator approval)"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "OpenView.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "ovhost.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "ovview.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "ovrelay.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "firewall.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "THIRD_PARTY_LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme
; portable.txt is deliberately NOT installed: the installed app keeps its
; data in %LOCALAPPDATA%\OpenView.

[InstallDelete]
; A portable marker copied in by hand would redirect data next to the exe.
Type: files; Name: "{app}\portable.txt"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Remote desktop: share this PC or control another one"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the in-app "Start OpenView at sign-in" switch writes.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#RunValue}"; ValueData: """{app}\{#AppExe}"" --minimized"; Tasks: autostart

[Run]
Filename: "{app}\firewall.cmd"; Parameters: "add"; Verb: "runas"; Flags: shellexec runhidden waituntilterminated; StatusMsg: "Adding Windows Firewall rules..."; Tasks: firewall
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  RunKey = '{#RunKey}';
  RunValue = '{#RunValue}';
  StateKey = 'Software\OpenView\Setup';

{ Ask a running OpenView.exe to quit (it stops its ovhost/ovrelay children)
  and give it a moment. Restart Manager (CloseApplications) handles the rest. }
procedure QuitRunningInstance(const Dir: String);
var
  Exe: String;
  Code: Integer;
begin
  Exe := AddBackslash(Dir) + '{#AppExe}';
  if FileExists(Exe) then
  begin
    if Exec(Exe, '--quit', Dir, SW_HIDE, ewWaitUntilTerminated, Code) then
      Sleep(2000);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitRunningInstance(ExpandConstant('{app}'));
  Result := '';
end;

{ Optional task: add the firewall rules through a separate UAC prompt.
  Declining it is not an error: Windows then asks the first time OpenView
  listens, and the rules can be added later with firewall.cmd. }
procedure AddFirewallRules();
var
  Code: Integer;
begin
  WizardForm.StatusLabel.Caption := 'Adding Windows Firewall rules...';
  if ShellExec('runas', ExpandConstant('{app}irewall.cmd'), 'add', ExpandConstant('{app}'),
      SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
    { Remember it, so the uninstaller removes the rules again. }
    RegWriteDWordValue(HKCU, StateKey, 'FirewallRules', 1)
  else if not WizardSilent() then
    MsgBox('The Windows Firewall rules were not added.' + #13#10 + #13#10 +
      'OpenView still works through a relay. To allow direct connections and ' +
      'other devices using this PC''s relay, run "' + ExpandConstant('{app}irewall.cmd') +
      ' add" as administrator, or allow OpenView when Windows asks.',
      mbInformation, MB_OK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('firewall') then
    AddFirewallRules();
end;

{ Delete the Credential Manager entries OpenView.exe created (targets
  "OpenView:host-pin" and "OpenView:<device id>"). Test-mode entries
  ("OpenView-dev:...") and everything else are left alone. }
procedure DeleteCredentials();
var
  ListFile, Line, Target: String;
  Lines: TArrayOfString;
  I, P, Code: Integer;
begin
  ListFile := AddBackslash(GetTempDir()) + 'openview-uninstall-creds.txt';
  if not Exec(ExpandConstant('{cmd}'),
      '/c cmdkey /list:OpenView:* > "' + ListFile + '"',
      '', SW_HIDE, ewWaitUntilTerminated, Code) then
    exit;
  if not LoadStringsFromFile(ListFile, Lines) then
    exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Line := Lines[I];
    P := Pos('OpenView:', Line);
    if P > 0 then
    begin
      Target := Trim(Copy(Line, P, Length(Line)));
      { Only plain names: never pass anything with quotes or spaces on. }
      if (Pos('"', Target) = 0) and (Pos(' ', Target) = 0) then
        Exec(ExpandConstant('{sys}\cmdkey.exe'), '/delete:' + Target, '',
          SW_HIDE, ewWaitUntilTerminated, Code);
    end;
  end;
  DeleteFile(ListFile);
end;

procedure RemoveFirewallRules();
var
  Flag: Cardinal;
  Code: Integer;
begin
  if RegQueryDWordValue(HKCU, StateKey, 'FirewallRules', Flag) and (Flag = 1) then
  begin
    if FileExists(ExpandConstant('{app}\firewall.cmd')) then
      ShellExec('runas', ExpandConstant('{app}\firewall.cmd'), 'remove', '',
        SW_HIDE, ewWaitUntilTerminated, Code);
  end;
end;

{ Remove the Run value only if it points at this installation (a portable
  copy elsewhere may own it). }
procedure RemoveAutostart();
var
  Data: String;
begin
  if RegQueryStringValue(HKCU, RunKey, RunValue, Data) then
    if Pos(Lowercase(ExpandConstant('{app}\{#AppExe}')), Lowercase(Data)) > 0 then
      RegDeleteValue(HKCU, RunKey, RunValue);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    QuitRunningInstance(ExpandConstant('{app}'));
    RemoveAutostart();
    RemoveFirewallRules();
  end;
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteKeyIncludingSubkeys(HKCU, StateKey);
    RegDeleteKeyIfEmpty(HKCU, 'Software\OpenView');
    DataDir := ExpandConstant('{localappdata}\OpenView');
    { Silent uninstalls keep user data. }
    if not UninstallSilent() then
      if MsgBox('Also delete your OpenView data?' + #13#10 + #13#10 +
          'This removes settings, saved partners, logs and this PC''s device ID (' +
          DataDir + '), and the PINs OpenView saved in Windows Credential Manager.' + #13#10 + #13#10 +
          'Keep them if you plan to reinstall: the device ID stays the same.',
          mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        DelTree(DataDir, True, True, True);
        DeleteCredentials();
      end;
  end;
end;

; Facial installer (WP-025) - compiled by product/scripts/package-release.ps1 via ISCC.
; The packaging script passes:
;   /DAppVersion=<ver>  /DPayloadDir=<staged payload>  /DOutputDir=<installer/out>
; Produces facial-setup-<AppVersion>.exe.
;
; Layout: read-only assets install under %ProgramFiles%\Facial; the launcher points the app
; at a per-user writable data dir (%LOCALAPPDATA%\Facial) for settings + projects.
;
; Modes (shown only when an existing install is detected), least -> most destructive:
;   Update (default) | Soft reinstall | Full reinstall | Uninstall

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef PayloadDir
  #define PayloadDir "payload"
#endif
#ifndef OutputDir
  #define OutputDir "out"
#endif

#define AppName "Facial"
#define AppExe "facial.exe"

[Setup]
AppId={{8F2A9C7E-3B41-4D6E-9A1F-FAC1A100D025}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Facial
DefaultDirName={autopf}\Facial
DisableProgramGroupPage=yes
DefaultGroupName=Facial
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=facial-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}

[Files]
Source: "{#PayloadDir}\facial.exe";        DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\launch-facial.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\product\*";          DestDir: "{app}\product"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Facial";           Filename: "{app}\launch-facial.cmd"; WorkingDir: "{app}"; IconFilename: "{app}\{#AppExe}"
Name: "{group}\Uninstall Facial"; Filename: "{uninstallexe}"

[Code]
const
  MODE_UPDATE = 0;
  MODE_SOFT   = 1;
  MODE_FULL   = 2;
  MODE_UNINST = 3;
  UninstKey   = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8F2A9C7E-3B41-4D6E-9A1F-FAC1A100D025}_is1';

var
  ModePage: TInputOptionWizardPage;

function DataDir(): String;
begin
  Result := ExpandConstant('{localappdata}\Facial');
end;

function SettingsFile(): String;
begin
  Result := DataDir() + '\config\default.json';
end;

function PriorUninstaller(): String;
var
  s: String;
begin
  s := '';
  if not RegQueryStringValue(HKLM, UninstKey, 'UninstallString', s) then
    RegQueryStringValue(HKCU, UninstKey, 'UninstallString', s);
  Result := s;
end;

function IsUpgrade(): Boolean;
begin
  Result := (PriorUninstaller() <> '') or DirExists(ExpandConstant('{autopf}\Facial'));
end;

function SelectedMode(): Integer;
begin
  if IsUpgrade() then
    Result := ModePage.SelectedValueIndex
  else
    Result := MODE_UPDATE;
end;

{ Read workspace_root from the user settings JSON (simple string scan). }
function ReadWorkspaceRoot(): String;
var
  rawA: AnsiString;
  raw, ws: String;
  p, q: Integer;
begin
  Result := '';
  if not FileExists(SettingsFile()) then exit;
  if not LoadStringFromFile(SettingsFile(), rawA) then exit;
  raw := String(rawA);
  p := Pos('"workspace_root"', raw);
  if p = 0 then exit;
  raw := Copy(raw, p + 16, Length(raw));
  p := Pos('"', raw); if p = 0 then exit;
  raw := Copy(raw, p + 1, Length(raw));
  q := Pos('"', raw); if q = 0 then exit;
  ws := Copy(raw, 1, q - 1);
  StringChangeEx(ws, '\\', '\', True); { unescape JSON backslashes }
  Result := ws;
end;

{ Delete a relocated workspace only with explicit per-item confirmation (default = keep). }
procedure MaybeDeleteRelocatedWorkspace();
var
  ws: String;
begin
  ws := ReadWorkspaceRoot();
  if ws = '' then exit;
  if CompareText(ws, DataDir()) = 0 then exit;        { not relocated }
  if not DirExists(ws) then exit;
  if MsgBox('You configured a workspace in a different location:' + #13#10 + ws + #13#10#13#10
            + 'Delete it and ALL its projects too?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    DelTree(ws, True, True, True);
end;

procedure InitializeWizard();
begin
  ModePage := CreateInputOptionPage(wpWelcome,
    'Install mode', 'An existing Facial installation was detected.',
    'Choose how to proceed (top is safest):', True, False);
  ModePage.Add('Update - refresh the program, KEEP settings and projects');
  ModePage.Add('Soft reinstall - clean program install, KEEP settings and projects');
  ModePage.Add('Full reinstall - clean program install, DELETE settings and projects');
  ModePage.Add('Uninstall - remove Facial, DELETE settings and projects');
  ModePage.SelectedValueIndex := MODE_UPDATE;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = ModePage.ID) and (not IsUpgrade());
end;

{ Uninstall mode: run the existing uninstaller, then stop setup (non-empty result aborts). }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  rc: Integer;
  unins: String;
begin
  Result := '';
  if SelectedMode() = MODE_UNINST then
  begin
    unins := RemoveQuotes(PriorUninstaller());
    if unins <> '' then
      Exec(unins, '', '', SW_SHOW, ewWaitUntilTerminated, rc)
    else
    begin
      { No registered uninstaller; remove what we can directly. }
      MaybeDeleteRelocatedWorkspace();
      DelTree(DataDir(), True, True, True);
      DelTree(ExpandConstant('{autopf}\Facial'), True, True, True);
    end;
    Result := 'Facial has been uninstalled. Setup will now close.';
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  mode: Integer;
begin
  if CurStep <> ssInstall then exit;
  mode := SelectedMode();
  { Soft + Full start from a clean program tree (drop orphaned asset files). }
  if (mode = MODE_SOFT) or (mode = MODE_FULL) then
    DelTree(ExpandConstant('{app}\product'), True, True, True);
  { Full also deletes user data, with the relocated-workspace prompt first. }
  if mode = MODE_FULL then
  begin
    MaybeDeleteRelocatedWorkspace();
    DelTree(DataDir(), True, True, True);
  end;
end;

{ Add/Remove Programs uninstall: offer to delete settings + projects (spec: uninstall is
  the most destructive mode). Relocated workspaces are a separate, explicit per-item prompt. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep <> usUninstall then exit;
  if not DirExists(DataDir()) then exit;
  if UninstallSilent() or
     (MsgBox('Also delete Facial settings and projects?' + #13#10 + DataDir(),
             mbConfirmation, MB_YESNO) = IDYES) then
  begin
    if not UninstallSilent() then
      MaybeDeleteRelocatedWorkspace();
    DelTree(DataDir(), True, True, True);
  end;
end;

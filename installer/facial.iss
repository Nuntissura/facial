; Facial installer (WP-025) - compiled by product/scripts/package-release.ps1 via ISCC.
; The packaging script passes:
;   /DAppVersion=<ver>  /DPayloadDir=<staged payload>  /DOutputDir=<transient output>
; package-release.ps1 publishes the result as the sole current installer beside the
; sole current portable EXE in installer/, then archives the superseded pair.
;
; Layout: read-only assets install under %ProgramFiles%\Facial. facial.exe resolves
; its per-user writable settings + workspace under %LOCALAPPDATA%\Facial directly.
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
  #define OutputDir "."
#endif

#define AppName "Facial"
#define AppExe "facial.exe"
#define InstallerSourceSha256 GetSHA256OfFile(SourcePath + "\facial.iss")
#define InstallerSourceSha256First Copy(InstallerSourceSha256, 1, 32)
#define InstallerSourceSha256Last Copy(InstallerSourceSha256, 33, 32)
#define ReleaseDefaultConfigSha256 GetSHA256OfFile(PayloadDir + "\product\config\default.json")
#define RetirementToolSha256 GetSHA256OfFile(PayloadDir + "\product\scripts\retire-legacy-media-db.ps1")

[Setup]
AppId={{8F2A9C7E-3B41-4D6E-9A1F-FAC1A100D025}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Facial
VersionInfoDescription=Facial sha256[0:32] {#InstallerSourceSha256First}
VersionInfoProductName=Facial sha256[32:64] {#InstallerSourceSha256Last}
DefaultDirName={autopf}\Facial
DisableProgramGroupPage=yes
DefaultGroupName=Facial
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline
RedirectionGuard=yes
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=facial-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}

[Files]
; Validator-only payload copies. Setup can extract these in InitializeSetup
; without installing, launching, registering, or touching the live app.
Source: "{#PayloadDir}\facial.exe";     DestName: "_verify-facial.exe";     Flags: dontcopy noencryption
Source: "{#PayloadDir}\facial-cli.exe"; DestName: "_verify-facial-cli.exe"; Flags: dontcopy noencryption
Source: "{#PayloadDir}\product\scripts\retire-legacy-media-db.ps1"; DestName: "_verify-retire-legacy-media-db.ps1"; Flags: dontcopy noencryption
Source: "{#PayloadDir}\product\config\default.json"; DestName: "_verify-release-default-config.json"; Flags: dontcopy noencryption
Source: "{#PayloadDir}\facial.exe";        DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\facial-cli.exe";    DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\product\*";          DestDir: "{app}\product"; Excludes: "scripts\test-retire-legacy-media-db.ps1"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; Remove the retired console-launch wrapper from upgrades of older installs.
Type: files; Name: "{app}\launch-facial.cmd"

[Tasks]
Name: "startmenuicon"; Description: "Add Facial to the Windows &Start menu (All apps)"; GroupDescription: "Shortcuts:"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Icons]
Name: "{group}\Facial";           Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: startmenuicon
Name: "{group}\Uninstall Facial"; Filename: "{uninstallexe}"; Tasks: startmenuicon
Name: "{commondesktop}\Facial";   Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch Facial"; WorkingDir: "{app}"; Flags: postinstall nowait skipifsilent runasoriginaluser

[Code]
const
  MODE_UPDATE = 0;
  MODE_SOFT   = 1;
  MODE_FULL   = 2;
  MODE_UNINST = 3;
  FACIAL_VERIFY_EXIT_CODE = 1;
  UninstKey   = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8F2A9C7E-3B41-4D6E-9A1F-FAC1A100D025}_is1';
  RetirementArchiveName = '.facial-media-retirement';
  DataDirHoldName = 'Facial.wp079-data-dir-hold';
  PriorUninstallArguments = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /REDIRECTIONGUARD';
  PriorUninstallCleanupAttempts = 100;
  PriorUninstallCleanupDelayMs = 100;
  INVALID_FILE_ATTRIBUTES = $FFFFFFFF;
  ERROR_FILE_NOT_FOUND = 2;
  ERROR_PATH_NOT_FOUND = 3;

var
  ModePage: TInputOptionWizardPage;

function GetFileAttributesW(lpFileName: String): Cardinal;
  external 'GetFileAttributesW@kernel32.dll stdcall';

function ModeCleansProgramTree(Mode: Integer): Boolean;
begin
  Result := (Mode = MODE_SOFT) or (Mode = MODE_FULL);
end;

function ModeDeletesUserData(Mode: Integer): Boolean;
begin
  Result := Mode = MODE_FULL;
end;

function ModeDeletesRelocatedState(Mode: Integer): Boolean;
begin
  Result := Mode = MODE_FULL;
end;

function IsRetirementArchiveChild(Name: String): Boolean;
begin
  Result := CompareText(Name, RetirementArchiveName) = 0;
end;

function IsHexString(Value: String): Boolean;
var
  I: Integer;
begin
  Result := Value <> '';
  for I := 1 to Length(Value) do
    if Pos(Copy(Value, I, 1), '0123456789abcdefABCDEF') = 0 then
    begin
      Result := False;
      exit;
    end;
end;

function IsSafeVerifierDestination(Path: String): Boolean;
var
  NormalizedPath: String;
  TempRoot: String;
  Leaf: String;
  Suffix: String;
begin
  Result := False;
  if Path = '' then exit;
  NormalizedPath := RemoveBackslashUnlessRoot(ExpandFileName(Path));
  { The setup temp constant may use an 8.3 alias while the caller uses the long
    path. LocalAppData\Temp is the stable user-owned boundary for this verifier. }
  TempRoot := RemoveBackslashUnlessRoot(
    ExpandFileName(ExpandConstant('{localappdata}\Temp')));
  if CompareText(Path, NormalizedPath) <> 0 then exit;
  if CompareText(Copy(NormalizedPath, 1, Length(TempRoot) + 1),
    AddBackslash(TempRoot)) <> 0 then exit;
  if CompareText(RemoveBackslashUnlessRoot(ExtractFileDir(NormalizedPath)),
    TempRoot) <> 0 then exit;
  Leaf := ExtractFileName(NormalizedPath);
  if CompareText(Copy(Leaf, 1, 24), 'facial-installer-verify-') <> 0 then exit;
  Suffix := Copy(Leaf, 25, Length(Leaf));
  Result := (Length(Suffix) = 32) and IsHexString(Suffix);
end;

procedure AssertVerifierTargetAbsent(VerifyDir: String; Name: String);
var
  Target: String;
begin
  Target := AddBackslash(VerifyDir) + Name;
  if FileExists(Target) or DirExists(Target) then
    RaiseException('FACIALVERIFY refuses to overwrite the pre-existing export target: ' + Target);
end;

function InitializeSetup: Boolean;
var
  VerifyDir: String;
  GuiSource: String;
  CliSource: String;
  DefaultConfigSource: String;
  ContractSource: String;
  ReceiptSource: String;
begin
  VerifyDir := ExpandConstant('{param:FACIALVERIFY|}');
  if VerifyDir <> '' then
  begin
    if not IsSafeVerifierDestination(VerifyDir) then
      RaiseException('FACIALVERIFY requires an exact facial-installer-verify-<32-hex> ' +
        'directory directly below the current temporary root: ' + VerifyDir);
    VerifyDir := RemoveBackslashUnlessRoot(ExpandFileName(VerifyDir));
    if FileExists(VerifyDir) or DirExists(VerifyDir) then
      RaiseException('FACIALVERIFY requires a new export directory and refuses the pre-existing path: ' +
        VerifyDir);
    AssertVerifierTargetAbsent(VerifyDir, 'facial.exe');
    AssertVerifierTargetAbsent(VerifyDir, 'facial-cli.exe');
    AssertVerifierTargetAbsent(VerifyDir, 'retire-legacy-media-db.ps1');
    AssertVerifierTargetAbsent(VerifyDir, 'release-default-config.json');
    AssertVerifierTargetAbsent(VerifyDir, 'update-mode-contract.txt');
    AssertVerifierTargetAbsent(VerifyDir, 'facialverify-receipt.json');
    if not ForceDirectories(VerifyDir) then
      RaiseException('Could not create the FACIALVERIFY export directory: ' + VerifyDir);
    ExtractTemporaryFile('_verify-facial.exe');
    ExtractTemporaryFile('_verify-facial-cli.exe');
    ExtractTemporaryFile('_verify-retire-legacy-media-db.ps1');
    ExtractTemporaryFile('_verify-release-default-config.json');
    GuiSource := ExpandConstant('{tmp}\_verify-facial.exe');
    CliSource := ExpandConstant('{tmp}\_verify-facial-cli.exe');
    DefaultConfigSource := ExpandConstant('{tmp}\_verify-release-default-config.json');
    if not CopyFile(GuiSource, VerifyDir + '\facial.exe', True) then
      RaiseException('Could not export the packaged facial.exe payload.');
    if not CopyFile(CliSource, VerifyDir + '\facial-cli.exe', True) then
      RaiseException('Could not export the packaged facial-cli.exe payload.');
    if not CopyFile(ExpandConstant('{tmp}\_verify-retire-legacy-media-db.ps1'),
      VerifyDir + '\retire-legacy-media-db.ps1', True) then
      RaiseException('Could not export the packaged WP-079 retirement tool.');
    if not CopyFile(DefaultConfigSource, VerifyDir + '\release-default-config.json', True) then
      RaiseException('Could not export the compiled release default configuration.');
    if ModeCleansProgramTree(MODE_UPDATE) or ModeDeletesUserData(MODE_UPDATE) or
       ModeDeletesRelocatedState(MODE_UPDATE) then
      RaiseException('Update mode must preserve program-tree or user-data state.');
    if (not ModeCleansProgramTree(MODE_SOFT)) or ModeDeletesUserData(MODE_SOFT) or
       ModeDeletesRelocatedState(MODE_SOFT) then
      RaiseException('Soft reinstall mode contract is inconsistent.');
    if (not ModeCleansProgramTree(MODE_FULL)) or (not ModeDeletesUserData(MODE_FULL)) or
       (not ModeDeletesRelocatedState(MODE_FULL)) then
      RaiseException('Full reinstall mode contract is inconsistent.');
    if (not IsRetirementArchiveChild('.facial-media-retirement')) or
       IsRetirementArchiveChild('.facial') then
      RaiseException('Retirement-archive preservation predicate is inconsistent.');
    ContractSource := ExpandConstant('{tmp}\_verify-update-mode-contract.txt');
    if not SaveStringToFile(ContractSource,
      'update_cleans_program_tree=false' + #13#10 +
      'update_deletes_user_data=false' + #13#10 +
      'update_deletes_relocated_state=false' + #13#10 +
      'soft_cleans_program_tree=true' + #13#10 +
      'soft_deletes_user_data=false' + #13#10 +
      'full_cleans_program_tree=true' + #13#10 +
      'full_deletes_user_data=true' + #13#10 +
      'relocated_delete_target=.facial' + #13#10 +
      'relocated_reparse_points_forbidden=true' + #13#10 +
      'default_data_delete_targets=.facial|config\default.json' + #13#10 +
      'default_data_empty_directory_cleanup=config|data-root' + #13#10 +
      'default_data_unknown_siblings_preserved=true' + #13#10 +
      'default_data_ancestor_delete_forbidden=true' + #13#10 +
      'default_data_all_children_delete_forbidden=true' + #13#10 +
      'default_data_reparse_points_forbidden=true' + #13#10 +
      'retirement_tool_packaged=true' + #13#10 +
      'retirement_tool_sha256={#RetirementToolSha256}' + #13#10 +
      'retirement_test_script_packaged=false' + #13#10 +
      'retirement_archive_preserved=true' + #13#10 +
      'verifier_destination_contract=temp-root\facial-installer-verify-<32-hex>' + #13#10 +
      'verifier_overwrite_policy=refuse-all-export-targets' + #13#10, False) then
      RaiseException('Could not create the compiled update-mode contract.');
    if not CopyFile(ContractSource, VerifyDir + '\update-mode-contract.txt', True) then
      RaiseException('Could not export the compiled update-mode contract without overwrite.');
    ReceiptSource := ExpandConstant('{tmp}\_verify-facialverify-receipt.json');
    if not SaveStringToFile(ReceiptSource,
      '{' + #13#10 +
      '  "installer_source_sha256": "{#InstallerSourceSha256}",' + #13#10 +
      '  "verifier_exit_code": ' + IntToStr(FACIAL_VERIFY_EXIT_CODE) + ',' + #13#10 +
      '  "release_default_config_name": "release-default-config.json",' + #13#10 +
      '  "release_default_config_sha256": "{#ReleaseDefaultConfigSha256}",' + #13#10 +
      '  "update_cleans_program_tree": false,' + #13#10 +
      '  "update_deletes_user_data": false,' + #13#10 +
      '  "update_deletes_relocated_state": false,' + #13#10 +
      '  "soft_cleans_program_tree": true,' + #13#10 +
      '  "soft_deletes_user_data": false,' + #13#10 +
      '  "full_cleans_program_tree": true,' + #13#10 +
      '  "full_deletes_user_data": true,' + #13#10 +
      '  "relocated_delete_target": ".facial",' + #13#10 +
      '  "relocated_reparse_points_forbidden": true,' + #13#10 +
      '  "default_data_delete_targets": [".facial", "config\\default.json"],' + #13#10 +
      '  "default_data_empty_directory_cleanup": ["config", "data-root"],' + #13#10 +
      '  "default_data_unknown_siblings_preserved": true,' + #13#10 +
      '  "default_data_ancestor_delete_forbidden": true,' + #13#10 +
      '  "default_data_all_children_delete_forbidden": true,' + #13#10 +
      '  "default_data_reparse_points_forbidden": true,' + #13#10 +
      '  "retirement_tool_packaged": true,' + #13#10 +
      '  "retirement_tool_sha256": "{#RetirementToolSha256}",' + #13#10 +
      '  "retirement_test_script_packaged": false,' + #13#10 +
      '  "retirement_archive_preserved": true,' + #13#10 +
      '  "verifier_destination_contract": "temp-root\\facial-installer-verify-<32-hex>",' + #13#10 +
      '  "verifier_overwrite_policy": "refuse-all-export-targets",' + #13#10 +
      '  "prior_uninstaller_arguments": "' + PriorUninstallArguments + '",' + #13#10 +
      '  "prior_uninstaller_exit_required": 0,' + #13#10 +
      '  "prior_uninstaller_cleanup_check": "registry-and-shortcuts",' + #13#10 +
      '  "prior_uninstaller_registry_provenance": "same-root-key-view",' + #13#10 +
      '  "prior_uninstaller_exact_executable": "InstallLocation\\unins000.exe",' + #13#10 +
      '  "prior_uninstaller_hkcu_execution": "ExecAsOriginalUser",' + #13#10 +
      '  "prior_uninstaller_hkcu_trust_boundary": "LocalAppData",' + #13#10 +
      '  "prior_uninstaller_hklm_execution": "Exec",' + #13#10 +
      '  "prior_uninstaller_hklm_trust_boundary": "ProgramFiles",' + #13#10 +
      '  "prior_uninstaller_data_dir_hold": "' + DataDirHoldName + '",' + #13#10 +
      '  "prior_uninstaller_data_dir_restore": "finally-before-exact-cleanup",' + #13#10 +
      '  "prior_uninstaller_data_dir_postcondition": "source-present-hold-absent",' + #13#10 +
      '  "prior_uninstaller_cleanup_order": "restore-then-exact-app-owned-targets"' + #13#10 +
      '}' + #13#10, False) then
      RaiseException('Could not create the compiled FACIALVERIFY receipt.');
    if not CopyFile(ReceiptSource, VerifyDir + '\facialverify-receipt.json', True) then
      RaiseException('Could not export the compiled FACIALVERIFY receipt without overwrite.');
    Result := False;
    exit;
  end;
  Result := True;
end;

function DataDir(): String;
begin
  Result := ExpandConstant('{localappdata}\Facial');
end;

function SettingsFile(): String;
begin
  Result := DataDir() + '\config\default.json';
end;

function AnyPathExists(Path: String): Boolean;
begin
  Result := FileExists(Path) or DirExists(Path);
end;

function GetPathAttributesOrAbsent(Path: String; Description: String;
  var Attributes: Cardinal): Boolean;
var
  ErrorCode: LongInt;
begin
  Attributes := GetFileAttributesW(Path);
  if Attributes <> INVALID_FILE_ATTRIBUTES then
  begin
    Result := True;
    exit;
  end;

  ErrorCode := DLLGetLastError;
  if (ErrorCode = ERROR_FILE_NOT_FOUND) or (ErrorCode = ERROR_PATH_NOT_FOUND) then
  begin
    Result := False;
    exit;
  end;

  RaiseException('Refusing user-data cleanup because the ' + Description +
    ' path attributes could not be inspected (Windows error ' +
    IntToStr(ErrorCode) + '): ' + Path);
end;

{ Check the exact lexical path and every existing ancestor up to the volume
  root. A normal final target is not safe when DataRoot or config is a junction:
  a privileged delete would otherwise traverse that ancestor into another tree. }
procedure AssertPathAndExistingAncestorsNotReparse(Path: String;
  Description: String);
var
  CurrentPath: String;
  ParentPath: String;
  Attributes: Cardinal;
begin
  CurrentPath := RemoveBackslashUnlessRoot(ExpandFileName(Path));
  if CurrentPath = '' then
    RaiseException('Refusing user-data cleanup because the ' + Description +
      ' path is empty.');

  while CurrentPath <> '' do
  begin
    if GetPathAttributesOrAbsent(CurrentPath, Description, Attributes) and
       ((Attributes and FILE_ATTRIBUTE_REPARSE_POINT) <> 0) then
      RaiseException('Refusing user-data cleanup because the ' + Description +
        ' path or an ancestor is a reparse point: ' + CurrentPath);

    ParentPath := RemoveBackslashUnlessRoot(ExtractFileDir(CurrentPath));
    if (ParentPath = '') or (CompareText(ParentPath, CurrentPath) = 0) then
      exit;
    CurrentPath := ParentPath;
  end;
end;

procedure AssertDefaultDataCleanupPathsNotReparse(DataRoot: String;
  ConfigDir: String; ManagedStateDir: String; ConfigFile: String);
begin
  AssertPathAndExistingAncestorsNotReparse(DataRoot, 'Facial data-root');
  AssertPathAndExistingAncestorsNotReparse(ConfigDir, 'Facial config-directory');
  AssertPathAndExistingAncestorsNotReparse(ManagedStateDir,
    'Facial managed-state');
  AssertPathAndExistingAncestorsNotReparse(ConfigFile, 'Facial settings-file');
end;

function DirectoryHasChildren(Path: String): Boolean;
var
  FindRec: TFindRec;
begin
  Result := False;
  if not FindFirst(AddBackslash(Path) + '*', FindRec) then exit;
  try
    repeat
      if (FindRec.Name <> '.') and (FindRec.Name <> '..') then
      begin
        Result := True;
        exit;
      end;
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

procedure RemoveDirectoryIfEmptyChecked(Path: String; Description: String);
begin
  if not AnyPathExists(Path) then exit;
  AssertPathAndExistingAncestorsNotReparse(Path, Description);
  if not DirExists(Path) then
    RaiseException('Refusing empty-directory cleanup because the exact ' + Description +
      ' target is not a directory: ' + Path);
  if DirectoryHasChildren(Path) then exit;
  if not RemoveDir(Path) then
    RaiseException('Could not remove the empty ' + Description + ': ' + Path);
  if AnyPathExists(Path) then
    RaiseException('Empty ' + Description + ' cleanup postcondition failed: ' + Path);
end;

{ Full and uninstall delete only the two exact app-owned user-data targets.
  Unknown siblings, configured workspace roots, raw files, and the WP-079
  recovery archive are never enumerated as deletion candidates. }
procedure DeleteDefaultAppOwnedDataChecked();
var
  DataRoot: String;
  ManagedStateDir: String;
  ConfigDir: String;
  ConfigFile: String;
  ManagedStateWasPresent: Boolean;
  ConfigFileWasPresent: Boolean;
  ConfigDirWasPresent: Boolean;
  DataRootWasPresent: Boolean;
begin
  DataRoot := RemoveBackslashUnlessRoot(ExpandFileName(DataDir()));
  DataRootWasPresent := AnyPathExists(DataRoot);
  if DataRootWasPresent and (not DirExists(DataRoot)) then
    RaiseException('Refusing user-data cleanup because the Facial data root is not a directory: ' +
      DataRoot);

  ManagedStateDir := RemoveBackslashUnlessRoot(
    ExpandFileName(AddBackslash(DataRoot) + '.facial'));
  if CompareText(ManagedStateDir, AddBackslash(DataRoot) + '.facial') <> 0 then
    RaiseException('Refusing cleanup because the managed-state target is not the exact ' +
      'DataDir\.facial path: ' + ManagedStateDir);
  ManagedStateWasPresent := AnyPathExists(ManagedStateDir);
  if ManagedStateWasPresent and (not DirExists(ManagedStateDir)) then
    RaiseException('Refusing cleanup because the exact DataDir\.facial target is not a directory: ' +
      ManagedStateDir);

  ConfigDir := RemoveBackslashUnlessRoot(
    ExpandFileName(AddBackslash(DataRoot) + 'config'));
  ConfigFile := RemoveBackslashUnlessRoot(
    ExpandFileName(AddBackslash(ConfigDir) + 'default.json'));
  if CompareText(ConfigFile, AddBackslash(DataRoot) + 'config\default.json') <> 0 then
    RaiseException('Refusing cleanup because the settings target is not the exact ' +
      'DataDir\config\default.json path: ' + ConfigFile);
  ConfigDirWasPresent := AnyPathExists(ConfigDir);
  if ConfigDirWasPresent and (not DirExists(ConfigDir)) then
    RaiseException('Refusing cleanup because the Facial config path is not a directory: ' +
      ConfigDir);
  ConfigFileWasPresent := AnyPathExists(ConfigFile);
  if ConfigFileWasPresent and (not FileExists(ConfigFile)) then
    RaiseException('Refusing cleanup because the exact DataDir\config\default.json target ' +
      'is not a file: ' + ConfigFile);

  { All target paths, types, and ancestor attributes are proven before the
    first mutation. Recheck the complete set immediately before each exact
    destructive call to narrow the remaining filesystem race window. }
  AssertDefaultDataCleanupPathsNotReparse(DataRoot, ConfigDir,
    ManagedStateDir, ConfigFile);
  if ManagedStateWasPresent then
  begin
    if not DirExists(ManagedStateDir) then
      RaiseException('The exact Facial managed-state directory changed after preflight: ' +
        ManagedStateDir);
    AssertDefaultDataCleanupPathsNotReparse(DataRoot, ConfigDir,
      ManagedStateDir, ConfigFile);
    if not DelTree(ManagedStateDir, True, True, True) then
      RaiseException('Could not remove the exact Facial managed-state directory: ' +
        ManagedStateDir);
    if AnyPathExists(ManagedStateDir) then
      RaiseException('Managed-state cleanup postcondition failed: ' + ManagedStateDir);
  end;

  if ConfigFileWasPresent then
  begin
    if not FileExists(ConfigFile) then
      RaiseException('The exact Facial settings file changed after preflight: ' + ConfigFile);
    AssertDefaultDataCleanupPathsNotReparse(DataRoot, ConfigDir,
      ManagedStateDir, ConfigFile);
    if not DeleteFile(ConfigFile) then
      RaiseException('Could not remove the exact Facial settings file: ' + ConfigFile);
    if AnyPathExists(ConfigFile) then
      RaiseException('Settings-file cleanup postcondition failed: ' + ConfigFile);
  end;

  { Structural cleanup is empty-only. Unknown siblings make the directory
    non-empty, in which case it is preserved without mutation. }
  if ConfigDirWasPresent then
    RemoveDirectoryIfEmptyChecked(ConfigDir, 'Facial config directory');
  if DataRootWasPresent then
    RemoveDirectoryIfEmptyChecked(DataRoot, 'Facial data root');
end;

function DefaultDataDirHoldPath(): String;
begin
  Result := AddBackslash(ExpandConstant('{localappdata}')) + DataDirHoldName;
end;

procedure HoldDefaultDataDir(var DataDirHeld: Boolean; HoldPath: String);
begin
  DataDirHeld := False;
  if AnyPathExists(HoldPath) then
    RaiseException('A prior Facial data-directory hold already exists. Refusing to overwrite it: ' +
      HoldPath);
  if not AnyPathExists(DataDir()) then exit;
  if not DirExists(DataDir()) then
    RaiseException('Refusing predecessor compatibility mode because the Facial data root is ' +
      'not a directory: ' + DataDir());
  if not RenameFile(DataDir(), HoldPath) then
    RaiseException('Could not move the entire Facial data directory into its compatibility hold: ' +
      HoldPath);
  { Set this immediately so the caller's finally block restores the directory even
    if a postcondition check below fails. }
  DataDirHeld := True;
  if AnyPathExists(DataDir()) or (not DirExists(HoldPath)) then
    RaiseException('The Facial data-directory hold postcondition failed: ' + HoldPath);
end;

function RestoreDefaultDataDir(DataDirHeld: Boolean; HoldPath: String;
  var ErrorText: String): Boolean;
begin
  Result := True;
  ErrorText := '';
  if not DataDirHeld then exit;
  if not AnyPathExists(HoldPath) then
  begin
    ErrorText := 'The entire data-directory hold is missing and could not be restored: ' + HoldPath;
    Result := False;
    exit;
  end;
  if not DirExists(HoldPath) then
  begin
    ErrorText := 'The recoverable data-directory hold is not a directory: ' + HoldPath;
    Result := False;
    exit;
  end;
  if AnyPathExists(DataDir()) then
  begin
    ErrorText := 'The Facial data-directory destination is occupied; the entire preserved ' +
      'directory remains recoverable at: ' + HoldPath;
    Result := False;
    exit;
  end;
  if not RenameFile(HoldPath, DataDir()) then
  begin
    ErrorText := 'Could not restore the entire Facial data directory; it remains recoverable at: ' +
      HoldPath;
    Result := False;
    exit;
  end;
  if (not DirExists(DataDir())) or AnyPathExists(HoldPath) then
  begin
    ErrorText := 'Data-directory restore postconditions failed. Inspect the recoverable hold: ' +
      HoldPath;
    Result := False;
  end;
end;

procedure AssertDefaultDataDirPostconditions(DataDirWasHeld: Boolean;
  HoldPath: String);
begin
  if AnyPathExists(HoldPath) then
    RaiseException('The whole-data-directory compatibility hold was not cleared: ' + HoldPath);
  if DataDirWasHeld and (not DirExists(DataDir())) then
    RaiseException('The Facial data directory was not restored from the recoverable hold: ' +
      HoldPath);
end;

function NormalizeRegisteredPath(Value: String; Description: String): String;
var
  Candidate: String;
begin
  Candidate := Trim(Value);
  if Candidate = '' then
    RaiseException('The registered Facial ' + Description + ' is empty.');
  if Candidate[1] = '"' then
  begin
    if (Length(Candidate) < 2) or (Candidate[Length(Candidate)] <> '"') then
      RaiseException('The registered Facial ' + Description + ' has malformed quoting.');
    Candidate := Copy(Candidate, 2, Length(Candidate) - 2);
    if Pos('"', Candidate) <> 0 then
      RaiseException('The registered Facial ' + Description + ' contains embedded quoting or arguments.');
  end
  else if Pos('"', Candidate) <> 0 then
    RaiseException('The registered Facial ' + Description + ' contains malformed quoting or arguments.');
  if Length(Candidate) < 3 then
    RaiseException('The registered Facial ' + Description + ' is not an absolute drive path.');
  if Pos('/', Candidate) <> 0 then
    RaiseException('The registered Facial ' + Description + ' must use a canonical Windows path.');
  if (Pos(':', Copy(Candidate, 3, Length(Candidate))) <> 0) or
     (Pos('*', Candidate) <> 0) or (Pos('?', Candidate) <> 0) then
    RaiseException('The registered Facial ' + Description + ' contains a forbidden path token.');
  if (Pos('\..\', Candidate) <> 0) or (Pos('\.\', Candidate) <> 0) or
     (Copy(Candidate, 1, 3) = '..\') or (Copy(Candidate, 1, 2) = '.\') or
     (Copy(Candidate, Length(Candidate) - 2, 3) = '\..') or
     (Copy(Candidate, Length(Candidate) - 1, 2) = '\.') then
    RaiseException('The registered Facial ' + Description + ' contains a path-escape segment.');
  if (Candidate[2] <> ':') or (Candidate[3] <> '\') then
    RaiseException('The registered Facial ' + Description + ' is not an absolute drive path.');
  Result := RemoveBackslashUnlessRoot(ExpandFileName(Candidate));
end;

function IsPathBelow(Path: String; Root: String): Boolean;
var
  NormalizedPath: String;
  NormalizedRoot: String;
begin
  NormalizedPath := RemoveBackslashUnlessRoot(ExpandFileName(Path));
  NormalizedRoot := RemoveBackslashUnlessRoot(ExpandFileName(Root));
  Result := CompareText(Copy(NormalizedPath, 1, Length(NormalizedRoot) + 1),
    AddBackslash(NormalizedRoot)) = 0;
end;

function IsTrustedMachineInstallLocation(InstallLocation: String): Boolean;
begin
  Result := IsPathBelow(InstallLocation, ExpandConstant('{commonpf32}'));
  if IsWin64 then
    Result := Result or IsPathBelow(InstallLocation, ExpandConstant('{commonpf64}'));
end;

function ResolvePriorFromExactView(RootKey: Integer; CurrentUserOrigin: Boolean;
  ViewDescription: String; var Uninstaller: String): Boolean;
var
  RawInstallLocation: String;
  RawUninstaller: String;
  InstallLocation: String;
  ExpectedUninstaller: String;
begin
  Result := False;
  if not RegKeyExists(RootKey, UninstKey) then exit;
  if not RegQueryStringValue(RootKey, UninstKey, 'InstallLocation', RawInstallLocation) then
    RaiseException('The registered Facial predecessor is missing InstallLocation in ' +
      ViewDescription + '.');
  if not RegQueryStringValue(RootKey, UninstKey, 'UninstallString', RawUninstaller) then
    RaiseException('The registered Facial predecessor is missing UninstallString in ' +
      ViewDescription + '.');
  { Both values above come from the same exact root/key/view. Never combine a
    trusted InstallLocation from one registration with a command from another. }
  InstallLocation := NormalizeRegisteredPath(RawInstallLocation, 'InstallLocation');
  Uninstaller := NormalizeRegisteredPath(RawUninstaller, 'UninstallString');
  if not DirExists(InstallLocation) then
    RaiseException('The registered Facial predecessor InstallLocation is not a directory: ' +
      InstallLocation);
  ExpectedUninstaller := RemoveBackslashUnlessRoot(
    ExpandFileName(AddBackslash(InstallLocation) + 'unins000.exe'));
  if CompareText(ExtractFileName(Uninstaller), 'unins000.exe') <> 0 then
    RaiseException('The registered Facial predecessor command is not exactly unins000.exe: ' +
      Uninstaller);
  if CompareText(Uninstaller, ExpectedUninstaller) <> 0 then
    RaiseException('The registered Facial predecessor command does not exactly match ' +
      'InstallLocation\unins000.exe in ' + ViewDescription + '.');
  if CurrentUserOrigin then
  begin
    if not IsPathBelow(InstallLocation, ExpandConstant('{localappdata}')) then
      RaiseException('The HKCU Facial predecessor InstallLocation is outside LocalAppData: ' +
        InstallLocation);
  end
  else if not IsTrustedMachineInstallLocation(InstallLocation) then
    RaiseException('The HKLM Facial predecessor InstallLocation is outside Program Files: ' +
      InstallLocation);
  if not FileExists(Uninstaller) then
    RaiseException('The exact registered Facial predecessor uninstaller is missing: ' +
      Uninstaller);
  Result := True;
end;

function ResolvePriorUninstaller(var Uninstaller: String;
  var CurrentUserOrigin: Boolean): Boolean;
begin
  Uninstaller := '';
  CurrentUserOrigin := False;
  if IsWin64 and RegKeyExists(HKLM64, UninstKey) then
  begin
    Result := ResolvePriorFromExactView(HKLM64, False, 'HKLM64', Uninstaller);
    exit;
  end;
  if RegKeyExists(HKLM32, UninstKey) then
  begin
    Result := ResolvePriorFromExactView(HKLM32, False, 'HKLM32', Uninstaller);
    exit;
  end;
  if IsWin64 and RegKeyExists(HKCU64, UninstKey) then
  begin
    CurrentUserOrigin := True;
    Result := ResolvePriorFromExactView(HKCU64, True, 'HKCU64', Uninstaller);
    exit;
  end;
  if RegKeyExists(HKCU32, UninstKey) then
  begin
    CurrentUserOrigin := True;
    Result := ResolvePriorFromExactView(HKCU32, True, 'HKCU32', Uninstaller);
    exit;
  end;
  Result := False;
end;

function PriorUninstallRegistrationExists(): Boolean;
begin
  Result := RegKeyExists(HKLM32, UninstKey) or RegKeyExists(HKCU32, UninstKey);
  if IsWin64 then
    Result := Result or RegKeyExists(HKLM64, UninstKey) or RegKeyExists(HKCU64, UninstKey);
end;

function PriorShortcutExists(): Boolean;
begin
  Result :=
    FileExists(ExpandConstant('{commonprograms}\Facial\Facial.lnk')) or
    FileExists(ExpandConstant('{commonprograms}\Facial\Uninstall Facial.lnk')) or
    FileExists(ExpandConstant('{userprograms}\Facial\Facial.lnk')) or
    FileExists(ExpandConstant('{userprograms}\Facial\Uninstall Facial.lnk')) or
    FileExists(ExpandConstant('{commondesktop}\Facial.lnk')) or
    FileExists(ExpandConstant('{userdesktop}\Facial.lnk'));
end;

function WaitForPriorUninstallCleanup(): Boolean;
var
  Attempt: Integer;
begin
  for Attempt := 1 to PriorUninstallCleanupAttempts do
  begin
    if (not PriorUninstallRegistrationExists()) and (not PriorShortcutExists()) then
    begin
      Result := True;
      exit;
    end;
    Sleep(PriorUninstallCleanupDelayMs);
  end;
  Result := (not PriorUninstallRegistrationExists()) and (not PriorShortcutExists());
end;

function IsUpgrade(): Boolean;
var
  CurrentUserOrigin: Boolean;
  Uninstaller: String;
begin
  Result := ResolvePriorUninstaller(Uninstaller, CurrentUserOrigin);
  if not Result then
    Result := DirExists(ExpandConstant('{autopf}\Facial'));
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

function CachedRelocatedWorkspaceStateDir(): String;
var
  ws: String;
begin
  Result := '';
  ws := ReadWorkspaceRoot();
  if ws = '' then exit;
  ws := RemoveBackslashUnlessRoot(ExpandFileName(ws));
  if CompareText(ws, DataDir()) = 0 then exit;        { not relocated }
  if not DirExists(ws) then exit;
  Result := AddBackslash(ws) + '.facial';
  if not DirExists(Result) then
    Result := '';
end;

function ConfirmCachedRelocatedWorkspaceStateDeletion(var StateDir: String): Boolean;
begin
  StateDir := CachedRelocatedWorkspaceStateDir();
  Result := False;
  if StateDir = '' then exit;
  Result := MsgBox('Facial state exists in a relocated workspace:' + #13#10 + StateDir + #13#10#13#10
            + 'Delete this Facial state and its managed projects?' + #13#10
            + 'Raw media and all files outside .facial will be kept.' + #13#10
            + 'Recovery archives in .facial-media-retirement will be kept.',
            mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

procedure DeleteRelocatedWorkspaceStateChecked(StateDir: String);
var
  NormalizedStateDir: String;
  WorkspaceRoot: String;
begin
  if StateDir = '' then exit;
  NormalizedStateDir := RemoveBackslashUnlessRoot(ExpandFileName(StateDir));
  WorkspaceRoot := RemoveBackslashUnlessRoot(ExtractFileDir(NormalizedStateDir));
  if CompareText(ExtractFileName(NormalizedStateDir), '.facial') <> 0 then
    RaiseException('Refusing relocated-state cleanup outside an exact .facial directory: ' +
      NormalizedStateDir);
  if CompareText(NormalizedStateDir, AddBackslash(WorkspaceRoot) + '.facial') <> 0 then
    RaiseException('Refusing relocated-state cleanup because the target is not the exact ' +
      'workspace-root\.facial child: ' + NormalizedStateDir);
  if not AnyPathExists(NormalizedStateDir) then exit;
  if not DirExists(NormalizedStateDir) then
    RaiseException('The cached relocated .facial target is no longer a directory: ' +
      NormalizedStateDir);
  AssertPathAndExistingAncestorsNotReparse(WorkspaceRoot,
    'relocated Facial workspace-root');
  AssertPathAndExistingAncestorsNotReparse(NormalizedStateDir,
    'relocated Facial managed-state');
  { Repeat both lexical-chain checks immediately before the destructive call. }
  AssertPathAndExistingAncestorsNotReparse(WorkspaceRoot,
    'relocated Facial workspace-root');
  AssertPathAndExistingAncestorsNotReparse(NormalizedStateDir,
    'relocated Facial managed-state');
  if not DelTree(NormalizedStateDir, True, True, True) then
    RaiseException('Could not remove the cached relocated .facial state: ' + NormalizedStateDir);
  if AnyPathExists(NormalizedStateDir) then
    RaiseException('Relocated .facial cleanup postcondition failed: ' + NormalizedStateDir);
end;

{ Delete only Facial-owned state inside a relocated workspace. The workspace root
  can be a user's raw-media folder and is never itself a deletion target. }
procedure MaybeDeleteRelocatedWorkspaceState();
var
  StateDir: String;
begin
  if ConfirmCachedRelocatedWorkspaceStateDeletion(StateDir) then
    DeleteRelocatedWorkspaceStateChecked(StateDir);
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

procedure RunPriorUninstallerSafely(Uninstaller: String; CurrentUserOrigin: Boolean);
var
  DataDirHeld: Boolean;
  DeleteRelocatedState: Boolean;
  ExitCode: Integer;
  HoldPath: String;
  RelocatedStateDir: String;
  OperationError: String;
  RestoreError: String;
begin
  { Cache the relocated path while the predecessor settings still exist. The
    predecessor runs silently only after the operator has made this exact choice. }
  DeleteRelocatedState := ConfirmCachedRelocatedWorkspaceStateDeletion(RelocatedStateDir);
  HoldPath := DefaultDataDirHoldPath();
  DataDirHeld := False;
  OperationError := '';
  RestoreError := '';
  try
    try
      HoldDefaultDataDir(DataDirHeld, HoldPath);
      if not FileExists(Uninstaller) then
        RaiseException('The registered Facial predecessor uninstaller is missing: ' + Uninstaller);
      ExitCode := -1;
      if CurrentUserOrigin then
      begin
        if not ExecAsOriginalUser(Uninstaller, PriorUninstallArguments, '', SW_HIDE,
          ewWaitUntilTerminated, ExitCode) then
          RaiseException('Could not start the current-user Facial predecessor uninstaller: ' +
            Uninstaller);
      end
      else
      begin
        if not Exec(Uninstaller, PriorUninstallArguments, '', SW_HIDE,
          ewWaitUntilTerminated, ExitCode) then
          RaiseException('Could not start the machine Facial predecessor uninstaller: ' +
            Uninstaller);
      end;
      if ExitCode <> 0 then
        RaiseException('The registered Facial predecessor uninstaller returned exit code ' +
          IntToStr(ExitCode) + '; required exit code is 0.');
      { The uninstaller may leave its self-cleanup clone running briefly after it
        exits. Prove the exact registration and known Facial shortcuts disappear. }
      if not WaitForPriorUninstallCleanup() then
        RaiseException('The predecessor uninstaller returned 0, but its exact registry ' +
          'registration or Facial shortcuts remained after the bounded cleanup wait.');
    except
      OperationError := GetExceptionMessage();
    end;
  finally
    if not RestoreDefaultDataDir(DataDirHeld, HoldPath, RestoreError) then
    begin
      { RestoreError always names HoldPath so the entire preserved data directory
        can be found even when both the predecessor operation and restoration fail. }
    end;
  end;
  if RestoreError <> '' then
  begin
    if OperationError <> '' then
      OperationError := OperationError + #13#10;
    RaiseException(OperationError + RestoreError);
  end;
  if OperationError <> '' then
    RaiseException(OperationError);
  AssertDefaultDataDirPostconditions(DataDirHeld, HoldPath);
  { Only after successful predecessor removal and whole-DataDir restoration may
    the exact current app-owned targets be removed. }
  if DeleteRelocatedState then
    DeleteRelocatedWorkspaceStateChecked(RelocatedStateDir);
  DeleteDefaultAppOwnedDataChecked();
end;

{ Uninstall mode: safely hand off to the registered predecessor, then stop setup
  (a non-empty result intentionally aborts this setup after successful removal). }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  currentUserOrigin: Boolean;
  priorFound: Boolean;
  unins: String;
begin
  Result := '';
  if SelectedMode() = MODE_UNINST then
  begin
    priorFound := ResolvePriorUninstaller(unins, currentUserOrigin);
    if priorFound then
      RunPriorUninstallerSafely(unins, currentUserOrigin)
    else
    begin
      { No registered uninstaller; remove what we can directly. }
      MaybeDeleteRelocatedWorkspaceState();
      DeleteDefaultAppOwnedDataChecked();
      if DirExists(ExpandConstant('{autopf}\Facial')) and
         (not DelTree(ExpandConstant('{autopf}\Facial'), True, True, True)) then
        RaiseException('Could not remove the unregistered Facial program tree: ' +
          ExpandConstant('{autopf}\Facial'));
    end;
    Result := 'Facial has been uninstalled. Setup will now close.';
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  mode: Integer;
  programTree: String;
begin
  if CurStep <> ssInstall then exit;
  mode := SelectedMode();
  { Soft + Full start from a clean program tree (drop orphaned asset files). }
  programTree := ExpandConstant('{app}\product');
  if ModeCleansProgramTree(mode) and DirExists(programTree) then
  begin
    if not DelTree(programTree, True, True, True) then
      RaiseException('Could not clean the existing Facial program tree: ' + programTree);
    if DirExists(programTree) then
      RaiseException('Facial program-tree cleanup did not remove the exact target: ' + programTree);
  end;
  { Full also deletes user data, with the relocated-workspace prompt first. }
  if ModeDeletesUserData(mode) then
  begin
    if ModeDeletesRelocatedState(mode) then
      MaybeDeleteRelocatedWorkspaceState();
    DeleteDefaultAppOwnedDataChecked();
  end;
end;

{ Add/Remove Programs uninstall: offer to delete settings + projects (spec: uninstall is
  the most destructive mode). Relocated workspaces are a separate, explicit per-item prompt. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep <> usUninstall then exit;
  if not AnyPathExists(DataDir()) then exit;
  if not DirExists(DataDir()) then
    RaiseException('Refusing uninstall data cleanup because the Facial data root is not a directory: ' +
      DataDir());
  if UninstallSilent() or
     (MsgBox('Also delete Facial settings and projects?' + #13#10 + DataDir(),
             mbConfirmation, MB_YESNO) = IDYES) then
  begin
    if not UninstallSilent() then
      MaybeDeleteRelocatedWorkspaceState();
    DeleteDefaultAppOwnedDataChecked();
  end;
end;

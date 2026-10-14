!macro NSIS_HOOK_PREINSTALL
  ; A running service holds urma-node.exe open; stop it before files are replaced.
  nsExec::ExecToLog 'net stop urma-node'
  Pop $0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\urma-node.exe" service install'
  Pop $0
  ${If} $0 <> 0
    MessageBox MB_ICONSTOP "URMA Node service install failed ($0)." /SD IDOK
    SetErrorLevel 3
    Abort
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    nsExec::ExecToLog '"$INSTDIR\urma-node.exe" service uninstall'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; The service only runs from Program Files; refuse any other directory up front.
  ${If} $INSTDIR != "$PROGRAMFILES64\${PRODUCTNAME}"
    MessageBox MB_ICONSTOP "URMA Node installs only to $PROGRAMFILES64\${PRODUCTNAME}: its system service refuses any other location." /SD IDOK
    SetErrorLevel 3
    Abort
  ${EndIf}
  ; A running service holds urma-node.exe open; stop it and wait before files are replaced.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop urma-node'
  Pop $0
  StrCpy $1 0
  ${Do}
    nsExec::ExecToStack '"$SYSDIR\sc.exe" query urma-node'
    Pop $0
    Pop $2
    ${If} $0 <> 0
      ${Break}
    ${EndIf}
    ${StrLoc} $3 $2 "STOPPED" ">"
    ${If} $3 != ""
      ${Break}
    ${EndIf}
    IntOp $1 $1 + 1
    ${If} $1 >= 60
      MessageBox MB_ICONSTOP "The urma-node service did not stop." /SD IDOK
      SetErrorLevel 3
      Abort
    ${EndIf}
    Sleep 500
  ${Loop}
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
    ${If} $0 <> 0
      MessageBox MB_ICONSTOP "URMA Node service uninstall failed ($0); nothing was removed." /SD IDOK
      SetErrorLevel 3
      Abort
    ${EndIf}
  ${EndIf}
!macroend

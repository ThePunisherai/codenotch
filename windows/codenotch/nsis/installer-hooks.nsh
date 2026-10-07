; Installed copies can safely apply signed NSIS updates. Portable archives omit this marker.
!macro NSIS_HOOK_POSTINSTALL
  FileOpen $0 "$INSTDIR\.codenotch-installed" w
  FileWrite $0 "Codenotch NSIS$\r$\n"
  FileClose $0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  Delete "$INSTDIR\.codenotch-installed"
!macroend

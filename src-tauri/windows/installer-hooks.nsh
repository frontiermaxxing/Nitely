; Hooks for the release installer (src-tauri/tauri.release.json). The installer ships the nitely
; CLI beside the app; agents' hooks and MCP servers run `nitely`, so after installing, the CLI puts
; itself where they find it: `nitely install` picks the nitely already on PATH, or a folder on PATH
; (crates/nitely-core/src/install.rs). It replaces a running copy safely, so an update while agents
; work does not break them.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\nitely.exe" install --from "$INSTDIR\nitely.exe"'
!macroend

; The copy install puts in the user's WindowsApps folder (a machine without Rust) goes with the app.
; A copy in Cargo's folder belongs to a Rust setup and stays.
!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$LOCALAPPDATA\Microsoft\WindowsApps\nitely.exe"
!macroend

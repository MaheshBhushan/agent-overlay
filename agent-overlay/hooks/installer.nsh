; NSIS installer hooks for agent-overlay (Windows).
;
; After the files are in place, ask the freshly installed binary to write the
; status hooks into whichever agent CLIs this user has (Claude Code, codex,
; opencode, pi). Without this, a Windows user gets scraping-only status and no
; "needs approval" detection at all — there are no tmux panes to scrape.
;
; This only works because the bundle installs per-user ("installMode":
; "currentUser" in tauri.conf.json). A perMachine install runs elevated, so
; $PROFILE would be the administrator's and the hooks would land in a home
; directory the agent CLIs never read.
;
; Failure is non-fatal by design: the app's first run installs the hooks too,
; so a blocked or slow install step costs nothing.

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Installing agent status hooks (Claude Code, codex, opencode, pi)..."
  nsExec::ExecToLog '"$INSTDIR\agent-overlay.exe" --install-hooks'
  Pop $0
!macroend

; Uninstalling leaves the hooks in place on purpose. They are inert without the
; overlay running — each one is a 2-second-timeout connection to a port nothing
; is listening on — and silently editing the user's agent CLI configs during an
; uninstall is worse than leaving four dead entries behind.
!macro NSIS_HOOK_POSTUNINSTALL
!macroend

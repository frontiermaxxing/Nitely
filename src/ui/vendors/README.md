# Agent marks

`AgentMark` (src/ui/AgentMark.tsx) draws each agent as a rounded square in its colour. Nitely
ships no vendor marks: the products' owners require permission to show them (checked 2026-10-16:
Anthropic's trademark guidelines need its approval in advance; OpenAI publishes no Codex mark and
asks apps to keep its logos out; Cursor's brand page states no terms). Instead, as Andrew decided
on NIT-119, each agent is drawn with the icon of the copy installed on the person's own machine,
which is what their own taskbar already shows.

`src-tauri/src/agent_icons.rs` asks Windows for that icon (the same picture Explorer and the Start
menu show), in this order, and keeps the first it finds as a 64px PNG in the app data dir
(`agent-icons/<agent>.png`, read again after a week):

| Agent  | Where it looks |
| ------ | -------------- |
| Claude | the Claude desktop app (`Claude_pzs8sxrjxfjjc!Claude`), then `claude.exe` on PATH |
| Codex  | OpenAI's desktop app (`OpenAI.Codex_2p2nqsd0c76g0!App`), then `codex.exe` on PATH |
| Cursor | `Cursor.exe` beside the `cursor` script on PATH, then the usual install folders |

A program with no icon of its own is skipped rather than shown with Windows' generic one. The icon
sits on the agent's colour, clipped to the square, so a white mark drawn for a dark tile still
shows. Where nothing is installed (and in the browser preview, and on the Mac for now) the agent
keeps its two letters from `VENDORS` in src/ui/agents.ts: **Cl**, **Cx**, **Cu**.

To see what a machine yields: `cargo test -p nitely-app agent_icons -- --ignored --nocapture`
prints the sources that gave an icon and writes them to the temp folder.

A new agent is an entry in `VENDORS` plus, for its icon, its sources in `sources()` in
agent_icons.rs.

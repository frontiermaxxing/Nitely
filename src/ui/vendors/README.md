# Agent marks

`AgentMark` (src/ui/AgentMark.tsx) draws each agent as a rounded square in its colour with a mark
from `VENDORS` in src/ui/agents.ts. NIT-83 asked for each product's own icon. This file records,
per product, what its owner allows and why Nitely does not use the official mark yet. Checked on
2026-10-16 from each owner's own pages; nothing was downloaded from icon packs or third parties.

| Agent | Official mark | What the owner allows | Nitely shows |
| ----- | ------------- | --------------------- | ------------ |
| Claude Code (Anthropic) | The Claude spark. No public brand page; logos are in a press-kit zip linked from anthropic.com/news. | anthropic.com/legal/trademark-guidelines: the marks may be used only as Anthropic specifically allows, in materials it approves in advance; no change of colour, proportion or otherwise. Approval goes through marketing@anthropic.com. | **Cl** in Claude's colour |
| Codex (OpenAI) | None: openai.com/brand publishes the OpenAI wordmark and Blossom, nothing for Codex, and nothing says the Blossom may stand for Codex. | openai.com/brand: use the logo exactly as provided and only with permission; its rules for developers say an app's own UI should stay free of OpenAI logos. Requests go to partnercomms@openai.com. | **Cx** in Codex's colour |
| Cursor (Anysphere) | The cube, in the kit at cursor.com/brand (2D marks are single-colour, light and dark). | The page states no usage terms beyond spelling the name "Cursor". The cube alone is only inside the kit's zip, unread here, and the preview files carry a background, so using it means cutting it out of their artwork. | **Cu** in Cursor's colour |

All three products start with C, so the old single letters (C, X, U) told Codex and Cursor apart
only to someone who had read docs/visualization.md. Two letters say which one at a glance; the
square's colour and the hover title ("Codex · working") say the rest.

To switch an agent to its official mark once its owner allows it: add the SVG here with its source
URL, the date and the permission, set that agent's `mark` in `VENDORS` to `{ path: "..." }` (a
single path on the 16 by 16 grid, in the owner's monochrome variant, drawn in `--on-vendor`), and
check it at 12 to 22px in light and dark (#kit in the browser preview). Keep the clear space the
owner asks for; if the live dot would cover the mark, move the dot beside it.

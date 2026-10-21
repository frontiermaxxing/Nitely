import type { Ask, TimelineEntry } from "./types";

/**
 * The ask a timeline entry is about (NIT-546): an `ask` entry is the ask asked then with that
 * question, an `answer` entry the one answered then. Null for any other entry, or one no ask
 * matches (an older timeline).
 */
export function askOfEntry(entry: Pick<TimelineEntry, "kind" | "ts" | "summary">, asks: Ask[] | undefined): number | null {
  if (!asks?.length) return null;
  const found =
    entry.kind === "ask"
      ? asks.find((a) => a.asked_at === entry.ts && a.question.trim() === (entry.summary ?? "").trim())
      : entry.kind === "answer"
        ? asks.find((a) => a.answered_at === entry.ts)
        : undefined;
  return found ? found.id : null;
}

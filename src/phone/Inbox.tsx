import { useCallback, useEffect, useRef, useState } from "react";
import { signedGet, signedPost, unreachable } from "./api";
import { keyProject, phoneProjects, shownIndex, type Shown } from "./select";
import { plain } from "./plain";
import { Rich } from "./Rich";
import { acceptMessage, answerMessage, approvalMessage, sendBackMessage, sign, type Device } from "./keys";
import { StationTrack } from "../ui/StationTrack";
import { SeverityMark } from "../ui/SeverityMark";
import { VENDOR_LABEL, vendorOf } from "../ui/agents";
import { describeTrack, type Track } from "../ui/lifecycle";
import "../ui/ui.css";

interface PhoneIssue {
  key: string;
  title: string;
  kind: string;
  priority: number;
  state: string;
  track: Track;
}

/** A decision the phone can make (crates/nitely-core/src/phone.rs `decisions`). */
export interface PhoneDecision {
  kind: "ask" | "approve";
  id: string;
  since: number;
  issue: PhoneIssue | null;
  ask?: { id: number; question: string; options: string[]; asked_by: string; finding?: { priority: number; text: string } | null };
  patch_id?: string;
  reasons?: string[];
  review?: { reviewer: string; verdict: string; note: string; findings: { priority: number; text: string; carried?: boolean }[] };
}

interface Decisions {
  decisions: PhoneDecision[];
  at_pc: number;
}

const nowSecs = () => Math.floor(Date.now() / 1000);
const who = (actorOrVendor: string) => {
  const v = vendorOf(actorOrVendor);
  return v === "other" ? "An agent" : VENDOR_LABEL[v];
};

/** The decisions waiting on the person, one per screen, with their actions at the bottom. */
export function Inbox(props: { device: Device; onGone: (why: string) => void }) {
  const { device } = props;
  const [data, setData] = useState<Decisions | null>(null);
  // The decision on screen, by id: a refresh that adds or removes others never swaps it out (and
  // with it a half-written answer). Its position is kept for when it goes.
  const [shown, setShown] = useState<Shown>({ id: null, index: 0 });
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await signedGet<Decisions>(device, "/phone/api/decisions");
      if (r.status === 401 && /not paired/.test(r.body.error ?? "")) return props.onGone("This phone was removed in the Nitely app.");
      if (r.status !== 200) return setError(r.body.error ?? `The PC answered ${r.status}.`);
      setError(null);
      setData(r.body);
    } catch (e) {
      setError(unreachable(e));
    }
  }, [device]);

  // On open, when the phone comes back to the page, and every 20 seconds while it is shown.
  useEffect(() => {
    load();
    const t = setInterval(() => document.visibilityState === "visible" && load(), 20000);
    const back = () => document.visibilityState === "visible" && load();
    document.addEventListener("visibilitychange", back);
    return () => {
      clearInterval(t);
      document.removeEventListener("visibilitychange", back);
    };
  }, [load]);

  // The project filter (NIT-236): kept on the phone, and back to all once its project has nothing left.
  const [wanted, setWanted] = useState<string | null>(storedProject);
  const all = data?.decisions ?? [];
  const { counts, project } = phoneProjects(all.map((d) => d.issue?.key), wanted);
  const pickProject = (p: string | null) => {
    setWanted(p);
    storeProject(p);
    setShown({ id: null, index: 0 });
  };
  const list = project ? all.filter((d) => keyProject(d.issue?.key) === project) : all;
  const index = shownIndex(list.map((d) => d.id), shown);
  const current = list[index];
  // Whatever is on screen is pinned by id, the first one included, and its position follows it on
  // every refresh: when it goes, the one after it takes its place.
  useEffect(() => {
    if (current && (current.id !== shown.id || index !== shown.index)) setShown({ id: current.id, index });
  }, [current, shown.id, shown.index, index]);
  const move = (by: number) => {
    const next = Math.max(0, Math.min(list.length - 1, index + by));
    setShown({ id: list[next]?.id ?? null, index: next });
  };

  // A clearly sideways swipe moves between decisions; a scroll, or a gesture in a text field, does not.
  const touch = useRef<{ x: number; y: number } | null>(null);
  const swipe = {
    onTouchStart: (e: React.TouchEvent) => {
      const inField = (e.target as HTMLElement).closest("textarea, input");
      touch.current = inField ? null : { x: e.touches[0].clientX, y: e.touches[0].clientY };
    },
    onTouchEnd: (e: React.TouchEvent) => {
      if (!touch.current) return;
      const dx = e.changedTouches[0].clientX - touch.current.x;
      const dy = e.changedTouches[0].clientY - touch.current.y;
      touch.current = null;
      if (Math.abs(dx) > 60 && Math.abs(dx) > 2 * Math.abs(dy)) move(dx < 0 ? 1 : -1);
    },
  };

  // The decided one leaves the list at the next load and the one after it moves into its place.
  // The selection is not touched here: by the time a slow reply arrives the person may be on
  // another decision, writing in it.
  const decided = async (what: string) => {
    setDone(what);
    await load();
  };

  if (!data && !error) return <section className="ph-body ph-quiet-page">Loading…</section>;
  return (
    <section className="ph-body ph-inbox" {...swipe}>
      {error && <p className="ph-error">{error}</p>}
      {done && (
        <p className="ph-done" role="status">
          {done}
        </p>
      )}
      {data && list.length === 0 && (
        <div className="ph-empty">
          <h1>Nothing to decide here</h1>
          <p>{data.at_pc > 0 ? `${data.at_pc} more ${data.at_pc === 1 ? "thing waits" : "things wait"} for you at the PC.` : "No question or approval waits for you."}</p>
          <div className="ph-actions">
            <button className="ph-btn" onClick={load}>
              Check again
            </button>
          </div>
        </div>
      )}
      {counts.length > 1 && (
        <select className="ph-project" aria-label="Project" value={project ?? ""} onChange={(e) => pickProject(e.target.value || null)}>
          <option value="">Every project ({all.length})</option>
          {counts.map((c) => (
            <option key={c.project} value={c.project}>
              {c.project} ({c.count})
            </option>
          ))}
        </select>
      )}
      {current && (
        <>
          <div className="ph-nav">
            <button className="ph-arrow" aria-label="Previous" disabled={index === 0} onClick={() => move(-1)}>
              ‹
            </button>
            <span className="ph-count">
              {index + 1} of {list.length}
              {data && data.at_pc > 0 && <span className="ph-quiet"> · {data.at_pc} at the PC</span>}
            </span>
            <button className="ph-arrow" aria-label="Next" disabled={index === list.length - 1} onClick={() => move(1)}>
              ›
            </button>
          </div>
          <Decision key={current.id} decision={current} device={device} onDecided={decided} onError={setError} />
        </>
      )}
    </section>
  );
}

/** Where the phone keeps the project its Inbox is filtered to. Storage can be missing or refuse. */
const PROJECT_KEY = "nitely.phone.project";
function storedProject(): string | null {
  try {
    return localStorage.getItem(PROJECT_KEY) || null;
  } catch {
    return null;
  }
}
function storeProject(project: string | null) {
  try {
    if (project) localStorage.setItem(PROJECT_KEY, project);
    else localStorage.removeItem(PROJECT_KEY);
  } catch {
    // Not kept: the filter holds until the page closes.
  }
}

function IssueHead(props: { issue: PhoneIssue }) {
  return (
    <div className="ph-issue">
      <div className="ph-issue-key">{props.issue.key}</div>
      <h1 className="ph-issue-title">{props.issue.title}</h1>
      {/* The compact track and its words: the full one's labels do not fit a phone's width. */}
      <div className="ph-track">
        <StationTrack track={props.issue.track} size="mini" />
        <span>{describeTrack(props.issue.track)}</span>
      </div>
    </div>
  );
}

function Decision(props: { decision: PhoneDecision; device: Device; onDecided: (what: string) => void; onError: (e: string | null) => void }) {
  const { decision: d, device } = props;
  const [busy, setBusy] = useState(false);
  const [writing, setWriting] = useState<"answer" | "sendback" | "accept" | "decline" | null>(null);
  const [text, setText] = useState("");

  const send = async (path: string, body: Record<string, unknown>, what: string) => {
    setBusy(true);
    props.onError(null);
    try {
      const r = await signedPost<{ ok: boolean }>(device, path, body);
      if (r.status === 200) props.onDecided(what);
      else props.onError(r.body.error ?? `The PC answered ${r.status}.`);
    } catch (e) {
      props.onError(unreachable(e));
    }
    setBusy(false);
  };

  const answer = (value: string) => {
    const time = nowSecs();
    const ask = d.ask!;
    send("/phone/api/answer", { ask: ask.id, answer: value, time, signature: sign(device.secret, answerMessage(ask.id, value, time)) }, `Answered: ${value}`);
  };
  const approve = () => {
    const key = d.issue!.key;
    const time = nowSecs();
    send("/phone/api/approve", { key, patch: d.patch_id, time, signature: sign(device.secret, approvalMessage(key, d.patch_id!, time)) }, `Approved ${key}.`);
  };
  // A review finding the agent asks the person to accept as it is (NIT-191); the reason goes on
  // one line, as the PC keeps it, so what the phone signs is what is checked later.
  const finding = d.kind === "ask" ? (d.ask!.finding ?? null) : null;
  const oneLine = (s: string) => s.trim().split(/\s+/).join(" ");
  const accept = () => {
    const key = d.issue!.key;
    const time = nowSecs();
    const reason = oneLine(text);
    send("/phone/api/accept", { ask: d.ask!.id, patch: d.patch_id, reason, time, signature: sign(device.secret, acceptMessage(key, d.patch_id!, finding!.text, reason, time)) }, `Accepted for ${key}.`);
  };
  const decline = () => {
    const time = nowSecs();
    const why = oneLine(text);
    send("/phone/api/decline", { ask: d.ask!.id, why, time, signature: sign(device.secret, answerMessage(d.ask!.id, why, time)) }, `Sent back: it stays a defect to fix.`);
  };
  const sendBack = () => {
    const key = d.issue!.key;
    const time = nowSecs();
    const reason = text.trim();
    send("/phone/api/sendback", { key, reason, time, signature: sign(device.secret, sendBackMessage(key, reason, time)) }, `Sent ${key} back to the agents.`);
  };

  return (
    <article className="ph-decision">
      <div className={`ph-kind ph-kind-${d.kind}`}>{finding ? "Accept this review finding?" : d.kind === "ask" ? `${who(d.ask!.asked_by)} asks` : "Approve this change?"}</div>
      {d.issue && <IssueHead issue={d.issue} />}

      {finding && (
        <div className="ph-why">
          <ul className="ph-findings">
            <li>
              <SeverityMark priority={finding.priority} /> <Rich text={finding.text} />
            </li>
          </ul>
          <h2>Why {who(d.ask!.asked_by)} asks you to accept it as it is</h2>
        </div>
      )}
      {d.kind === "ask" && (
        <p className="ph-question">
          <Rich text={d.ask!.question} />
        </p>
      )}
      {finding && <p className="ph-quiet">Accepting signs your decision: it no longer blocks the land, and the next review is told. Sent back, it stays a defect to fix.</p>}
      {d.kind === "approve" && (
        <div className="ph-why">
          {(d.reasons ?? []).length > 0 && (
            <>
              <h2>Why it waits for you</h2>
              <ul>
                {d.reasons!.map((r) => (
                  <li key={r}>
                    <Rich text={r} />
                  </li>
                ))}
              </ul>
            </>
          )}
          {d.review && (
            <>
              <h2>
                {who(d.review.reviewer)}'s review {d.review.verdict === "pass" ? "passed" : "asked for changes"}
                {d.review.note && <span className="ph-quiet"> ({d.review.note})</span>}
              </h2>
              {d.review.findings.length === 0 ? (
                <p className="ph-quiet">No findings.</p>
              ) : (
                <ul className="ph-findings">
                  {d.review.findings.map((f) => (
                    <li key={f.text}>
                      <SeverityMark priority={f.priority} /> <Rich text={f.text} />
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
        </div>
      )}

      <div className="ph-actions">
        {writing ? (
          <>
            <label className="ph-field">
              <span>{writing === "answer" ? "Your answer" : writing === "accept" ? "Why you accept it, in one line" : writing === "decline" ? "Why not, in one line" : "What should change"}</span>
              <textarea rows={writing === "accept" || writing === "decline" ? 2 : 4} value={text} autoFocus onChange={(e) => setText(e.target.value)} />
            </label>
            <button
              className="ph-btn ph-primary"
              disabled={busy || !text.trim()}
              onClick={() => (writing === "answer" ? answer(text.trim()) : writing === "accept" ? accept() : writing === "decline" ? decline() : sendBack())}
            >
              {writing === "answer" ? "Send answer" : writing === "accept" ? "Accept" : "Send back"}
            </button>
            <button className="ph-btn" disabled={busy} onClick={() => (setWriting(null), setText(""))}>
              Cancel
            </button>
          </>
        ) : finding ? (
          <>
            <button className="ph-btn ph-primary" disabled={busy || !d.patch_id} onClick={() => setWriting("accept")}>
              Accept
            </button>
            <button className="ph-btn" disabled={busy} onClick={() => setWriting("decline")}>
              Send back
            </button>
          </>
        ) : d.kind === "ask" ? (
          <>
            {d.ask!.options.map((o) => (
              <button key={o} className="ph-btn ph-option" disabled={busy} onClick={() => answer(o)}>
                {plain(o)}
              </button>
            ))}
            <button className="ph-btn" disabled={busy} onClick={() => setWriting("answer")}>
              {d.ask!.options.length ? "Another answer" : "Answer"}
            </button>
          </>
        ) : (
          <>
            <button className="ph-btn ph-primary" disabled={busy} onClick={approve}>
              Approve
            </button>
            <button className="ph-btn" disabled={busy} onClick={() => setWriting("sendback")}>
              Send back
            </button>
          </>
        )}
      </div>
    </article>
  );
}

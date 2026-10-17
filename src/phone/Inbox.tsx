import { useCallback, useEffect, useRef, useState } from "react";
import { signedGet, signedPost } from "./api";
import { answerMessage, approvalMessage, sendBackMessage, sign, type Device } from "./keys";
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
  ask?: { id: number; question: string; options: string[]; asked_by: string };
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
  const [shown, setShown] = useState<{ id: string | null; index: number }>({ id: null, index: 0 });
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
      setError(`Could not reach the PC: ${String(e)}`);
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

  const list = data?.decisions ?? [];
  const byId = list.findIndex((d) => d.id === shown.id);
  const index = byId >= 0 ? byId : Math.min(shown.index, Math.max(0, list.length - 1));
  const current = list[index];
  // Whatever is on screen is pinned by id from then on, the first one included.
  useEffect(() => {
    if (current && current.id !== shown.id) setShown({ id: current.id, index });
  }, [current, shown.id, index]);
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

  // The decided one leaves the list; the next one moves into its place.
  const decided = async (what: string) => {
    setDone(what);
    setShown({ id: null, index });
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
  const [writing, setWriting] = useState<"answer" | "sendback" | null>(null);
  const [text, setText] = useState("");

  const send = async (path: string, body: Record<string, unknown>, what: string) => {
    setBusy(true);
    props.onError(null);
    try {
      const r = await signedPost<{ ok: boolean }>(device, path, body);
      if (r.status === 200) props.onDecided(what);
      else props.onError(r.body.error ?? `The PC answered ${r.status}.`);
    } catch (e) {
      props.onError(`Could not reach the PC: ${String(e)}`);
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
  const sendBack = () => {
    const key = d.issue!.key;
    const time = nowSecs();
    const reason = text.trim();
    send("/phone/api/sendback", { key, reason, time, signature: sign(device.secret, sendBackMessage(key, reason, time)) }, `Sent ${key} back to the agents.`);
  };

  return (
    <article className="ph-decision">
      <div className={`ph-kind ph-kind-${d.kind}`}>{d.kind === "ask" ? `${who(d.ask!.asked_by)} asks` : "Approve this change?"}</div>
      {d.issue && <IssueHead issue={d.issue} />}

      {d.kind === "ask" && <p className="ph-question">{d.ask!.question}</p>}
      {d.kind === "approve" && (
        <div className="ph-why">
          {(d.reasons ?? []).length > 0 && (
            <>
              <h2>Why it waits for you</h2>
              <ul>
                {d.reasons!.map((r) => (
                  <li key={r}>{r}</li>
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
                      <SeverityMark priority={f.priority} /> {f.text}
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
              <span>{writing === "answer" ? "Your answer" : "What should change"}</span>
              <textarea rows={4} value={text} autoFocus onChange={(e) => setText(e.target.value)} />
            </label>
            <button className="ph-btn ph-primary" disabled={busy || !text.trim()} onClick={() => (writing === "answer" ? answer(text.trim()) : sendBack())}>
              {writing === "answer" ? "Send answer" : "Send back"}
            </button>
            <button className="ph-btn" disabled={busy} onClick={() => (setWriting(null), setText(""))}>
              Cancel
            </button>
          </>
        ) : d.kind === "ask" ? (
          <>
            {d.ask!.options.map((o) => (
              <button key={o} className="ph-btn ph-option" disabled={busy} onClick={() => answer(o)}>
                {o}
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

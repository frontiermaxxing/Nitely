// The Visual kit's row for Switch, Checkbox, Segmented control and Radio cards, in every state.

import { useState } from "react";
import { Checkbox, RadioCards, Segmented, Switch } from "../ui";

type Own = "enough" | "waits";
type LandBy = "agent" | "nitely" | "person";

export function KitToggles() {
  const [on, setOn] = useState(true);
  const [off, setOff] = useState(false);
  const [boxOn, setBoxOn] = useState(true);
  const [boxOff, setBoxOff] = useState(false);
  const [own, setOwn] = useState<Own | null>("enough");
  const [landBy, setLandBy] = useState<LandBy | null>("agent");
  return (
    <section>
      <h2>Switch, Checkbox, Segmented control, Radio cards</h2>
      <p className="muted small">A switch applies on its own; a checkbox only inside a form saved together. Two or three short options are a segmented control; options that each need a sentence are radio cards. The option that equals the default carries the default mark.</p>
      <div className="kit-row" style={{ gap: 24 }}>
        <Switch checked={on} onChange={setOn} label={on ? "On" : "Off"} />
        <Switch checked={off} onChange={setOff} label={off ? "On" : "Off"} />
        <Switch checked={false} onChange={() => {}} label="Disabled" disabled />
        <Switch checked onChange={() => {}} label="Disabled, on" disabled />
      </div>
      <div className="kit-row" style={{ gap: 24 }}>
        <Checkbox checked={boxOn} onChange={setBoxOn} label={boxOn ? "On" : "Off"} />
        <Checkbox checked={boxOff} onChange={setBoxOff} label={boxOff ? "On" : "Off"} />
        <Checkbox checked={false} onChange={() => {}} label="Disabled" disabled />
        <Checkbox checked onChange={() => {}} label="Disabled, on" disabled />
      </div>
      <div className="kit-row">
        <Segmented<Own>
          aria-label="Own-vendor review"
          value={own}
          onChange={setOwn}
          options={[
            { value: "enough", label: "Is enough", isDefault: true },
            { value: "waits", label: "Waits for me" },
          ]}
        />
      </div>
      <RadioCards<LandBy>
        aria-label="Who lands a change"
        value={landBy}
        onChange={setLandBy}
        options={[
          { value: "agent", label: "The agent", why: "The agent that made the change lands it once the review passes.", isDefault: true },
          { value: "nitely", label: "Nitely", why: "Nitely lands passed changes in a queue, several on one gate." },
          { value: "person", label: "You", why: "Every change waits for you to land it.", disabled: true },
        ]}
      />
    </section>
  );
}

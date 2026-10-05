// Phone sync: hands the agent pills to Rust (phone.rs) whenever they change.
// Rust strips them down, throttles and publishes; this side only skips repeats.

import { Bridge } from "../core/bridge";
import { State } from "../core/state";

export function registerPhoneSync() {
  let last = "";
  State.subscribe(() => {
    const agents = State.tasks
      .filter((t) => t.id === "integration_claude" || t.id.startsWith("agent_"))
      .map((t) => ({ id: t.id, name: t.name, color: t.color, state: t.state, step: t.steps[t.stepIndex] ?? "" }));
    const key = JSON.stringify(agents);
    if (key === last) return;
    last = key;
    void Bridge.phoneAgents(agents);
  });
}

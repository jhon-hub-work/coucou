// Boo for OpenCode — BOO-MANAGED-PLUGIN
//
// Written by Boo's settings window, and removed from there too (or just delete
// this file). It tells the Boo island what your OpenCode sessions are doing by
// handing small JSON events to the local boo-hook relay. Nothing leaves your PC,
// and nothing here waits for Boo: if Boo is closed the relay exits at once.
//
// Watch only. OpenCode 1.18 does not call a plugin's `permission.ask` hook, so a
// permission prompt is shown in the island as a question and answered in OpenCode.

import { spawn } from "node:child_process";

const EXE = __BOO_HOOK_EXE__;
const AGENT = "opencode";

const TOOLS = {
  bash: "Bash", read: "Read", write: "Write", edit: "Edit", multiedit: "MultiEdit",
  patch: "Edit", apply_patch: "Edit", glob: "Glob", grep: "Grep", list: "LS",
  webfetch: "WebFetch", websearch: "WebSearch", task: "Task", todowrite: "TodoWrite",
};
const toolName = (t) => TOOLS[t] || (t ? t.charAt(0).toUpperCase() + t.slice(1) : "Tool");

// Only the fields the island shows: a whole file in `content` has no business here.
const KEEP = ["command", "path", "pattern", "url", "query", "description"];
function toolInput(args) {
  const out = {};
  if (!args || typeof args !== "object") return out;
  for (const k of KEEP) if (typeof args[k] === "string") out[k] = args[k].slice(0, 300);
  if (typeof args.filePath === "string") out.file_path = args.filePath;
  return out;
}

function fire(event, payload) {
  try {
    const child = spawn(EXE, ["--agent", AGENT, event], {
      stdio: ["pipe", "ignore", "ignore"],
      windowsHide: true,
    });
    child.on("error", () => {}); // Boo not installed or not there: say nothing.
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ hook_event_name: event, ...payload }));
    const timer = setTimeout(() => { try { child.kill(); } catch {} }, 5000);
    child.on("close", () => clearTimeout(timer));
  } catch {
    // Never let a notification problem reach OpenCode.
  }
}

export const BooPlugin = async ({ directory }) => {
  const children = new Set(); // sub-agent sessions, by id
  const base = (sessionID, cwd) => ({ session_id: sessionID || "", cwd: cwd || directory || "" });

  return {
    event: async ({ event }) => {
      const p = event.properties || {};
      switch (event.type) {
        case "session.created": {
          const info = p.info || {};
          if (info.parentID) {
            children.add(info.id);
            fire("SubagentStart", base(info.id, info.directory));
          } else {
            fire("SessionStart", base(info.id, info.directory));
          }
          break;
        }
        case "session.idle":
          if (children.delete(p.sessionID)) fire("SubagentStop", base(p.sessionID));
          else fire("Stop", base(p.sessionID));
          break;
        case "session.error":
          // Stopping a turn yourself is not an error.
          if (p.error && p.error.name === "MessageAbortedError") fire("Stop", base(p.sessionID));
          else fire("StopFailure", { ...base(p.sessionID), message: (p.error && p.error.data && p.error.data.message) || "" });
          break;
        case "session.deleted":
          if (p.info && !children.delete(p.info.id)) fire("SessionEnd", base(p.info.id, p.info.directory));
          break;
        case "permission.asked": // OpenCode 1.18
        case "permission.updated": { // older builds
          const what = p.permission || p.type || "a tool";
          const where = Array.isArray(p.patterns) ? p.patterns.join(", ") : p.pattern || p.title || "";
          fire("Notification", { ...base(p.sessionID), message: `OpenCode needs permission: ${what} ${where}?`.replace(/\s+\?$/, "?") });
          break;
        }
      }
    },
    "chat.message": async (input, output) => {
      const text = (output.parts || []).filter((x) => x.type === "text").map((x) => x.text).join(" ");
      fire("UserPromptSubmit", { ...base(input.sessionID), prompt: text.replace(/^"|"$/g, "").slice(0, 200) });
    },
    "tool.execute.before": async (input, output) => {
      fire("PreToolUse", { ...base(input.sessionID), tool_name: toolName(input.tool), tool_input: toolInput(output.args) });
    },
    "tool.execute.after": async (input) => {
      fire("PostToolUse", { ...base(input.sessionID), tool_name: toolName(input.tool) });
    },
  };
};

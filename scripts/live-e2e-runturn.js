// Run one turn over the Elena WebSocket. Args via process.env:
//   WS_URL    full ws:// or wss:// URL including ?token=...
//   PROMPT    user message text
//   AUTONOMY  cautious | moderate | yolo (default yolo)
//   MODEL     model id override (default llama-3.3-70b-versatile)
//   TIMEOUT   ms (default 120000)
//
// Stdout: one line of JSON {reason, text, text_chars, tool_calls, awaiting, errors}.

const WebSocket = require("ws");

const url = process.env.WS_URL;
const prompt = process.env.PROMPT;
const autonomy = process.env.AUTONOMY || "yolo";
const model = process.env.MODEL || "llama-3.3-70b-versatile";
const timeoutMs = Number(process.env.TIMEOUT || 120000);

if (!url || !prompt) {
  console.error("WS_URL and PROMPT env vars required");
  process.exit(2);
}

const ws = new WebSocket(url);
const out = { reason: null, text: "", tool_calls: [], awaiting: null, errors: [] };
const deadline = setTimeout(() => {
  out.reason = "timeout";
  try { ws.terminate(); } catch (_) {}
  flush();
}, timeoutMs);

ws.on("open", () => {
  const frame = { action: "send_message", text: prompt, autonomy, model };
  ws.send(JSON.stringify(frame));
});

ws.on("message", (m) => {
  let ev;
  try { ev = JSON.parse(m.toString()); } catch (_) { return; }
  switch (ev.event) {
    case "text_delta":         out.text += ev.delta; break;
    case "tool_use_complete":  out.tool_calls.push({ name: ev.name, input: ev.input }); break;
    case "awaiting_approval":  out.awaiting = ev; ws.close(); clearTimeout(deadline); break;
    case "error":              out.errors.push({ kind: ev.kind, message: ev.message }); break;
    case "done":               out.reason = ev.reason; clearTimeout(deadline); ws.close(); break;
  }
});

ws.on("close", () => flush());
ws.on("error", (e) => { out.errors.push({ kind: "ws", message: String(e) }); });

let flushed = false;
function flush() {
  if (flushed) return;
  flushed = true;
  process.stdout.write(JSON.stringify({ ...out, text_chars: out.text.length }));
  process.exit(0);
}

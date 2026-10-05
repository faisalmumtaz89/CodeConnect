// A scripted OpenAI-compatible model for the live tests: streaming chat.completions, scenario chosen by
// `cc:<name>` in the last user message, step k = assistant tool rounds since that message. No credentials and no
// network beyond loopback.
//
// Step fields: text, textBytes (generated reply of N bytes), chunks (split the text into N deltas), delayMs
// (between deltas), tools [{name, args, genContent}] (genContent: a generated `content` argument of N bytes; `{n}`
// in a filePath is replaced by the request number).
//
// Usage: node mock-model.js <port>   (prints "listening <port>" when ready)
import { createServer } from "node:http"

const gen = (/** @type {number} */ nb) => {
  const line = "0123456789 ".repeat(9) + "\n"
  return line.repeat(Math.ceil(nb / line.length)).slice(0, nb)
}

/** @type {Record<string, any[]>} */
export const SCENARIOS = {
  fast: [{ text: "fast reply done" }],
  slow: [{ text: "slow reply " + "word ".repeat(200) + "done", chunks: 200, delayMs: 10 }],
  steps: [
    { text: "Step one.", tools: [{ name: "bash", args: { command: "sleep 2; echo alpha", description: "first" } }] },
    { text: "Step two.", tools: [{ name: "bash", args: { command: "sleep 2; echo beta", description: "second" } }] },
    { text: "All steps finished." },
  ],
  ask: [{ text: "Running a command that needs approval.", tools: [{ name: "bash", args: { command: "echo approved-run", description: "needs approval" } }] }, { text: "ask finished" }],
  many: [
    { text: "Making 150 files.", tools: [{ name: "bash", args: { command: "python3 -c \"[open('m%03d.txt'%i,'w').write(('%099d\\n'%i)*600) for i in range(150)]\"", description: "make 150 files of 60 KB" } }] },
    { text: "many finished" },
  ],
  w64: [{ text: "Writing 60 KB.", tools: [{ name: "write", genContent: 60000, args: { filePath: "w{n}.txt" } }] }, { text: "w64 finished" }],
  flood: [
    { text: "Printing 200 KB.", tools: [{ name: "bash", args: { command: "python3 -c \"import sys\nfor i in range(2000): sys.stdout.write('%099d\\n' % i); sys.stdout.flush()\"", description: "print 200 KB" } }] },
    ...Array.from({ length: 12 }, (_, k) => ({
      text: `Burst ${k}.`,
      tools: Array.from({ length: 4 }, (_, j) => ({ name: "bash", args: { command: `echo burst-${k}-${j}`, description: "burst" } })),
    })),
    { text: "Writing 100 KB.", tools: [{ name: "write", genContent: 100000, args: { filePath: "flood{n}.txt" } }] },
    { textBytes: 100000, chunks: 1000, delayMs: 2 },
  ],
}

/** @param {any} m */
const userText = (m) =>
  typeof m.content === "string"
    ? m.content
    : Array.isArray(m.content)
      ? m.content.map((/** @type {any} */ c) => c.text || "").filter((/** @type {string} */ t) => !t.trimStart().startsWith("<system-reminder>")).join(" ")
      : ""

let n = 0
/** @param {any} body */
function plan(body) {
  const msgs = body.messages || []
  if (!body.tools || body.tools.length === 0) return { text: "Live test session" }
  let lu = -1
  for (let i = msgs.length - 1; i >= 0; i--) if (msgs[i].role === "user") { lu = i; break }
  const m = (lu >= 0 ? userText(msgs[lu]) : "").match(/cc:([a-z0-9_-]+)/i)
  const steps = m ? SCENARIOS[m[1]] : undefined
  if (!steps) return { text: "no scenario" }
  let k = 0
  for (let i = lu + 1; i < msgs.length; i++) if (msgs[i].role === "assistant" && msgs[i].tool_calls) k++
  if (k >= steps.length) return { text: "done " + m?.[1] }
  const st = { ...steps[k] }
  if (st.textBytes) st.text = gen(st.textBytes)
  if (st.tools)
    st.tools = st.tools.map((/** @type {any} */ t) => ({
      name: t.name,
      args: { ...t.args, ...(t.args.filePath ? { filePath: t.args.filePath.replace("{n}", String(n)) } : {}), ...(t.genContent ? { content: gen(t.genContent) } : {}) },
    }))
  return st
}

/** @param {string} s @param {number} k */
const split = (s, k) => {
  if (!s) return []
  const size = Math.ceil(s.length / Math.max(1, k || 1))
  const out = []
  for (let i = 0; i < s.length; i += size) out.push(s.slice(i, i + size))
  return out
}
const sleep = (/** @type {number} */ ms) => new Promise((r) => setTimeout(r, ms))

export function serve(port = 0) {
  const server = createServer((req, res) => {
    let data = ""
    req.on("data", (c) => (data += c))
    req.on("end", async () => {
      /** @type {any} */
      let body = {}
      try {
        body = JSON.parse(data || "{}")
      } catch {}
      if (req.url?.includes("/models")) {
        res.writeHead(200, { "content-type": "application/json" })
        return res.end(JSON.stringify({ data: [{ id: "mock-model" }] }))
      }
      if (!req.url?.includes("chat/completions")) {
        res.writeHead(404)
        return res.end()
      }
      const id = "chatcmpl-" + ++n
      const p = plan(body)
      const base = { id, object: "chat.completion.chunk", created: Math.floor(Date.now() / 1000), model: body.model }
      const usage = { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 }
      if (!body.stream) {
        res.writeHead(200, { "content-type": "application/json" })
        return res.end(JSON.stringify({ id, object: "chat.completion", model: body.model, choices: [{ index: 0, message: { role: "assistant", content: p.text || "" }, finish_reason: "stop" }], usage }))
      }
      res.writeHead(200, { "content-type": "text/event-stream" })
      let closed = false
      res.on("close", () => (closed = !res.writableEnded))
      const sse = (/** @type {any} */ o) => res.write("data: " + JSON.stringify(o) + "\n\n")
      sse({ ...base, choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] })
      for (const t of split(p.text, p.chunks)) {
        if (closed) return
        sse({ ...base, choices: [{ index: 0, delta: { content: t }, finish_reason: null }] })
        if (p.delayMs) await sleep(p.delayMs)
      }
      if (p.tools) {
        p.tools.forEach((/** @type {any} */ t, /** @type {number} */ j) => {
          sse({ ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: j, id: "call_" + n + "_" + j, type: "function", function: { name: t.name, arguments: "" } }] }, finish_reason: null }] })
          sse({ ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: j, function: { arguments: JSON.stringify(t.args) } }] }, finish_reason: null }] })
        })
        sse({ ...base, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }], usage })
      } else sse({ ...base, choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage })
      res.write("data: [DONE]\n\n")
      res.end()
    })
  })
  return new Promise((resolve) => server.listen(port, "127.0.0.1", () => resolve(server)))
}

if (import.meta.main) {
  const server = /** @type {any} */ (await serve(Number(process.argv[2] || 0)))
  console.log("listening " + server.address().port)
}

// The link contract as one file. A fixed script drives the plugin through a fake OpenCode and a fake daemon; every
// line either side wrote is recorded in order as {"dir":"plugin"|"ccd","frame":…}. The run must reproduce
// fixtures/opencode/link-v1-frames.jsonl byte for byte; the daemon's tests decode and map the same file.
// The hello's pid is the only value normalised (to 4242). CC_OPENCODE_CONTRACT_WRITE=1 rewrites the file.
// Rows with a "case" key are hand-written examples of what this run does not reach (a failed list, a second
// activation, split and stubbed history items, …); they are kept on a rewrite and must have the frame shapes the
// run's own frames have.
import { afterEach, expect, test } from "bun:test"
import { readFileSync, writeFileSync } from "node:fs"
import { fileURLToPath } from "node:url"
import { fakeApi } from "./fake-api.js"
import { fakeCcd, fixture, loadPlugin, settled, until, welcome } from "./helpers.js"

const FILE = fixture("link-v1-frames.jsonl")
/** @type {(() => Promise<void> | void)[]} */
const cleanups = []
afterEach(async () => {
  for (const f of cleanups.splice(0).reverse()) await f()
})

const A = "ses_0f00a000000aAAAAAAAAAAAAAA"
const B = "ses_0f00b000000bBBBBBBBBBBBBBB"
const DIR = "/Users/ada/project"
const id = (/** @type {string} */ kind, /** @type {number} */ n) => `${kind}_0f00${String(n).padStart(3, "0")}00000xxxxxxxxxxxxxxx`
const userMsg = (/** @type {string} */ sid, /** @type {number} */ n, /** @type {string} */ text) => ({
  info: { id: id("msg", n), sessionID: sid, role: "user", time: { created: 1700000000000 + n }, agent: "build", model: { providerID: "mock", modelID: "mock-model" } },
  parts: [{ id: id("prt", n), sessionID: sid, messageID: id("msg", n), type: "text", text }],
})
const assistant = (/** @type {string} */ sid, /** @type {number} */ n, /** @type {number} */ parent, /** @type {any[]} */ parts, completed = true) => ({
  info: {
    id: id("msg", n), sessionID: sid, role: "assistant", parentID: id("msg", parent), agent: "build", modelID: "mock-model", providerID: "mock",
    time: completed ? { created: 1700000000000 + n, completed: 1700000000500 + n } : { created: 1700000000000 + n },
    ...(completed ? { finish: "stop" } : {}),
    summary: { diffs: [{ file: "a.txt", patch: "@@ -1 +1 @@" }] },
  },
  parts: parts.map((p, i) => ({ id: id("prt", n * 10 + i), sessionID: sid, messageID: id("msg", n), ...p })),
})

test("the scripted run reproduces link-v1-frames.jsonl", async () => {
  const p = await loadPlugin()
  const f = fakeApi({ directory: DIR })
  const ccd = fakeCcd(p.socket, { onHello: (_h, c) => welcome(c, { [A]: { from: id("msg", 2), inclusive: false } }) })
  cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())

  const bashInput = { command: "ls -la", description: "list" }
  const runningTool = { type: "tool", tool: "bash", callID: "call_1", state: { status: "running", input: bashInput, title: "ls -la", metadata: { output: "" }, time: { start: 1700000000100 } } }
  f.server.sessions = [
    { id: A, directory: DIR, title: "contract", time: { created: 1600000000000, updated: 1700000000000 } },
    { id: B, parentID: A, directory: DIR, title: "child", time: { created: 1700000000004, updated: 1700000000005 } },
  ]
  f.server.messages.set(A, [
    userMsg(A, 1, "first prompt"),
    assistant(A, 2, 1, [{ type: "text", text: "done", time: { start: 1, end: 2 } }]),
    userMsg(A, 3, "list the files"),
    assistant(A, 4, 3, [{ type: "step-start" }, { type: "reasoning", text: "thinking", time: { start: 1, end: 2 } }, runningTool], false),
  ])
  f.server.messages.set(B, [userMsg(B, 5, "child task")])
  f.server.status = { [A]: { type: "busy" }, [B]: { type: "busy" } }
  const perm = { id: "per_0f00a00000001xxxxxxxxxxxxx", sessionID: A, permission: "bash", patterns: ["ls -la"], metadata: { command: "ls -la", description: "list" }, always: ["ls *"], tool: { messageID: id("msg", 4), callID: "call_1" } }
  const question = { id: "que_0f00b00000001xxxxxxxxxxxxx", sessionID: B, questions: [{ question: "Which?", header: "Pick", options: [{ label: "Red", description: "r" }] }], tool: { messageID: id("msg", 5), callID: "call_q" } }
  f.server.permissions = [perm]
  f.server.questions = [question]

  // live events while the first snapshot reads the server: held, then flushed before `settled`
  let once = true
  f.server.before["session.messages"] = () => {
    if (!once) return
    once = false
    f.emit("message.part.updated", { sessionID: A, part: { id: id("prt", 60), sessionID: A, messageID: id("msg", 6), type: "text", text: "partial", time: { start: 1700000000200 } } })
    f.emit("message.part.updated", { sessionID: A, part: { id: id("prt", 60), sessionID: A, messageID: id("msg", 6), type: "text", text: "partial more", time: { start: 1700000000200 } } })
  }

  await p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: "0123456789abcdef0123456789abcdef" }, /** @type {any} */ ({}))
  // seen before the first connect: noted for the verdict, not sent
  f.emit("message.part.updated", { sessionID: A, part: f.server.messages.get(A)?.[3].parts[2] })
  f.emit("permission.asked", perm)
  const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())

  // live: the reply, the tool's terminal, the text's terminal, the turn's end; an oversized frame; an oversized card
  f.emit("permission.replied", { sessionID: A, requestID: perm.id, reply: "once" })
  f.emit("message.part.updated", { sessionID: A, part: { ...f.server.messages.get(A)?.[3].parts[2], state: { ...runningTool.state, status: "completed", output: "a.txt\n", metadata: { output: "a.txt\n", exit: 0 }, time: { start: 1700000000100, end: 1700000000300 } } } })
  f.emit("message.part.updated", { sessionID: A, part: { id: id("prt", 60), sessionID: A, messageID: id("msg", 6), type: "text", text: "partial more, done", time: { start: 1700000000200, end: 1700000000400 } } })
  f.emit("message.updated", { sessionID: A, info: assistant(A, 6, 3, [], true).info })
  f.emit("message.part.updated", { sessionID: B, part: { id: id("prt", 70), sessionID: B, messageID: id("msg", 7), type: "tool", tool: "write", callID: "call_w", state: { status: "completed", input: Object.fromEntries(Array.from({ length: 20 }, (_, i) => ["f" + i, "w".repeat(70000)])), output: "", title: "", metadata: {}, time: { start: 1, end: 2 } } } })
  f.emit("question.asked", { ...question, id: "que_0f00b00000002xxxxxxxxxxxxx", questions: [{ question: "\u0001".repeat(200000), header: "Big", options: [] }] })
  f.navigate({ name: "session", params: { sessionID: A } })
  await until(() => c.frames.some((/** @type {any} */ x) => x.t === "head" && x.route === "session"))
  f.storeSession({ ...f.server.sessions[0] })
  await until(() => c.frames.filter((/** @type {any} */ x) => x.t === "head").length === 3)

  // the root goes idle: a requests-only sync
  f.server.status = {}
  f.server.permissions = []
  f.emit("session.status", { sessionID: A, status: { type: "idle" } })
  f.emit("session.idle", { sessionID: A })
  await until(() => settled(c, 2))

  // the daemon asks for a resync
  c.send({ type: "opencode_resync", acked: { [A]: { from: id("msg", 6), inclusive: false } } })
  await until(() => settled(c, 3))

  const rows = c.wire.map((/** @type {any} */ w) => {
    const frame = JSON.parse(w.line)
    if (frame.type === "opencode_hello") frame.pid = 4242
    return JSON.stringify({ dir: w.dir, frame }) + "\n"
  })
  const text = rows.join("")
  const file = readFileSync(FILE, "utf8").split("\n").filter(Boolean)
  const golden = file.filter((l) => "case" in JSON.parse(l))
  if (process.env.CC_OPENCODE_CONTRACT_WRITE === "1") writeFileSync(fileURLToPath(FILE), text + golden.map((l) => l + "\n").join(""))
  const want = readFileSync(FILE, "utf8").split("\n").filter(Boolean).filter((l) => !("case" in JSON.parse(l)))
  const got = text.split("\n").filter(Boolean)
  expect(got.length).toBe(want.length)
  for (let i = 0; i < want.length; i++) expect(got[i]).toBe(want[i])
  for (const l of got) expect(shapeErrors(JSON.parse(l).frame)).toEqual([])
  expect(golden.length).toBeGreaterThan(0)
  for (const l of golden) {
    const g = JSON.parse(l)
    expect(Object.keys(g)).toEqual(["dir", "case", "frame"])
    expect([g.case, shapeErrors(g.frame)]).toEqual([g.case, []])
  }
})

/** The keys of every frame and page item, in the order the plugin writes them. */
const FRAME = {
  opencode_hello: [["type", "wire", "nonce", "pid", "start", "activation", "directory", "api"]],
  opencode_welcome: [["type", "link", "acked"]],
  opencode_resync: [["type", "acked"]],
  head: [["t", "seq", "route"], ["t", "seq", "route", "session_id", "directory"]],
  ev: [["t", "seq", "type", "properties"]],
  stub: [["t", "seq", "type", "ids", "size", "sha256"], ["t", "seq", "type", "ids", "part_type", "size", "sha256"], ["t", "seq", "type", "ids", "part_type", "status", "size", "sha256"]],
  card_stub: [["t", "seq", "type", "properties", "size", "sha256"]],
  sync_begin: [["t", "sync", "reason", "scope", "as_of", "status", "status_ok"]],
  sync_page: [["t", "sync", "n", "items"]],
  sync_request: [["t", "sync", "kind", "request", "verdict"], ["t", "sync", "kind", "stub", "verdict"]],
  sync_end: [
    ["t", "sync", "done_at", "list_ok", "permissions_ok", "questions_ok", "requests_ok", "lower", "pages", "items", "bytes", "activation"],
    ["t", "sync", "done_at", "permissions_ok", "questions_ok", "requests_ok", "lower", "pages", "items", "bytes", "activation"],
  ],
  settled: [["t", "sync"]],
}
const ITEM = [
  ["session"],
  ["sessionID", "info", "parts"],
  ["sessionID", "info", "parts", "split"],
  ["sessionID", "info", "part", "split"],
  ["stub", "kind", "ids", "size", "sha256"],
  ["stub", "kind", "ids", "part_type", "size", "sha256"],
  ["stub", "kind", "ids", "part_type", "status", "size", "sha256"],
  ["error", "sessionID"],
]
const DEAD = /^(absent|session-missing|not-busy|anchor|asked-before-activation|epoch:(abort|stop|idle|retry|deleted|disposed)(:ancestor)?)$/

/** @param {any} f */
function shapeErrors(f) {
  const errs = []
  const kind = f.t ?? f.type
  const keys = JSON.stringify(Object.keys(f))
  if (!(/** @type {Record<string, string[][]>} */ (FRAME))[kind]?.some((k) => JSON.stringify(k) === keys)) errs.push(`${kind}: keys ${keys}`)
  if (kind === "sync_page")
    for (const it of f.items) if (!ITEM.some((k) => JSON.stringify(k) === JSON.stringify(Object.keys(it)))) errs.push(`item keys ${JSON.stringify(Object.keys(it))}`)
  if (kind === "sync_request") {
    const v = f.verdict
    if (JSON.stringify(Object.keys(v)) !== '["live","dead","listed","in_store"]') errs.push("verdict keys")
    if (v.live !== (v.dead === null) || (v.dead !== null && !DEAD.test(v.dead))) errs.push("verdict " + JSON.stringify(v))
    if (![true, false, "unknown"].includes(v.listed)) errs.push("listed")
  }
  for (const b of Object.values(f.lower ?? f.acked ?? {}))
    if (!(b.from === null || typeof b.from === "string") || typeof b.inclusive !== "boolean") errs.push("bound " + JSON.stringify(b))
  return errs
}

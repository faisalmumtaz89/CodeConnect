// The answer path against a fake OpenCode and a fake daemon: the stage frames of a reply the keyboard could give,
// every refusal of one it could not (the request de-listed, gone from the store, its session missing or idle, its
// tool call over, its epoch ended, the keyboard elsewhere, a dialog open, auto mode, words at a root, the command
// expired, the link replaced, the API missing), the result of a reply the server refused or that failed, malformed
// commands that never throw and always attest, keyboard moves injected between the facts and the reply, the cards
// auto mode never raises, the card context and its bound, the commands memory and its eviction, and the frames a
// snapshot never holds.
import { afterEach, describe, expect, test } from "bun:test"
import { createRoot, getOwner } from "solid-js/dist/solid.js"
import { fakeApi } from "./fake-api.js"
import { fakeCcd, loadPlugin, settled, sleep, until, welcome } from "./helpers.js"

const NONCE = "0123456789abcdef0123456789abcdef"
const DIR = "/Users/ada/project"
const A = "ses_a"
const B = "ses_b"
/** @type {(() => Promise<void> | void)[]} */
const cleanups = []
afterEach(async () => {
  for (const f of cleanups.splice(0).reverse()) await f()
})

/** A busy session whose newest assistant message has a running tool part `call_<n>`. @param {string} sid @param {string} n @param {string} [parentID] */
const busy = (sid, n, parentID) => ({
  session: { id: sid, parentID, directory: DIR, time: { created: Date.now() + 60000, updated: Date.now() + 60000 } },
  messages: [
    { info: { id: `msg_${n}1`, sessionID: sid, role: "user", time: { created: 1 } }, parts: [] },
    {
      info: { id: `msg_${n}2`, sessionID: sid, role: "assistant", parentID: `msg_${n}1`, time: { created: 2 } },
      parts: [{ id: `prt_${n}`, sessionID: sid, messageID: `msg_${n}2`, type: "tool", callID: `call_${n}`, tool: "bash", state: { status: "running", input: { command: "ls" } } }],
    },
  ],
})
/** @param {string} id @param {string} sid @param {string} n */
const permission = (id, sid, n) => ({ id, sessionID: sid, permission: "bash", patterns: ["ls"], metadata: { command: "ls" }, always: ["ls *"], tool: { messageID: `msg_${n}2`, callID: `call_${n}` } })
const PERM = permission("per_a", A, "a")

/** @param {ReturnType<typeof fakeApi>} f */
function serve(f) {
  const a = busy(A, "a")
  const b = busy(B, "b", A)
  f.server.sessions = [a.session, b.session]
  f.server.messages.set(A, a.messages)
  f.server.messages.set(B, b.messages)
  f.server.status = { [A]: { type: "busy" }, [B]: { type: "busy" } }
  f.storeSession({ id: A, directory: DIR })
  f.storeSession({ id: B, parentID: A, directory: DIR })
  f.navigate({ name: "session", params: { sessionID: A } })
}

/**
 * @param {{ mode?: "normal" | "auto" | null, consts?: Record<string, number>, api?: (f: ReturnType<typeof fakeApi>) => void, onHello?: (h: any, c: any) => void }} [o]
 */
async function setup(o = {}) {
  const p = await loadPlugin({ consts: o.consts })
  const ccd = fakeCcd(p.socket, { onHello: o.onHello })
  const f = fakeApi({ mode: o.mode })
  serve(f)
  o.api?.(f)
  cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())
  await p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
  const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
  return { ...p, ccd, f, c }
}

/** The server asks: the tool part runs, the request is listed, the bus says so. @param {ReturnType<typeof fakeApi>} f @param {any} perm */
function ask(f, perm) {
  const m = f.server.messages.get(perm.sessionID)?.[1]
  f.emit("message.part.updated", { sessionID: perm.sessionID, part: m?.parts[0] })
  f.server.permissions.push(perm)
  f.emit("permission.asked", perm)
}
/** @param {any} perm @param {string} reply @param {any} [extra] */
const command = (perm, reply, extra = {}) => ({ type: "opencode_answer", kind: "permission", request_id: perm.id, session_id: perm.sessionID, reply, expires_at_ms: Date.now() + 10000, ...extra })
/** @param {any} c @param {string} id */
const stages = (c, id) => c.frames.filter((/** @type {any} */ x) => x.t === "answer" && x.request_id === id).map(({ t, seq, ...rest }) => rest)
/** @param {any} c @param {string} id */
const terminal = (c, id) => stages(c, id).some((s) => s.stage === "refused" || s.stage === "result")
/** @param {any} c @param {string} id */
const done = (c, id) => until(() => terminal(c, id) && stages(c, id))
/** The plugin's reply calls; the keyboard's own carry the TUI's `workspace`. @param {ReturnType<typeof fakeApi>} f */
const replies = (f) => f.server.calls.filter((x) => x.method === "permission.reply" && x.args.workspace === undefined)
/** @param {any} c @param {string} id */
const card = (c, id) => c.frames.find((/** @type {any} */ x) => x.t === "ev" && x.type === "permission.asked" && x.properties.id === id)

describe("a reply the keyboard could give", () => {
  test("received, sent, result 200 in order; one call with the directory; the reply forwarded", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    await until(() => card(c, PERM.id))
    c.send(command(PERM, "once"))
    const got = await done(c, PERM.id)
    expect(got).toEqual([{ request_id: PERM.id, stage: "received" }, { request_id: PERM.id, stage: "sent" }, { request_id: PERM.id, stage: "result", http: 200 }])
    expect(replies(f).map((x) => x.args)).toEqual([{ requestID: PERM.id, directory: DIR, reply: "once" }])
    await until(() => c.frames.some((/** @type {any} */ x) => x.type === "permission.replied" && x.properties.requestID === PERM.id && x.properties.reply === "once"))
    const lines = c.lines.filter((l) => l.includes('"t":"answer"'))
    for (const l of lines) expect(l).toMatch(/^\{"t":"answer","seq":\d+,"request_id":"per_a","stage":"(received|sent)"\}\n$|^\{"t":"answer","seq":\d+,"request_id":"per_a","stage":"result","http":200\}\n$/)
    const seqs = c.frames.filter((/** @type {any} */ x) => x.t === "answer").map((/** @type {any} */ x) => x.seq)
    expect(seqs).toEqual([...seqs].sort((a, b) => a - b))
    expect(new Set(seqs).size).toBe(3)
  })

  test("always and reject are sent as given; a reject with words in a child session carries them", async () => {
    const { c, f } = await setup()
    const pb = permission("per_b", B, "b")
    ask(f, PERM)
    ask(f, pb)
    c.send(command(PERM, "always"))
    await done(c, PERM.id)
    expect(stages(c, PERM.id).at(-1)).toEqual({ request_id: PERM.id, stage: "result", http: 200 })
    f.navigate({ name: "session", params: { sessionID: B } })
    c.send(command(pb, "reject", { message: "use the other file" }))
    const got = await done(c, pb.id)
    expect(got.at(-1)).toEqual({ request_id: pb.id, stage: "result", http: 200 })
    expect(replies(f).map((x) => x.args)).toEqual([
      { requestID: PERM.id, directory: DIR, reply: "always" },
      { requestID: pb.id, directory: DIR, reply: "reject", message: "use the other file" },
    ])
  })

  test("a stage frame and a permission.replied are written at once during a snapshot; ordinary events wait", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    await until(() => card(c, PERM.id))
    f.server.delay = 60
    c.send({ type: "opencode_resync" })
    await until(() => f.server.calls.filter((x) => x.method === "session.list").length === 2)
    c.send(command(PERM, "once"))
    f.emit("permission.replied", { sessionID: A, requestID: "per_other", reply: "reject" })
    f.emit("session.status", { sessionID: A, status: { type: "busy" } })
    await until(() => settled(c, 2))
    // the hold lasts from sync_begin to sync_end; what was held is flushed between sync_end and settled
    const at = (/** @type {(x: any) => boolean} */ p) => c.frames.findIndex(p)
    const syncBegin = c.frames.findIndex((/** @type {any} */ x) => x.t === "sync_begin" && x.sync === 2)
    const syncEnd = c.frames.findIndex((/** @type {any} */ x) => x.t === "sync_end" && x.sync === 2)
    expect(syncBegin).toBeGreaterThan(0)
    for (const stage of ["received", "sent", "result"]) {
      const i = at((x) => x.t === "answer" && x.stage === stage)
      expect([stage, i > syncBegin && i < syncEnd]).toEqual([stage, true])
    }
    const replied = at((x) => x.type === "permission.replied" && x.properties.requestID === "per_other")
    expect(replied > syncBegin && replied < syncEnd).toBe(true)
    expect(at((x) => x.type === "session.status" && x.seq > c.frames[syncBegin].as_of)).toBeGreaterThan(syncEnd)
    expect(stages(c, PERM.id).at(-1)).toEqual({ request_id: PERM.id, stage: "result", http: 200 })
  })
})

describe("refusals before the call", () => {
  test("the server no longer lists the request: not-listed, no call", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    f.server.permissions = []
    c.send(command(PERM, "once"))
    expect(await done(c, PERM.id)).toEqual([{ request_id: PERM.id, stage: "received" }, { request_id: PERM.id, stage: "refused", reason: "not-listed", detail: "the server no longer lists the request" }])
    expect(replies(f)).toEqual([])
  })

  test("a failed list falls back to the store: in it the reply goes, not in it the reply is refused", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    f.server.failing.add("permission.list")
    c.send(command(PERM, "once"))
    expect((await done(c, PERM.id)).at(-1)).toEqual({ request_id: PERM.id, stage: "result", http: 200 })
    const p2 = permission("per_a2", A, "a")
    f.server.permissions.push(p2)
    c.send(command(p2, "once"))
    expect((await done(c, p2.id)).at(-1)).toMatchObject({ stage: "refused", reason: "not-in-store" })
    expect(replies(f).length).toBe(1)
  })

  test("the session and its status: 404 is session-missing, any other failure is server, idle is not-busy", async () => {
    const { c, f } = await setup()
    const cases = /** @type {const} */ ([
      ["per_s1", () => f.server.failing.add("session.get"), "server"],
      ["per_s2", () => void (f.server.sessions = f.server.sessions.filter((s) => s.id !== A)), "session-missing"],
      ["per_s2b", () => void (f.server.sessions = [busy(A, "a").session, ...f.server.sessions]), "once"],
      ["per_s3", () => f.server.failing.add("session.status"), "server"],
      ["per_s4", () => void (f.server.status[A] = { type: "idle" }), "not-busy"],
      ["per_s5", () => f.server.failing.add("session.message"), "server"],
      ["per_s6", () => void (f.server.messages.get(A)[1].parts[0].state.status = "completed"), "anchor"],
    ])
    for (const [id, arrange, reason] of cases) {
      const p = permission(id, A, "a")
      ask(f, p)
      arrange()
      c.send(command(p, "once"))
      expect((await done(c, id)).at(-1)).toMatchObject(reason === "once" ? { stage: "result", http: 200 } : { stage: "refused", reason })
      f.server.failing.clear()
      f.server.status[A] = { type: "busy" }
    }
    expect(replies(f).length).toBe(1)
  })

  test("a request without a tool is never anchored; a doom-loop request is anchored by its matching running part", async () => {
    const { c, f } = await setup()
    const noTool = { id: "per_nt", sessionID: A, permission: "external_directory", patterns: ["/x"], metadata: {}, always: [] }
    f.server.permissions.push(noTool)
    f.emit("permission.asked", noTool)
    c.send(command(noTool, "once"))
    expect((await done(c, noTool.id)).at(-1)).toMatchObject({ stage: "refused", reason: "anchor" })
    const doom = { id: "per_dl", sessionID: A, permission: "doom_loop", patterns: ["*"], metadata: { tool: "bash", input: { command: "ls" } }, always: [] }
    f.server.permissions.push(doom)
    f.emit("permission.asked", doom)
    c.send(command(doom, "reject"))
    expect((await done(c, doom.id)).at(-1)).toEqual({ request_id: doom.id, stage: "result", http: 200 })
    expect(replies(f).length).toBe(1)
  })

  test("the keyboard elsewhere: another root, home, a grandchild; a dialog open; a parent's screen still answers its child", async () => {
    const { c, f } = await setup()
    f.server.sessions.push({ id: "ses_c", parentID: B, directory: DIR, time: { created: Date.now() + 60000 } })
    f.server.messages.set("ses_c", busy("ses_c", "c", B).messages)
    f.server.status["ses_c"] = { type: "busy" }
    f.storeSession({ id: "ses_c", parentID: B, directory: DIR })
    const pc = permission("per_c", "ses_c", "c")
    const cases = /** @type {const} */ ([
      ["per_v1", A, () => f.navigate({ name: "session", params: { sessionID: "ses_other" } }), "not-visible"],
      ["per_v2", A, () => f.navigate({ name: "home" }), "not-visible"],
      ["per_v3", A, () => f.setDialog(true), "dialog"],
    ])
    for (const [id, sid, arrange, reason] of cases) {
      const p = permission(id, sid, "a")
      ask(f, p)
      arrange()
      c.send(command(p, "once"))
      expect((await done(c, id)).at(-1)).toMatchObject({ stage: "refused", reason })
      f.navigate({ name: "session", params: { sessionID: A } })
      f.setDialog(false)
    }
    ask(f, pc)
    c.send(command(pc, "once"))
    expect((await done(c, pc.id)).at(-1)).toMatchObject({ stage: "refused", reason: "not-visible" })
    expect(replies(f)).toEqual([])
    const pb = permission("per_b", B, "b")
    ask(f, pb)
    c.send(command(pb, "once"))
    expect((await done(c, pb.id)).at(-1)).toEqual({ request_id: pb.id, stage: "result", http: 200 })
  })

  test("words with a reject at a root: words-at-root, also when the parent vanished between card and tap", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    c.send(command(PERM, "reject", { message: "no" }))
    expect((await done(c, PERM.id)).at(-1)).toMatchObject({ stage: "refused", reason: "words-at-root" })
    const pb = permission("per_b", B, "b")
    f.navigate({ name: "session", params: { sessionID: B } })
    ask(f, pb)
    expect((await until(() => card(c, pb.id))).properties.ctx.parent).toBe(A)
    f.storeSession({ id: B, directory: DIR })
    f.server.sessions.find((s) => s.id === B).parentID = undefined
    c.send(command(pb, "reject", { message: "no" }))
    expect((await done(c, pb.id)).at(-1)).toMatchObject({ stage: "refused", reason: "words-at-root" })
    expect(replies(f)).toEqual([])
  })

  test("an epoch ended since the ask: an abort, an ancestor's abort, a dispose", async () => {
    const { c, f } = await setup()
    const pb = permission("per_b", B, "b")
    f.emit("session.created", { info: { id: B, parentID: A } })
    f.navigate({ name: "session", params: { sessionID: B } })
    const cases = /** @type {const} */ ([
      ["per_e1", () => f.emit("session.error", { sessionID: B, error: { name: "MessageAbortedError" } }), "epoch:abort"],
      ["per_e2", () => f.emit("session.error", { sessionID: A, error: { name: "MessageAbortedError" } }), "epoch:abort:ancestor"],
      ["per_e3", () => f.emit("server.instance.disposed", {}), "epoch:disposed"],
    ])
    for (const [id, arrange, detail] of cases) {
      const p = { ...pb, id }
      ask(f, p)
      arrange()
      c.send(command(p, "once"))
      expect((await done(c, id)).at(-1)).toEqual({ request_id: id, stage: "refused", reason: "epoch", detail })
    }
    expect(replies(f)).toEqual([])
  })

  test("an expired command is refused without a read of the server's facts being trusted", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    c.send(command(PERM, "once", { expires_at_ms: Date.now() - 1 }))
    expect((await done(c, PERM.id)).at(-1)).toMatchObject({ stage: "refused", reason: "expired" })
    expect(replies(f)).toEqual([])
  })

  test("a missing permission.reply is named in the hello and refuses the command", async () => {
    const { c, f } = await setup({ api: (f) => delete (/** @type {any} */ (f.api.client.permission).reply) })
    expect(c.hello.api).toEqual({ ok: false, missing: ["api.client.permission.reply"], version: "1.18.34" })
    ask(f, PERM)
    c.send(command(PERM, "once"))
    expect((await done(c, PERM.id)).at(-1)).toMatchObject({ stage: "refused", reason: "api" })
  })

  test("a store read that throws at the section: refused:error, remembered, no call", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    const read = f.api.state.session.permission
    f.server.before["session.get"] = () => {
      f.api.state.session.permission = () => {
        throw new Error("store gone")
      }
    }
    c.send(command(PERM, "once"))
    const got = await done(c, PERM.id)
    f.api.state.session.permission = read
    expect(got.at(-1)).toEqual({ request_id: PERM.id, stage: "refused", reason: "error", detail: "store gone" })
    expect(replies(f)).toEqual([])
    delete f.server.before["session.get"]
    c.send({ type: "opencode_resync" })
    await until(() => settled(c, 2))
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_begin" && x.sync === 2).commands).toEqual([{ request_id: PERM.id, stage: "refused", reason: "error" }])
  })
})

describe("the link that carried the command", () => {
  test("gone before the call: refused:link remembered, carried by the next link's snapshot, and written to a later link of the same activation", async () => {
    const { c, f, ccd } = await setup({ consts: { BACKOFF_MIN_MS: 100, BACKOFF_MAX_MS: 100 } })
    ask(f, PERM)
    await until(() => card(c, PERM.id))
    let cut = 0
    f.server.before["session.get"] = async () => {
      if (cut++) return
      await sleep(20)
      c.socket.destroy()
      await sleep(50)
    }
    c.send(command(PERM, "once"))
    const c2 = await until(() => ccd.conns.length === 2 && settled(ccd.last(), 2) && ccd.last(), 4000)
    expect(stages(c, PERM.id)).toEqual([{ request_id: PERM.id, stage: "received" }])
    expect(c2.frames.find((/** @type {any} */ x) => x.t === "sync_begin").commands).toEqual([{ request_id: PERM.id, stage: "refused", reason: "link" }])
    expect(replies(f)).toEqual([])

    // the same, with the redial landing before the section: the refusal is written to the new link
    const p2 = permission("per_a2", A, "a")
    ask(f, p2)
    cut = 0
    f.server.before["session.get"] = async () => {
      if (cut++) return
      await sleep(20)
      c2.socket.destroy()
      await sleep(600)
    }
    c2.send(command(p2, "once"))
    const c3 = await until(() => ccd.conns.length === 3 && stages(ccd.last(), p2.id).length && ccd.last(), 6000)
    expect(stages(c3, p2.id)).toEqual([{ request_id: p2.id, stage: "refused", reason: "link", detail: "the link that carried the command is gone" }])
    expect(replies(f)).toEqual([])
  })
})

describe("the server's answer", () => {
  test("404 (answered by someone else first), 500, and a call that throws: sent, then the result, never a refusal", async () => {
    const { c, f } = await setup()
    const cases = /** @type {const} */ ([
      ["per_r1", () => void (f.server.before["permission.reply"] = () => void (f.server.permissions = f.server.permissions.filter((p) => p.id !== "per_r1"))), 404],
      ["per_r2", () => f.server.failing.add("permission.reply"), 500],
      ["per_r3", () => f.server.throwing.add("permission.reply"), 0],
    ])
    for (const [id, arrange, http] of cases) {
      const p = permission(id, A, "a")
      ask(f, p)
      arrange()
      c.send(command(p, "once"))
      expect((await done(c, id)).map((s) => s.stage)).toEqual(["received", "sent", "result"])
      expect(stages(c, id).at(-1)).toEqual({ request_id: id, stage: "result", http })
      delete f.server.before["permission.reply"]
      f.server.failing.clear()
      f.server.throwing.clear()
    }
    expect(replies(f).length).toBe(3)
  })
})

describe("malformed commands and injected keyboard moves", () => {
  test("no malformed command throws; one with a request id is attested, one without is ignored; a question is not wired", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    const bad = [
      {},
      { kind: "permission" },
      { request_id: 5 },
      { request_id: [] },
      { request_id: null },
      { request_id: { per: "a" } },
      { request_id: "per_m1", kind: "question", session_id: A, reply: "once", expires_at_ms: Date.now() + 10000 },
      { request_id: "per_m2", kind: "permission", session_id: 7, reply: "once", expires_at_ms: Date.now() + 10000 },
      { request_id: "per_m3", kind: "permission", session_id: A, reply: "maybe", expires_at_ms: Date.now() + 10000 },
      { request_id: "per_m4", kind: "permission", session_id: A, reply: "once", expires_at_ms: "soon" },
      { request_id: "per_m5", kind: "permission", session_id: A, reply: "once", message: "words", expires_at_ms: Date.now() + 10000 },
      { request_id: "per_m6", kind: "permission", session_id: A, reply: "reject", message: 9, expires_at_ms: Date.now() + 10000 },
      { request_id: "per_m7", kind: "permission", session_id: A, reply: "once", expires_at_ms: Date.now() + 10000, extra: { deep: [null, {}] } },
      { request_id: "per_m8", kind: ["permission"], session_id: {}, reply: null, expires_at_ms: [1] },
      { request_id: PERM.id, kind: "permission", session_id: ["ses_a"], reply: "once", expires_at_ms: Date.now() + 10000 },
    ]
    for (const m of bad) c.send({ type: "opencode_answer", ...m })
    const ids = ["per_m1", "per_m2", "per_m3", "per_m4", "per_m5", "per_m6", "per_m8", PERM.id]
    await until(() => ids.every((id) => terminal(c, id)))
    for (const id of ids) expect(stages(c, id)).toEqual([{ request_id: id, stage: "received" }, { request_id: id, stage: "refused", reason: "error", detail: "malformed opencode_answer" }])
    expect((await done(c, "per_m7")).at(-1)).toEqual({ request_id: "per_m7", stage: "refused", reason: "not-listed", detail: "the server no longer lists the request" })
    expect(c.frames.filter((/** @type {any} */ x) => x.t === "answer").map((/** @type {any} */ x) => x.request_id).filter((id) => !ids.includes(id) && id !== "per_m7")).toEqual([])
    expect(replies(f)).toEqual([])
    expect(c.closed).toBe(false)
    f.emit("permission.v2.asked", { id: "per_v2", sessionID: A, action: "ask" })
    c.send(command(PERM, "once"))
    await until(() => stages(c, PERM.id).length === 5)
    expect(stages(c, PERM.id).slice(2)).toEqual([{ request_id: PERM.id, stage: "received" }, { request_id: PERM.id, stage: "sent" }, { request_id: PERM.id, stage: "result", http: 200 }])
    expect(replies(f).length).toBe(1)
  })

  test("a move injected between the facts and the section refuses the reply; a queued user message does not", async () => {
    const { c, f } = await setup()
    const moves = /** @type {const} */ ([
      ["route", () => f.navigate({ name: "home" }), "not-visible"],
      ["esc", () => void f.api.client.permission.reply({ requestID: current, reply: "reject", directory: DIR, workspace: "keyboard" }), "not-in-store"],
      ["abort", () => f.emit("session.error", { sessionID: A, error: { name: "MessageAbortedError" } }), "epoch"],
      ["dispose", () => f.emit("server.instance.disposed", {}), "epoch"],
      ["dialog", () => f.setDialog(true), "dialog"],
      ["auto", () => f.setMode("auto"), "mode"],
      // the list is read beside the session, so the server's answer may or may not have it: either refusal is right
      ["delist", () => void (f.server.permissions = f.server.permissions.filter((p) => p.id !== current)), /not-listed|not-in-store/],
      ["idle", () => void (f.server.status[A] = { type: "idle" }), "not-busy"],
    ])
    const benign = () => f.emit("message.updated", { sessionID: A, info: { id: "msg_u" + n, sessionID: A, role: "user", time: { created: Date.now() } } })
    let current = ""
    let n = 0
    /** @type {(() => void)[]} */
    let inject = []
    let fired = false
    f.server.before["session.get"] = () => {
      if (fired) return
      fired = true
      for (const i of inject) i()
    }
    const reset = () => {
      f.navigate({ name: "session", params: { sessionID: A } })
      f.setDialog(false)
      f.setMode("normal")
      f.server.status[A] = { type: "busy" }
    }
    /** @param {string[]} names */
    const run = async (names) => {
      const p = permission("per_x" + ++n, A, "a")
      current = p.id
      inject = names.map((name) => (name === "user" ? benign : /** @type {() => void} */ (moves.find((m) => m[0] === name)?.[1])))
      fired = false
      ask(f, p)
      c.send(command(p, "once"))
      const got = await done(c, p.id)
      reset()
      return got.at(-1)
    }
    for (const [name, , reason] of moves) expect([name, await run([name])]).toEqual([name, expect.objectContaining({ stage: "refused", reason: expect.stringMatching(reason) })])
    expect(replies(f)).toEqual([])
    expect(await run(["user"])).toEqual({ request_id: current, stage: "result", http: 200 })
    expect(await run([])).toEqual({ request_id: current, stage: "result", http: 200 })
    expect(replies(f).length).toBe(2)
    // random combinations: a reply goes out exactly when no move was injected, and every run attests
    let seed = 7
    const rand = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648
    for (let i = 0; i < 24; i++) {
      const names = [...moves.map((m) => m[0]), "user"].filter(() => rand() < 0.25)
      const before = replies(f).length
      const last = await run(names)
      const refusing = names.filter((x) => x !== "user")
      if (refusing.length) expect([names, last.stage]).toEqual([names, "refused"])
      else expect([names, last]).toEqual([names, { request_id: current, stage: "result", http: 200 }])
      expect(replies(f).length).toBe(before + (refusing.length ? 0 : 1))
    }
  })
})

describe("auto mode", () => {
  test("an ask under auto mode raises no card; OpenCode's own reply is forwarded", async () => {
    const { c, f } = await setup({ mode: "auto" })
    ask(f, PERM)
    await until(() => c.frames.some((/** @type {any} */ x) => x.type === "permission.replied" && x.properties.requestID === PERM.id && x.properties.reply === "once"))
    await sleep(100)
    expect(card(c, PERM.id)).toBeUndefined()
    expect(f.store.permissions).toEqual([])
  })

  test("an unknown mode keeps the card but marks its context unusable", async () => {
    const { c, f } = await setup({ mode: null })
    ask(f, PERM)
    const got = await until(() => card(c, PERM.id))
    expect(got.properties.ctx).toEqual({ ok: false, tool_ids: ["bash", "edit", "read", "question"], mcp: [], mode: "unknown", parent: null, ms: expect.any(Number) })
    c.send(command(PERM, "once"))
    expect((await done(c, PERM.id)).at(-1)).toMatchObject({ stage: "refused", reason: "mode", detail: "the permission mode is unknown" })
  })

  /** Renders the app slot under an owner holding `context` in place of the fake's. @param {any} context */
  const renderUnder = (context) => (/** @type {ReturnType<typeof fakeApi>} */ f) => {
    f.api.slots.register = (/** @type {any} */ plugin) => {
      createRoot((_dispose) => {
        ;/** @type {any} */ (getOwner()).context = context
        plugin.slots?.app?.({}, {})
      })
      return "slot-1"
    }
  }
  const heads = (/** @type {any} */ c) => c.frames.filter((/** @type {any} */ x) => x.t === "head")

  test("a context value whose read throws is skipped: the head is pushed and the Permission context beside it is found", async () => {
    const hostile = {
      get permission() {
        throw new Error("not ready")
      },
    }
    const permission = { get mode() { return "normal" }, set() {}, toggle() {} }
    const { c, f } = await setup({ api: renderUnder({ [Symbol("Permission")]: permission, [Symbol("Other")]: hostile }) })
    await until(() => heads(c).length === 1)
    expect(heads(c)[0]).toMatchObject({ route: "session", session_id: A, directory: DIR })
    ask(f, PERM)
    expect((await until(() => card(c, PERM.id))).properties.ctx).toMatchObject({ ok: true, mode: "normal" })
  })

  test("a context that throws on enumeration leaves the mode unknown, and the head is still pushed", async () => {
    const context = new Proxy({}, { ownKeys() { throw new Error("not ready") } })
    const { c, f } = await setup({ api: renderUnder(context) })
    await until(() => heads(c).length === 1)
    expect(heads(c)[0]).toMatchObject({ route: "session", session_id: A, directory: DIR })
    ask(f, PERM)
    expect((await until(() => card(c, PERM.id))).properties.ctx).toMatchObject({ ok: false, mode: "unknown" })
  })

  test("a request pending when auto was switched on: refused at the tap, listed by a snapshot under ctx.mode auto", async () => {
    const { c, f } = await setup()
    ask(f, PERM)
    await until(() => card(c, PERM.id))
    f.setMode("auto")
    c.send(command(PERM, "once"))
    expect((await done(c, PERM.id)).at(-1)).toMatchObject({ stage: "refused", reason: "mode", detail: "the permission mode is auto" })
    c.send({ type: "opencode_resync" })
    await until(() => settled(c, 2))
    const r = c.frames.find((/** @type {any} */ x) => x.t === "sync_request" && x.sync === 2)
    expect(r.request.ctx).toMatchObject({ ok: true, mode: "auto", parent: null })
    expect(r.verdict).toEqual({ live: true, dead: null, listed: true, in_store: true })
    expect(replies(f)).toEqual([])
  })
})

describe("card context", () => {
  test("tool ids, connected MCP servers and the parent ride the card and the snapshot request", async () => {
    const { c, f } = await setup({ api: (f) => void (f.server.mcp = { s7mcp: { status: "connected" }, apply: { status: "failed" } }) })
    const pb = permission("per_b", B, "b")
    ask(f, pb)
    const got = await until(() => card(c, pb.id))
    expect(got.properties.ctx).toEqual({ ok: true, tool_ids: ["bash", "edit", "read", "question"], mcp: ["s7mcp"], mode: "normal", parent: A, ms: expect.any(Number) })
    expect(got.properties.anchor_input).toEqual({ command: "ls" })
    expect(Object.keys(got.properties)).toEqual([...Object.keys(pb), "anchor_input", "ctx"])
    c.send({ type: "opencode_resync" })
    await until(() => settled(c, 2))
    const r = c.frames.find((/** @type {any} */ x) => x.t === "sync_request" && x.sync === 2)
    expect({ ...r.request.ctx, ms: 0 }).toEqual({ ...got.properties.ctx, ms: 0 })
    expect(f.server.calls.filter((x) => x.method === "tool.ids").length).toBe(2)
  })

  test("a failed or late read sends the card with ctx.ok false", async () => {
    const { c, f } = await setup({ consts: { CTX_READ_MS: 150 } })
    f.server.failing.add("mcp.status")
    ask(f, PERM)
    expect((await until(() => card(c, PERM.id))).properties.ctx).toMatchObject({ ok: false, tool_ids: [], mcp: [] })
    f.server.failing.clear()
    f.server.before["tool.ids"] = () => sleep(400)
    const p2 = permission("per_a2", A, "a")
    const t0 = Date.now()
    ask(f, p2)
    const late = await until(() => card(c, p2.id))
    expect(Date.now() - t0).toBeLessThan(350)
    expect(late.properties.ctx).toMatchObject({ ok: false, mode: "normal" })
    expect(late.properties.ctx.ms).toBeGreaterThanOrEqual(150)
    f.server.toolIds = /** @type {any} */ (["bash", 3])
    delete f.server.before["tool.ids"]
    const p3 = permission("per_a3", A, "a")
    ask(f, p3)
    expect((await until(() => card(c, p3.id))).properties.ctx.ok).toBe(false)
  })
})

describe("the commands memory", () => {
  test("keeps the newest stage per request id in first-seen order, evicts the oldest past the cap, and rides sync_begin only when non-empty", async () => {
    const { c, f } = await setup({ consts: { COMMANDS_CAP: 3 } })
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_begin")).not.toHaveProperty("commands")
    const ids = ["per_1", "per_2", "per_3", "per_4"]
    for (const id of ids) {
      c.send(command({ id, sessionID: A }, "once"))
      await done(c, id)
    }
    ask(f, permission("per_3", A, "a"))
    c.send(command({ id: "per_3", sessionID: A }, "once"))
    await until(() => stages(c, "per_3").some((s) => s.stage === "result"))
    c.send({ type: "opencode_resync" })
    await until(() => settled(c, 2))
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_begin" && x.sync === 2).commands).toEqual([
      { request_id: "per_2", stage: "refused", reason: "not-listed" },
      { request_id: "per_3", stage: "result", http: 200 },
      { request_id: "per_4", stage: "refused", reason: "not-listed" },
    ])
    c.send(command({ id: "per_5", sessionID: A }, "once"))
    await done(c, "per_5")
    c.send({ type: "opencode_resync" })
    await until(() => settled(c, 3))
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_begin" && x.sync === 3).commands.map((/** @type {any} */ x) => x.request_id)).toEqual(["per_3", "per_4", "per_5"])
  })
})

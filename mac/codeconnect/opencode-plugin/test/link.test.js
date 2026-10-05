// The link against a fake daemon on a real unix socket: hello, refusals, timeouts, a FIN that OpenCode does not
// report, backoff, the queue cap, the paged snapshot and its bounds, held frames and `settled`, coalesced
// triggers, the head, and dispose.
import { afterEach, describe, expect, test } from "bun:test"
import { spawnSync } from "node:child_process"
import { mkdirSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { fakeApi } from "./fake-api.js"
import { fakeCcd, loadPlugin, refuse, settled, sleep, START, until, welcome, writeAgent } from "./helpers.js"

const NONCE = "0123456789abcdef0123456789abcdef"
/** @type {(() => Promise<void> | void)[]} */
let cleanups = []
afterEach(async () => {
  for (const f of cleanups.splice(0).reverse()) await f()
})

/**
 * @param {{ agent?: boolean, onHello?: (h: any, c: any) => void, read?: boolean, api?: (f: ReturnType<typeof fakeApi>) => void }} [o]
 */
async function setup(o = {}) {
  const p = await loadPlugin({ agent: o.agent })
  const ccd = fakeCcd(p.socket, { onHello: o.onHello, read: o.read })
  const f = fakeApi()
  o.api?.(f)
  cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())
  await p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
  return { ...p, ccd, f }
}

const pad = (/** @type {number} */ n) => String(n).padStart(3, "0")
/** @param {string} sid @param {number} n @param {string} [role] @param {any} [extra] */
const msg = (sid, n, role = "assistant", extra = {}) => ({
  info: { id: `msg_${sid}_${pad(n)}`, sessionID: sid, role, time: { created: 1000 + n, completed: role === "assistant" ? 1000 + n : undefined } },
  parts: [{ id: `prt_${sid}_${pad(n)}`, sessionID: sid, messageID: `msg_${sid}_${pad(n)}`, type: "text", text: "text " + n, ...extra }],
})
/** @param {string} id @param {number} created @param {string} [parentID] */
const session = (id, created, parentID) => ({ id, parentID, directory: "/Users/ada/project", time: { created, updated: created } })
/** @param {any} conn */
const kinds = (conn) => conn.frames.map((/** @type {any} */ f) => f.t ?? f.type)
/** @param {any} conn @param {number} sync */
const items = (conn, sync) => conn.frames.filter((/** @type {any} */ f) => f.t === "sync_page" && f.sync === sync).flatMap((/** @type {any} */ f) => f.items)
/** @param {any} conn @param {number} sync */
const msgIds = (conn, sync) => items(conn, sync).filter((/** @type {any} */ i) => i.info).map((/** @type {any} */ i) => i.info.id)

describe("hello", () => {
  test("is the first line, exactly", async () => {
    const { ccd } = await setup({ onHello: () => {} })
    const c = await until(() => ccd.last()?.hello && ccd.last())
    expect(c.lines[0]).toBe(
      JSON.stringify({
        type: "opencode_hello", wire: 1, nonce: NONCE, pid: process.pid, start: START, activation: 1, directory: "/Users/ada/project",
        api: { ok: true, missing: [], version: "1.18.34" },
      }) + "\n",
    )
  })

  test("names every missing API member and stays observe-only", async () => {
    const { ccd } = await setup({
      onHello: () => {},
      api: (f) => {
        delete (/** @type {any} */ (f.api.client).question)
        ;/** @type {any} */ (f.api.app).version = undefined
      },
    })
    const c = await until(() => ccd.last()?.hello && ccd.last())
    expect(c.hello.api).toEqual({ ok: false, missing: ["api.client.question.list", "api.app.version"], version: null })
  })

  test("does not dial until agent.json exists", async () => {
    const { ccd, dir } = await setup({ agent: false })
    await sleep(700)
    expect(ccd.conns.length).toBe(0)
    writeAgent(dir)
    await until(() => ccd.last()?.hello, 3000)
  })

  test("agent.json is read only as a regular file of at most 4 KiB; a FIFO there does not hold the read", async () => {
    const { mod, dir } = await loadPlugin()
    const at = (/** @type {string} */ n) => join(dir, n)
    writeFileSync(at("ok.json"), JSON.stringify({ pid: 1, start: START }))
    expect(await mod.readStart(at("ok.json"))).toEqual(START)
    writeFileSync(at("big.json"), JSON.stringify({ pid: 1, start: START, pad: "x".repeat(5000) }))
    expect(await mod.readStart(at("big.json"))).toBeNull()
    expect(spawnSync("mkfifo", [at("fifo.json")]).status).toBe(0)
    const t0 = Date.now()
    expect(await mod.readStart(at("fifo.json"))).toBeNull()
    expect(Date.now() - t0).toBeLessThan(1000)
    mkdirSync(at("dir.json"))
    expect(await mod.readStart(at("dir.json"))).toBeNull()
    expect(await mod.readStart(at("missing.json"))).toBeNull()
  })

  test("a final refusal stops dialing; a non-final one backs off and dials again", async () => {
    let n = 0
    const a = await setup({ onHello: (_h, c) => refuse(c, true) })
    await until(() => a.ccd.conns.length === 1 && a.ccd.last().closed)
    await sleep(1500)
    expect(a.ccd.conns.length).toBe(1)
    const b = await setup({ onHello: (_h, c) => refuse(c, ++n >= 3) })
    await until(() => b.ccd.conns.length === 3 && b.ccd.last().closed, 6000)
    await sleep(2500)
    expect(b.ccd.conns.length).toBe(3)
  }, 15000)

  test("an unanswered hello times out after 3 s and is dialed again", async () => {
    const { ccd } = await setup({ onHello: () => {} })
    const c = await until(() => ccd.last()?.hello && ccd.last())
    const t0 = Date.now()
    await until(() => c.closed, 5000)
    const waited = Date.now() - t0
    expect(waited).toBeGreaterThan(2700)
    expect(waited).toBeLessThan(3600)
    await until(() => ccd.conns.length === 2, 3000)
  }, 10000)
})

describe("connection", () => {
  test("a FIN alone (no close event) still ends the link, and it reconnects", async () => {
    const { ccd } = await setup()
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    c.socket.end()
    await until(() => ccd.conns.length === 2 && settled(ccd.last(), 2), 3000)
  })

  test("backoff doubles from 0.5 s to 8 s with ±20% jitter", async () => {
    const { mod } = await loadPlugin()
    for (let a = 0; a < 10; a++) {
      const base = Math.min(8000, 500 * 2 ** a)
      expect(mod.backoffDelay(a, 0)).toBe(Math.round(base * 0.8))
      expect(mod.backoffDelay(a, 0.999999)).toBeLessThanOrEqual(Math.round(base * 1.2))
      expect(mod.backoffDelay(a, 0.5)).toBe(base)
    }
  })

  test("a queue over 4 MiB closes the link instead of growing, and the next link resyncs", async () => {
    const { ccd, f } = await setup({ read: false })
    const c = await until(() => ccd.last())
    welcome(c)
    await sleep(100)
    const big = "y".repeat(60000)
    const part = (/** @type {number} */ i) => ({ sessionID: "ses_q", part: { id: "prt_" + i, sessionID: "ses_q", type: "tool", callID: "c" + i, state: { status: "completed", input: Object.fromEntries(Array.from({ length: 15 }, (_, k) => ["f" + k, big])) } } })
    for (let i = 0; i < 200 && !c.closed; i++) {
      f.emit("message.part.updated", part(i))
      await sleep(1)
    }
    await until(() => c.closed, 3000)
    const next = await until(() => ccd.conns.length === 2 && ccd.last(), 3000)
    next.socket.resume()
    await until(() => next.hello)
    welcome(next)
    await until(() => next.frames.some((/** @type {any} */ x) => x.t === "settled"), 5000)
    expect(next.frames.find((/** @type {any} */ x) => x.t === "sync_begin")).toMatchObject({ reason: "connect", scope: "full" })
  })

  test("the backoff starts over only once a link settles, not at a welcome", async () => {
    const { ccd } = await setup({
      onHello: (_h, c) => {
        welcome(c)
        setTimeout(() => c.socket.destroy(), 5)
      },
      api: (f) => (f.server.delay = 100),
    })
    await until(() => ccd.conns.length === 4, 8000)
    const t = ccd.conns.map((/** @type {any} */ c) => c.at)
    const gaps = t.slice(1).map((/** @type {number} */ v, /** @type {number} */ i) => v - t[i])
    expect(gaps[0]).toBeLessThan(900)
    expect(gaps[2]).toBeGreaterThan(1500)
  }, 12000)

  test("dispose closes the link and stops reconnecting", async () => {
    const { ccd, f } = await setup()
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    f.dispose()
    await until(() => c.closed)
    await sleep(1200)
    expect(ccd.conns.length).toBe(1)
    expect(f.handlerCount()).toBe(0)
  })
})

describe("sync", () => {
  test("a welcome starts a full sync: begin, pages, requests, end, settled", async () => {
    const { ccd } = await setup({
      api: (f) => {
        f.server.sessions = [session("ses_a", Date.now() + 60000)]
        f.server.messages.set("ses_a", [msg("ses_a", 1, "user"), msg("ses_a", 2)])
        f.server.status = { ses_a: { type: "busy" } }
      },
    })
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    expect(kinds(c)).toEqual(["opencode_hello", "head", "sync_begin", "sync_page", "sync_end", "settled"])
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_end")).toMatchObject({ list_ok: true, requests_ok: true })
    const [begin, , end] = c.frames.slice(2)
    expect(begin).toEqual({ t: "sync_begin", sync: 1, reason: "connect", scope: "full", as_of: 1, status: { ses_a: { type: "busy" } }, status_ok: true })
    expect(end).toMatchObject({ t: "sync_end", sync: 1, permissions_ok: true, questions_ok: true, lower: { ses_a: { from: null, inclusive: false } }, pages: 1, items: 3, activation: 1 })
    expect(msgIds(c, 1)).toEqual(["msg_ses_a_002", "msg_ses_a_001"])
  })

  test("pages each root newest-first down to its lower bound", async () => {
    const T = Date.now()
    const { ccd, f } = await setup({
      onHello: () => {},
      api: (f) => {
        f.server.sessions = [
          session("ses_acked", 1), session("ses_incl", 1), session("ses_new", T + 60000), session("ses_prompted", 1),
          session("ses_busy", 1), session("ses_idle", 1), session("ses_kid_old", 5, "ses_new"), session("ses_kid", T + 70000, "ses_new"),
        ]
        for (const s of ["ses_acked", "ses_incl", "ses_new", "ses_prompted", "ses_busy", "ses_idle"])
          f.server.messages.set(s, Array.from({ length: 25 }, (_, i) => msg(s, i + 1, i % 5 === 0 ? "user" : "assistant")))
        f.server.messages.set("ses_kid", [msg("ses_kid", 1, "user"), msg("ses_kid", 2)])
        f.server.messages.set("ses_kid_old", [msg("ses_kid_old", 1, "user")])
        f.server.status = { ses_busy: { type: "busy" } }
      },
    })
    const c = await until(() => ccd.last()?.hello && ccd.last())
    for (const [s, n] of [["ses_prompted", 21], ["ses_new", 21]])
      f.emit("message.updated", { sessionID: s, info: { id: `msg_${s}_0${n}`, sessionID: s, role: "user", time: { created: Date.now() } } })
    welcome(c, { ses_acked: { from: "msg_ses_acked_010", inclusive: false }, ses_incl: { from: "msg_ses_incl_010", inclusive: true } })
    await until(() => settled(c, 1))
    const ids = msgIds(c, 1)
    const of = (/** @type {string} */ s) => ids.filter((/** @type {string} */ i) => i.startsWith(`msg_${s}_`)).map((/** @type {string} */ i) => Number(i.slice(-3))).sort((a, b) => a - b)
    const range = (/** @type {number} */ a, /** @type {number} */ b) => Array.from({ length: b - a + 1 }, (_, i) => a + i)
    expect(of("ses_acked")).toEqual(range(11, 25))
    expect(of("ses_incl")).toEqual(range(10, 25))
    expect(of("ses_new")).toEqual(range(1, 25))
    expect(of("ses_prompted")).toEqual(range(21, 25))
    expect(of("ses_busy")).toEqual(range(21, 25))
    expect(of("ses_idle")).toEqual([])
    expect(of("ses_kid")).toEqual([1, 2])
    expect(of("ses_kid_old")).toEqual([])
    const end = c.frames.find((/** @type {any} */ x) => x.t === "sync_end")
    expect(end.lower).toEqual({
      ses_acked: { from: "msg_ses_acked_010", inclusive: false },
      ses_incl: { from: "msg_ses_incl_010", inclusive: true },
      ses_new: { from: null, inclusive: false },
      ses_prompted: { from: "msg_ses_prompted_021", inclusive: true },
      ses_busy: { from: "msg_ses_busy_021", inclusive: true },
    })
    const calls = f.server.calls.filter((x) => x.method === "session.messages" && x.args.sessionID === "ses_acked")
    expect(calls.map((x) => [x.args.limit, x.args.before ?? null])).toEqual([[10, null], [10, "msg_ses_acked_016"]])
  })

  test("pages stay under 256 KiB; a message is split in part order and an oversized part is a stub", async () => {
    const big = (/** @type {string} */ c) => c.repeat(60000)
    const { ccd } = await setup({
      api: (f) => {
        f.server.sessions = [session("ses_p", Date.now() + 60000)]
        const m = msg("ses_p", 1)
        m.parts = Array.from({ length: 8 }, (_, i) => ({ id: "prt_" + i, sessionID: "ses_p", messageID: m.info.id, type: "text", text: big(String(i)) }))
        m.parts.push({ id: "prt_huge", sessionID: "ses_p", messageID: m.info.id, type: "tool", callID: "call_h", state: { status: "completed", input: Object.fromEntries(Array.from({ length: 6 }, (_, k) => ["f" + k, big("z")])) } })
        f.server.messages.set("ses_p", [m])
        f.server.status = { ses_p: { type: "busy" } }
      },
    })
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    const pages = c.lines.filter((/** @type {string} */ l) => l.startsWith('{"t":"sync_page"'))
    expect(pages.length).toBeGreaterThan(1)
    for (const l of pages) expect(Buffer.byteLength(l)).toBeLessThanOrEqual(262144)
    const all = items(c, 1)
    const split = all.filter((/** @type {any} */ i) => i.split)
    expect(split.flatMap((/** @type {any} */ i) => i.parts ?? [i.part]).map((/** @type {any} */ p) => p.id)).toEqual(Array.from({ length: 8 }, (_, i) => "prt_" + i))
    const stub = all.find((/** @type {any} */ i) => i.stub)
    expect(stub).toMatchObject({ stub: true, kind: "part", ids: { sessionID: "ses_p", messageID: "msg_ses_p_001", partID: "prt_huge", callID: "call_h" } })
    expect(stub.size).toBeGreaterThan(262144)
  })

  test("live frames are held during a sync and flushed after sync_end, then settled", async () => {
    const { ccd } = await setup({
      api: (f) => {
        f.server.sessions = [session("ses_h", Date.now() + 60000)]
        f.server.messages.set("ses_h", [msg("ses_h", 1, "user")])
        f.server.status = { ses_h: { type: "busy" } }
        f.server.before["session.messages"] = () => {
          f.emit("session.status", { sessionID: "ses_h", status: { type: "busy" } })
          f.emit("message.updated", { sessionID: "ses_h", info: { id: "msg_ses_h_002", sessionID: "ses_h", role: "assistant", time: { created: 2 } } })
        }
      },
    })
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    expect(kinds(c)).toEqual(["opencode_hello", "head", "sync_begin", "sync_page", "sync_end", "ev", "ev", "settled"])
    const begin = c.frames[2]
    const end = c.frames[4]
    const held = c.frames.slice(5, 7)
    for (const h of held) expect(h.seq).toBeGreaterThan(begin.as_of)
    expect(held.map((/** @type {any} */ h) => h.seq)).toEqual([held[0].seq, held[0].seq + 1])
    expect(end.done_at).toBeGreaterThanOrEqual(held[1].seq)
  })

  test("triggers during a sync coalesce into one follow-up; a resync request is a full sync", async () => {
    const { ccd, f } = await setup({ api: (f) => (f.server.delay = 40) })
    const c = await until(() => ccd.last()?.frames.some((/** @type {any} */ x) => x.t === "sync_begin") && ccd.last())
    f.emit("session.idle", { sessionID: "ses_x" })
    f.emit("session.error", { sessionID: "ses_x", error: { name: "UnknownError" } })
    f.emit("session.status", { sessionID: "ses_x", status: { type: "retry", attempt: 1 } })
    await until(() => settled(c, 2), 3000)
    await sleep(300)
    const begins = c.frames.filter((/** @type {any} */ x) => x.t === "sync_begin")
    expect(begins.map((/** @type {any} */ b) => [b.sync, b.scope, b.reason])).toEqual([[1, "full", "connect"], [2, "requests", "trigger:session.idle"]])
    c.socket.write(JSON.stringify({ type: "opencode_resync", acked: { ses_x: { from: "msg_1", inclusive: true } } }) + "\n")
    await until(() => settled(c, 3), 3000)
    expect(c.frames.filter((/** @type {any} */ x) => x.t === "sync_begin").at(-1)).toMatchObject({ sync: 3, scope: "full", reason: "requested" })
  })

  test("full snapshots start at least a second apart", async () => {
    const { ccd } = await setup()
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    for (let i = 0; i < 3; i++) c.socket.write(JSON.stringify({ type: "opencode_resync", acked: {} }) + "\n")
    await until(() => settled(c, 2), 3000)
    await sleep(1300)
    const begins = c.wire.filter((/** @type {any} */ w) => w.line.startsWith('{"t":"sync_begin"'))
    expect(begins.length).toBe(2)
    expect(c.at2 - c.at1).toBeGreaterThanOrEqual(950)
  })

  test("a failed session list is reported, and a malformed acked entry is dropped", async () => {
    const { ccd, f } = await setup({
      onHello: (_h, c) => welcome(c, { ses_ok: { from: null, inclusive: false }, ses_bad: { from: 5, inclusive: false }, nope: { from: null, inclusive: true }, ses_x: null }),
      api: (f) => f.server.failing.add("session.list"),
    })
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_end")).toMatchObject({ list_ok: false, requests_ok: true, lower: {}, pages: 0 })
    f.server.failing.delete("session.list")
    f.server.sessions = [session("ses_ok", 1), session("ses_bad", 1)]
    f.server.messages.set("ses_ok", [msg("ses_ok", 1, "user")])
    f.server.messages.set("ses_bad", [msg("ses_bad", 1, "user")])
    c.socket.write(JSON.stringify({ type: "opencode_resync", acked: { ses_ok: { from: null, inclusive: false }, ses_bad: { from: 5, inclusive: false } } }) + "\n")
    await until(() => settled(c, 2), 3000)
    expect(c.frames.filter((/** @type {any} */ x) => x.t === "sync_end").at(-1).lower).toEqual({ ses_ok: { from: null, inclusive: false } })
  })
})

describe("requests", () => {
  test("a live permission carries its anchored input and a live verdict; a failed list falls back to the store", async () => {
    const T = Date.now() + 60000
    const { ccd, f } = await setup({
      onHello: () => {},
      api: (f) => {
        f.server.sessions = [session("ses_r", T)]
        const m = msg("ses_r", 2)
        m.info.time.completed = undefined
        m.parts = [{ id: "prt_t", sessionID: "ses_r", messageID: m.info.id, type: "tool", callID: "call_1", tool: "bash", state: { status: "running", input: { command: "ls" } } }]
        f.server.messages.set("ses_r", [msg("ses_r", 1, "user"), m])
        f.server.status = { ses_r: { type: "busy" } }
      },
    })
    const c = await until(() => ccd.last()?.hello && ccd.last())
    const perm = { id: "per_1", sessionID: "ses_r", permission: "bash", patterns: ["ls"], metadata: { command: "ls" }, always: ["ls *"], tool: { messageID: "msg_ses_r_002", callID: "call_1" } }
    f.emit("message.part.updated", { sessionID: "ses_r", part: f.server.messages.get("ses_r")?.[1].parts[0] })
    f.emit("permission.asked", perm)
    f.server.permissions = [perm]
    welcome(c)
    await until(() => settled(c, 1))
    const r = c.frames.find((/** @type {any} */ x) => x.t === "sync_request")
    expect(r).toEqual({ t: "sync_request", sync: 1, kind: "permission", request: { ...perm, anchor_input: { command: "ls" } }, verdict: { live: true, dead: null, listed: true, in_store: false } })

    f.server.failing.add("permission.list")
    f.store.permissions = [perm]
    f.emit("session.idle", { sessionID: "ses_r" })
    await until(() => settled(c, 2))
    const r2 = c.frames.filter((/** @type {any} */ x) => x.t === "sync_request").at(-1)
    expect(r2.verdict).toEqual({ live: false, dead: "epoch:idle", listed: "unknown", in_store: true })
    expect(c.frames.find((/** @type {any} */ x) => x.t === "sync_end" && x.sync === 2).permissions_ok).toBe(false)
  })

  test("a live card is forwarded with its anchor; an oversized one is a card stub", async () => {
    const { ccd, f } = await setup()
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    f.emit("message.part.updated", { sessionID: "ses_c", part: { id: "prt_1", sessionID: "ses_c", type: "tool", callID: "call_9", state: { status: "pending", input: { command: "echo" } } } })
    f.emit("permission.asked", { id: "per_9", sessionID: "ses_c", permission: "bash", tool: { messageID: "m", callID: "call_9" } })
    f.emit("question.asked", { id: "que_1", sessionID: "ses_c", questions: [{ question: "q".repeat(2000000) }], tool: { messageID: "m", callID: "call_x" } })
    await until(() => c.frames.some((/** @type {any} */ x) => x.t === "card_stub"))
    const card = c.frames.find((/** @type {any} */ x) => x.type === "permission.asked")
    expect(card.properties.anchor_input).toEqual({ command: "echo" })
    expect(c.frames.find((/** @type {any} */ x) => x.t === "card_stub")).toMatchObject({ type: "question.asked", properties: { id: "que_1", sessionID: "ses_c", tool: { messageID: "m", callID: "call_x" } } })
  })
})

describe("requests across links and activations", () => {
  const T = Date.now() + 60000
  const perm = { id: "per_e", sessionID: "ses_e", permission: "bash", patterns: ["ls"], metadata: { command: "ls" }, always: ["ls *"], tool: { messageID: "msg_ses_e_002", callID: "call_e" } }
  /** @param {ReturnType<typeof fakeApi>} f */
  const serve = (f) => {
    f.server.sessions = [session("ses_e", T)]
    const m = msg("ses_e", 2)
    m.info.time.completed = undefined
    m.parts = [{ id: "prt_e", sessionID: "ses_e", messageID: m.info.id, type: "tool", callID: "call_e", tool: "bash", state: { status: "running", input: { command: "ls" } } }]
    f.server.messages.set("ses_e", [msg("ses_e", 1, "user"), m])
    f.server.status = { ses_e: { type: "busy" } }
    f.server.permissions = [perm]
  }
  /** @param {any} c @param {number} sync */
  const verdictOf = (c, sync) => c.frames.find((/** @type {any} */ x) => x.t === "sync_request" && x.sync === sync)?.verdict

  test("the pending list comes before any history page, and a card asked during a snapshot is not held", async () => {
    const { ccd, f } = await setup({ api: serve })
    f.emit("permission.asked", perm)
    let once = true
    f.server.before["session.messages"] = () => {
      if (once) f.emit("question.asked", { id: "que_mid", sessionID: "ses_e", questions: [{ question: "?" }] })
      once = false
    }
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    const k = kinds(c)
    expect(k.indexOf("sync_request")).toBeLessThan(k.indexOf("sync_page"))
    const card = c.frames.findIndex((/** @type {any} */ x) => x.type === "question.asked")
    expect(card).toBeGreaterThan(-1)
    expect(card).toBeLessThan(k.indexOf("sync_end"))
  })

  test("the epoch log outlives a reconnect and starts over at a new activation", async () => {
    const p = await loadPlugin()
    const ccd = fakeCcd(p.socket)
    const f1 = fakeApi()
    serve(f1)
    cleanups.push(p.cleanup, () => ccd.close(), () => f1.dispose())
    await p.mod.default.tui(/** @type {any} */ (f1.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
    f1.emit("message.part.updated", { sessionID: "ses_e", part: f1.server.messages.get("ses_e")?.[1].parts[0] })
    f1.emit("permission.asked", perm)
    const a = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    expect(a.hello.activation).toBe(1)
    expect(verdictOf(a, 1)).toEqual({ live: true, dead: null, listed: true, in_store: false })
    a.socket.end()
    const b = await until(() => ccd.conns.length === 2 && settled(ccd.last(), 2) && ccd.last(), 4000)
    expect(b.hello.activation).toBe(1)
    expect(verdictOf(b, 2)).toEqual({ live: true, dead: null, listed: true, in_store: false })

    f1.dispose()
    const f2 = fakeApi()
    serve(f2)
    cleanups.push(() => f2.dispose())
    await p.mod.default.tui(/** @type {any} */ (f2.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
    const c = await until(() => ccd.conns.length === 3 && settled(ccd.last(), 1) && ccd.last(), 4000)
    expect(c.hello.activation).toBe(2)
    expect(verdictOf(c, 1)).toEqual({ live: false, dead: "asked-before-activation", listed: true, in_store: false })
  })
})

describe("head", () => {
  test("pushes the route on change, with the session's folder, and again after a reconnect", async () => {
    const { ccd, f } = await setup()
    f.renderSlots()
    let c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    const heads = () => c.frames.filter((/** @type {any} */ x) => x.t === "head")
    await until(() => heads().length === 1)
    expect(heads()[0]).toMatchObject({ t: "head", route: "home" })
    f.navigate({ name: "session", params: { sessionID: "ses_h" } })
    await until(() => heads().length === 2)
    expect(heads()[1]).toMatchObject({ route: "session", session_id: "ses_h", directory: null })
    f.storeSession({ id: "ses_h", directory: "/Users/ada/elsewhere" })
    await until(() => heads().length === 3)
    expect(heads()[2]).toMatchObject({ route: "session", session_id: "ses_h", directory: "/Users/ada/elsewhere" })
    f.navigate({ name: "session", params: { sessionID: "ses_h" } })
    f.renderSlots()
    await sleep(50)
    expect(heads().length).toBe(3)
    f.navigate({ name: "plugin-route" })
    await until(() => heads().length === 4)
    expect(heads()[3]).toMatchObject({ route: "other" })
    expect(heads()[3].session_id).toBeUndefined()
    c.socket.end()
    c = await until(() => ccd.conns.length === 2 && ccd.last())
    await until(() => heads().length === 1)
    expect(c.frames[1]).toMatchObject({ t: "head", route: "other" })
  })
})

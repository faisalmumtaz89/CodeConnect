// The keyboard is never held up by the plugin: `tui()` resolves before any I/O, and no malformed event makes a
// listener throw or breaks the link.
import { afterEach, describe, expect, test } from "bun:test"
import { fakeApi } from "./fake-api.js"
import { fakeCcd, loadPlugin, settled, until } from "./helpers.js"

const NONCE = "0123456789abcdef0123456789abcdef"
/** @type {(() => Promise<void> | void)[]} */
const cleanups = []
afterEach(async () => {
  for (const f of cleanups.splice(0).reverse()) await f()
})

const hostile = new Proxy(
  {},
  {
    get() {
      throw new Error("getter")
    },
    has() {
      throw new Error("has")
    },
    ownKeys() {
      throw new Error("keys")
    },
  },
)

const MALFORMED = [
  undefined,
  null,
  {},
  { properties: null },
  { properties: hostile },
  hostile,
  { properties: { info: null, part: null, status: null, error: null } },
  { properties: { info: hostile, part: hostile } },
  { properties: { part: { type: "tool", state: null } } },
  { properties: { part: { type: "text", time: null } } },
  { properties: { info: { role: "user", time: hostile } } },
  { properties: { id: 5, sessionID: {}, tool: { callID: hostile } } },
]

describe("listeners", () => {
  test("tui() resolves before any connect, client call or slot", async () => {
    const p = await loadPlugin()
    const ccd = fakeCcd(p.socket)
    const f = fakeApi()
    cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())
    const t0 = performance.now()
    const r = p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
    expect(r).toBeInstanceOf(Promise)
    await r
    expect(performance.now() - t0).toBeLessThan(50)
    expect(ccd.conns.length).toBe(0)
    expect(f.server.calls.length).toBe(0)
    expect(f.slotCount()).toBe(0)
    expect(f.types().sort()).toEqual([...p.mod.SUBSCRIBED].sort())
    await until(() => ccd.last() && settled(ccd.last(), 1))
  })

  test("without a socket or nonce it registers nothing", async () => {
    const p = await loadPlugin()
    const f = fakeApi()
    cleanups.push(p.cleanup)
    await p.mod.default.tui(/** @type {any} */ (f.api), undefined, /** @type {any} */ ({}))
    await p.mod.default.tui(/** @type {any} */ (f.api), { socket: 3 }, /** @type {any} */ ({}))
    expect(f.handlerCount()).toBe(0)
  })

  test("no malformed event throws, and the link keeps forwarding", async () => {
    const p = await loadPlugin()
    const ccd = fakeCcd(p.socket)
    const f = fakeApi()
    cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())
    await p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
    const c = await until(() => ccd.last() && settled(ccd.last(), 1) && ccd.last())
    for (const type of p.mod.SUBSCRIBED) for (const e of MALFORMED) expect(() => f.emitRaw(type, e)).not.toThrow()
    f.emit("session.status", { sessionID: "ses_ok", status: { type: "busy" } })
    await until(() => c.frames.some((/** @type {any} */ x) => x.t === "ev" && x.properties?.sessionID === "ses_ok"))
    expect(c.closed).toBe(false)
  })

  test("a listener never waits on the socket: a stalled daemon costs nothing per event", async () => {
    const p = await loadPlugin()
    const ccd = fakeCcd(p.socket, { read: false })
    const f = fakeApi()
    cleanups.push(p.cleanup, () => ccd.close(), () => f.dispose())
    await p.mod.default.tui(/** @type {any} */ (f.api), { socket: p.socket, nonce: NONCE }, /** @type {any} */ ({}))
    const c = await until(() => ccd.last())
    c.send({ type: "opencode_welcome", link: "l", acked: {} })
    await new Promise((r) => setTimeout(r, 100))
    let worst = 0
    for (let i = 0; i < 2000; i++) {
      const t0 = performance.now()
      f.emit("message.part.updated", { sessionID: "ses_s", part: { id: "prt_" + i, sessionID: "ses_s", type: "text", text: "x".repeat(2000), time: { start: 1, end: 2 } } })
      worst = Math.max(worst, performance.now() - t0)
    }
    expect(worst).toBeLessThan(20)
  })
})

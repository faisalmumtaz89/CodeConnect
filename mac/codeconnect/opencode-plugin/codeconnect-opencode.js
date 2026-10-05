// CodeConnect's OpenCode TUI plugin (link wire version 1).
//
// It observes the TUI it runs in and forwards what it sees to the CodeConnect daemon over the daemon's unix socket:
// bus events (filtered and size-capped), the session the keyboard is showing, and on every connect a paged snapshot
// read from the OpenCode server, with the liveness verdict of every pending request. It executes nothing.
//
// The keyboard is never delayed by it: `tui()` returns at once, every listener body is synchronous, bounded and
// wrapped in try/catch, and all socket work (connect, hello, snapshot reads, writes) runs in the background. Writes
// are never awaited by a listener; a queue over its cap closes the link and the next connect resynchronises.
//
// Dependency-free on purpose: CodeConnect writes this one file into a private session directory and points
// OPENCODE_TUI_CONFIG at it. `solid-js` is the host's own copy, loaded on demand for the head effect.

import { createConnection } from "node:net"
import { readFileSync } from "node:fs"
import { createHash } from "node:crypto"
import { fileURLToPath } from "node:url"

/** @typedef {import("@opencode-ai/plugin/tui").TuiPluginApi} Api */
/** @typedef {import("@opencode-ai/plugin/tui").TuiPluginModule} TuiPluginModule */
/** @typedef {import("node:net").Socket} Socket */
/** @typedef {{ sec: number, usec: number }} Start */
/** @typedef {{ from: string | null, inclusive: boolean }} Bound */
/** @typedef {Record<string, Bound>} Acked */
/** @typedef {{ live: boolean, dead: string | null, listed: boolean | "unknown", in_store: boolean }} Verdict */
/** @typedef {"abort" | "idle" | "retry" | "deleted" | "disposed"} EpochEventType */
/** @typedef {{ seq: number, type: EpochEventType, sid?: string }} EpochEvent */
/** @typedef {{ line: string, bytes: number, stub: boolean }} Encoded */
/** @typedef {{ scope: "full" | "requests", reason: string }} Trigger */
/** @typedef {{ route: string, session_id?: string, directory?: string | null }} Head */

export const WIRE = 1
export const STRING_CAP = 65536
export const FRAME_CAP = 1048576
export const QUEUE_CAP = 4194304
export const PAGE_CAP = 262144
export const HELLO_TIMEOUT_MS = 3000
export const BACKOFF_MIN_MS = 500
export const BACKOFF_MAX_MS = 8000
const API_PAGE_LIMIT = 10
const READ_CAP = 1048576

/** The v1 bus events the daemon maps. Never `message.part.delta`, `session.next.*` or a `*.v2.*` event. */
export const SUBSCRIBED = Object.freeze([
  "session.created",
  "session.status",
  "session.idle",
  "session.error",
  "session.deleted",
  "server.instance.disposed",
  "message.updated",
  "message.removed",
  "message.part.updated",
  "message.part.removed",
  "permission.asked",
  "permission.replied",
  "question.asked",
  "question.replied",
  "question.rejected",
])
const SUBSCRIBED_SET = new Set(SUBSCRIBED)

/** Events after which the pending-request list is read again (no history pages). */
const REQUEST_TRIGGERS = new Set(["session.idle", "session.error", "session.deleted", "server.instance.disposed"])

/** Card fields: exempt from the string cap up to the frame cap, so the daemon classifies the whole text. */
const CARD_FIELDS = new Set(["metadata", "patterns", "always", "anchor_input", "questions"])

const MESSAGE_KEYS = ["id", "sessionID", "role", "parentID", "agent", "model", "modelID", "providerID", "error", "finish", "time"]

// ---------------------------------------------------------------------------------------------------------- caps

const utf8 = (/** @type {string} */ s) => Buffer.byteLength(s, "utf8")
/** @param {unknown} v */
export const encodedLength = (v) => utf8(JSON.stringify(v))
const sha256 = (/** @type {string} */ s) => createHash("sha256").update(s, "utf8").digest("hex")

/**
 * A string whose encoded JSON form is over `cap` bytes keeps its longest prefix (never splitting a surrogate pair)
 * such that prefix plus marker encode to at most `cap`. The marker is the daemon's own:
 * `… (N bytes elided; sha256 of the whole text H)`, N counted in UTF-8 bytes of the raw text.
 * @param {string} s
 * @param {number} [cap]
 */
export function cutString(s, cap = STRING_CAP) {
  if (s.length * 6 + 2 <= cap) return s
  if (s.length + 2 <= cap && encodedLength(s) <= cap) return s
  const total = utf8(s)
  const hash = sha256(s)
  const mark = (/** @type {number} */ kept) => `… (${total - kept} bytes elided; sha256 of the whole text ${hash})`
  const room = cap - encodedLength(mark(0))
  let lo = 0
  let hi = Math.min(s.length, cap)
  while (lo < hi) {
    const k = (lo + hi + 1) >> 1
    if (encodedLength(s.slice(0, k)) <= room) lo = k
    else hi = k - 1
  }
  let k = lo
  const c = s.charCodeAt(k - 1)
  if (k > 0 && c >= 0xd800 && c <= 0xdbff) k--
  const kept = s.slice(0, k)
  return kept + mark(utf8(kept))
}

const EVERYTHING = Symbol("exempt")

/**
 * Copy-on-write walk: removes patch bodies (`summary.diffs`, `filediff`, `revert.diff`, any string `patch`) and
 * cuts strings, except inside the top-level keys named by `exempt`. OpenCode's objects are never mutated.
 * @param {any} v
 * @param {Set<string>} [exempt]
 * @returns {any}
 */
export function prepare(v, exempt) {
  return walk(v, exempt, 0, "")
}

/**
 * @param {any} v
 * @param {Set<string> | typeof EVERYTHING | undefined} exempt
 * @param {number} depth
 * @param {string} key
 * @returns {any}
 */
function walk(v, exempt, depth, key) {
  if (depth > 64) return v
  if (typeof v === "string") return exempt === EVERYTHING ? v : cutString(v)
  if (Array.isArray(v)) {
    /** @type {any[] | null} */
    let o = null
    const sub = exempt === EVERYTHING ? EVERYTHING : undefined
    for (let i = 0; i < v.length; i++) {
      const x = walk(v[i], sub, depth + 1, key)
      if (x !== v[i]) (o ??= v.slice())[i] = x
    }
    return o ?? v
  }
  if (v && typeof v === "object") {
    /** @type {any} */
    let o = null
    for (const k of Object.keys(v)) {
      const drop =
        k === "filediff" || (k === "diffs" && key === "summary") || (k === "diff" && key === "revert") || (k === "patch" && typeof v[k] === "string")
      if (drop) {
        o ??= { ...v }
        delete o[k]
        continue
      }
      const sub = exempt === EVERYTHING ? EVERYTHING : depth === 0 && exempt instanceof Set && exempt.has(k) ? EVERYTHING : undefined
      const x = walk(v[k], sub, depth + 1, k)
      if (x !== v[k]) (o ??= { ...v })[k] = x
    }
    return o ?? v
  }
  return v
}

/**
 * The identifiers a stub keeps.
 * @param {any} p
 */
export function ids(p) {
  /** @type {Record<string, string>} */
  const o = {}
  const put = (/** @type {string} */ k, /** @type {unknown} */ v) => {
    if (typeof v === "string") o[k] = v
  }
  put("sessionID", p?.sessionID ?? p?.info?.sessionID ?? p?.part?.sessionID)
  put("id", p?.id)
  put("messageID", p?.info?.id ?? p?.part?.messageID ?? p?.messageID)
  put("partID", p?.part?.id ?? p?.partID)
  put("callID", p?.part?.callID ?? p?.tool?.callID)
  return o
}

/**
 * A live bus frame. Over the frame cap it becomes a stub whose size and sha256 are those of the full encoded line.
 * @param {number} seq
 * @param {string} type
 * @param {any} properties
 * @returns {Encoded}
 */
export function eventLine(seq, type, properties) {
  const line = JSON.stringify({ t: "ev", seq, type, properties: prepare(properties) }) + "\n"
  const bytes = utf8(line)
  if (bytes <= FRAME_CAP) return { line, bytes, stub: false }
  const s = JSON.stringify({ t: "stub", seq, type, ids: ids(properties), size: bytes, sha256: sha256(line) }) + "\n"
  return { line: s, bytes: utf8(s), stub: true }
}

/**
 * A permission or question card. Card fields are not cut; over the frame cap the card becomes a `card_stub` that
 * carries no request text.
 * @param {number} seq
 * @param {string} type
 * @param {any} card
 * @returns {Encoded}
 */
export function cardLine(seq, type, card) {
  const line = JSON.stringify({ t: "ev", seq, type, properties: prepare(card, CARD_FIELDS) }) + "\n"
  const bytes = utf8(line)
  if (bytes <= FRAME_CAP) return { line, bytes, stub: false }
  const s = JSON.stringify({ t: "card_stub", seq, type, properties: cardIds(card), size: bytes, sha256: sha256(line) }) + "\n"
  return { line: s, bytes: utf8(s), stub: true }
}

/** @param {any} card */
const cardIds = (card) => ({ id: card?.id, sessionID: card?.sessionID, permission: card?.permission, tool: card?.tool })

// -------------------------------------------------------------------------------------------------------- filter

/** @param {any} pt */
const isUserText = (pt) => pt.type === "text" && !("time" in pt)

/**
 * What is forwarded of each bus event: only state transitions. Its maps lose each entry at that entry's terminal
 * (user messages excepted: OpenCode re-emits their infos unchanged, so one entry per prompt is kept).
 */
export class Filter {
  constructor() {
    /** @type {Map<string, string>} */
    this.toolSent = new Map()
    /** @type {Set<string>} */
    this.started = new Set()
    /** @type {Map<string, string>} */
    this.messageSent = new Map()
  }

  /**
   * @param {string} type
   * @param {any} p
   * @returns {any} the properties to forward, or null
   */
  apply(type, p) {
    if (!SUBSCRIBED_SET.has(type)) return null
    if (type === "message.updated") {
      const i = p.info
      /** @type {Record<string, unknown>} */
      const keep = {}
      for (const k of MESSAGE_KEYS) if (k in i) keep[k] = i[k]
      if (typeof i.summary === "boolean") keep.summary = i.summary
      const completed = Boolean(i.time?.completed)
      const sig = JSON.stringify([i.role, i.parentID ?? null, keep.summary ?? null, completed, i.error?.name ?? null])
      if (this.messageSent.get(i.id) === sig) return null
      if (completed || i.role === "user") {
        this.messageSent.delete(i.id)
        if (i.role === "user") this.messageSent.set(i.id, sig)
      } else this.messageSent.set(i.id, sig)
      return { sessionID: p.sessionID ?? null, info: keep }
    }
    if (type === "message.part.updated") {
      let pt = p.part
      const ty = pt.type
      if (ty === "step-start") return null
      if (ty === "text" && (pt.synthetic || pt.ignored)) return null
      if ((ty === "text" || ty === "reasoning") && !isUserText(pt) && (pt.time ?? {}).end == null) {
        if (this.started.has(pt.id)) return null
        this.started.add(pt.id)
        return { sessionID: p.sessionID ?? null, part: { ...pt, text: "" } }
      }
      if (ty === "text" || ty === "reasoning") this.started.delete(pt.id)
      if (ty === "tool") {
        const status = pt.state.status
        if (status === "pending") return null
        if (status === "running") {
          if (this.toolSent.get(pt.id) === "running") return null
          this.toolSent.set(pt.id, "running")
          const { metadata, ...state } = pt.state
          pt = { ...pt, state }
        } else this.toolSent.delete(pt.id)
      }
      return { sessionID: p.sessionID ?? null, part: pt }
    }
    return p
  }
}

// ------------------------------------------------------------------------------------------------ epoch / liveness

/**
 * Per-activation record of requests asked and of the events that end a request's epoch (abort, idle, retry,
 * delete of its session or an ancestor; instance disposal). Created inside `tui()`, so a re-activation starts
 * empty and judges every older request dead. Events older than the oldest open request are pruned.
 */
export class EpochLog {
  constructor() {
    /** @type {Map<string, { seq: number, sid: string }>} */
    this.asked = new Map()
    /** @type {EpochEvent[]} */
    this.events = []
    /** @type {Map<string, string | undefined>} */
    this.parent = new Map()
  }

  /**
   * @param {number} seq
   * @param {string} type
   * @param {any} p
   */
  note(seq, type, p) {
    switch (type) {
      case "permission.asked":
      case "question.asked":
        if (typeof p.id === "string") this.asked.set(p.id, { seq, sid: p.sessionID })
        return
      case "permission.replied":
      case "question.replied":
      case "question.rejected":
        this.asked.delete(p.requestID)
        return this.prune()
      case "session.created":
        if (p.info?.id) this.parent.set(p.info.id, p.info.parentID)
        return
      case "session.status":
        if (p.status?.type === "idle" || p.status?.type === "retry") this.events.push({ seq, type: p.status.type, sid: p.sessionID })
        return this.prune()
      case "session.idle":
        this.events.push({ seq, type: "idle", sid: p.sessionID })
        return this.prune()
      case "session.error":
        if (p.error?.name === "MessageAbortedError") this.events.push({ seq, type: "abort", sid: p.sessionID })
        return this.prune()
      case "session.deleted": {
        const sid = p.info?.id ?? p.sessionID
        this.events.push({ seq, type: "deleted", sid })
        return this.prune()
      }
      case "server.instance.disposed":
        this.events.push({ seq, type: "disposed" })
        return this.prune()
    }
  }

  prune() {
    let oldest = Infinity
    for (const a of this.asked.values()) oldest = Math.min(oldest, a.seq)
    if (oldest === Infinity) this.events.length = 0
    else if (this.events.length && this.events[0].seq < oldest) this.events = this.events.filter((e) => e.seq >= oldest)
  }

  /** @param {string} sid */
  ancestors(sid) {
    const chain = []
    let p = this.parent.get(sid)
    while (p && chain.length < 8) {
      chain.push(p)
      p = this.parent.get(p)
    }
    return chain
  }

  /**
   * Why the request's epoch has ended, or null while it has not. "asked-before-activation" when this activation
   * never saw it asked.
   * @param {string} id
   * @param {string} sid
   * @param {string[]} ancestors
   * @returns {string | null}
   */
  ended(id, sid, ancestors) {
    const a = this.asked.get(id)
    if (!a) return "asked-before-activation"
    for (const e of this.events) {
      if (e.seq < a.seq) continue
      if (e.type === "disposed") return "epoch:disposed"
      if (e.sid === sid) return "epoch:" + e.type
      if (e.sid && ancestors.includes(e.sid)) return "epoch:" + e.type + ":ancestor"
    }
    return null
  }
}

/**
 * The liveness rule over gathered facts: PRESENT (listed, or in the TUI store when the list call failed), SERVER (the
 * session exists and is busy or retrying), ANCHOR (its own tool part is running in an unfinished, error-free
 * message; a doom-loop request: a matching running part in the newest assistant message; any other request
 * without a tool is never live), EPOCH (see EpochLog). The first failing condition names the dead reason.
 * @param {{ listed: boolean | "unknown", inStore: boolean, sessionExists: boolean, busy: boolean, anchor: boolean, epoch: string | null }} f
 * @returns {Verdict}
 */
export function verdict(f) {
  const present = f.listed === true || (f.listed === "unknown" && f.inStore)
  const dead = !present ? "absent" : !f.sessionExists ? "session-missing" : !f.busy ? "not-busy" : !f.anchor ? "anchor" : f.epoch
  return { live: dead === null, dead, listed: f.listed, in_store: f.inStore }
}

// ------------------------------------------------------------------------------------------------------ backoff

/**
 * Reconnect delay: 0.5 s doubling to 8 s, ±20% jitter.
 * @param {number} attempt 0 for the first retry
 * @param {number} [rand] in [0, 1)
 */
export function backoffDelay(attempt, rand = Math.random()) {
  const base = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** Math.min(attempt, 16))
  return Math.round(base * (0.8 + 0.4 * rand))
}

// --------------------------------------------------------------------------------------------------- API check

/**
 * Every member this plugin uses, checked at load. A missing member is named in the hello and leaves the link
 * observe-only.
 * @param {any} api
 * @returns {{ ok: boolean, missing: string[], version: string | null }}
 */
export function checkApi(api) {
  /** @type {string[]} */
  const missing = []
  /** @param {string} path @param {"function" | "object" | "string" | "getter"} kind */
  const need = (path, kind) => {
    try {
      let v = api
      for (const k of path.split(".").slice(1)) v = v?.[k]
      const ok = kind === "getter" ? v !== undefined : kind === "object" ? v !== null && typeof v === "object" : typeof v === kind
      if (!ok) missing.push(path)
    } catch {
      missing.push(path)
    }
  }
  need("api.event.on", "function")
  need("api.slots.register", "function")
  need("api.route.current", "getter")
  need("api.lifecycle.onDispose", "function")
  need("api.state.path.directory", "string")
  need("api.state.session.get", "function")
  need("api.state.session.permission", "function")
  need("api.state.session.question", "function")
  need("api.client.session.status", "function")
  need("api.client.session.list", "function")
  need("api.client.session.messages", "function")
  need("api.client.session.get", "function")
  need("api.client.session.message", "function")
  need("api.client.permission.list", "function")
  need("api.client.question.list", "function")
  need("api.app.version", "string")
  /** @type {string | null} */
  let version = null
  try {
    version = typeof api?.app?.version === "string" ? api.app.version : null
  } catch {}
  return { ok: missing.length === 0, missing, version }
}

// ------------------------------------------------------------------------------------------------------ plugin

let activations = 0

/** @param {unknown} v @returns {v is Start} */
const isStart = (v) =>
  !!v && typeof v === "object" && Number.isInteger(/** @type {any} */ (v).sec) && Number.isInteger(/** @type {any} */ (v).usec)

/**
 * `agent.json` next to this file: the agent's pid and birth time, written by CodeConnect after it started OpenCode.
 * @returns {Start | null}
 */
function readStart() {
  try {
    const v = JSON.parse(readFileSync(fileURLToPath(new URL("./agent.json", import.meta.url)), "utf8"))
    return isStart(v?.start) ? { sec: v.start.sec, usec: v.start.usec } : null
  } catch {
    return null
  }
}

/** @param {any} r */
const ok = (r) => !!r && !r.error && (r.response?.status ?? 200) < 400

/**
 * @param {Api} api
 * @param {any} options
 */
async function tui(api, options) {
  const socketPath = options?.socket
  const nonce = options?.nonce
  if (typeof socketPath !== "string" || typeof nonce !== "string") return
  activate(api, socketPath, nonce)
}

/**
 * @param {Api} api
 * @param {string} socketPath
 * @param {string} nonce
 */
function activate(api, socketPath, nonce) {
  const activation = ++activations
  const T0 = Date.now()
  const check = checkApi(api)
  /** @type {any} */
  const client = api.client
  const directory = () => {
    try {
      return api.state.path.directory
    } catch {
      return undefined
    }
  }

  const filter = new Filter()
  const epoch = new EpochLog()
  /** @type {Map<string, any>} callID -> the running tool part's input */
  const inputs = new Map()
  /** @type {Map<string, string>} root session -> first user message seen in this activation */
  const firstPrompt = new Map()
  let seq = 0
  let syncs = 0

  // link state
  /** @type {Socket | null} */
  let sock = null
  /** @type {"down" | "hello" | "live"} */
  let state = "down"
  let syncing = false
  /** @type {{ line: string, bytes: number, seq: number }[]} */
  let held = []
  let heldBytes = 0
  /** @type {Trigger | null} */
  let pending = null
  /** @type {Acked} */
  let acked = {}
  let attempt = 0
  let stopped = false
  let disposed = false
  /** @type {ReturnType<typeof setTimeout> | null} */
  let retryTimer = null
  /** @type {Head | null} */
  let head = null
  let headSent = ""
  /** @type {(() => void) | null} */
  let disposeRoot = null
  /** @type {(() => void)[]} */
  const unsubscribe = []

  const rootOf = (/** @type {string} */ s) => {
    let x = s
    for (let k = 0; k < 50 && epoch.parent.get(x); k++) x = /** @type {string} */ (epoch.parent.get(x))
    return x
  }

  // ---- listeners: synchronous and bounded; nothing here waits on I/O
  /**
   * @param {string} type
   * @param {any} p
   */
  function track(type, p) {
    epoch.note(seq, type, p)
    if (type === "message.part.updated" && p.part?.type === "tool" && p.part.callID) {
      const s = p.part.state?.status
      if (s === "pending" || s === "running") inputs.set(p.part.callID, p.part.state?.input)
      else inputs.delete(p.part.callID)
    }
    if (type === "message.updated") {
      const i = p.info
      if (i?.role === "user" && i.sessionID && !epoch.parent.get(i.sessionID) && !firstPrompt.has(i.sessionID) && (i.time?.created ?? 0) >= T0)
        firstPrompt.set(i.sessionID, i.id)
    }
  }

  /**
   * @param {string} type
   * @param {any} event
   */
  function forward(type, event) {
    const p = event?.properties ?? {}
    seq++
    track(type, p)
    if (REQUEST_TRIGGERS.has(type) || (type === "session.status" && p.status?.type === "retry")) trigger({ scope: "requests", reason: "trigger:" + type })
    const out = filter.apply(type, p)
    if (out === null || state !== "live") return
    if (type === "permission.asked" || type === "question.asked") enqueue(cardLine(seq, type, withAnchor(out)), seq)
    else enqueue(eventLine(seq, type, out), seq)
  }

  /** @param {any} request */
  const withAnchor = (request) => {
    const callID = request?.tool?.callID
    return callID && inputs.has(callID) ? { ...request, anchor_input: inputs.get(callID) } : request
  }

  // ---- writes
  /**
   * @param {Encoded} e
   * @param {number} s
   */
  function enqueue(e, s) {
    const c = sock
    if (!c || state !== "live") return
    if (syncing) {
      if (c.writableLength + heldBytes + e.bytes > QUEUE_CAP) return overflow(c)
      held.push({ line: e.line, bytes: e.bytes, seq: s })
      heldBytes += e.bytes
      return
    }
    if (c.writableLength + e.bytes > QUEUE_CAP) return overflow(c)
    c.write(e.line)
  }

  /** @param {Socket} c */
  function overflow(c) {
    if (sock !== c) return
    c.destroy()
  }

  /**
   * @param {Socket} c
   * @param {unknown} frame
   */
  const send = (c, frame) => {
    const line = JSON.stringify(frame) + "\n"
    c.write(line)
    return utf8(line)
  }

  /** @param {Socket} c */
  const drained = (c) =>
    new Promise((resolve) => {
      if (c.writableLength === 0 || c.destroyed) return resolve(undefined)
      const done = () => {
        c.off("drain", done)
        c.off("close", done)
        resolve(undefined)
      }
      c.on("drain", done)
      c.on("close", done)
    })

  // ---- head
  /** @param {Head} h */
  function setHead(h) {
    head = h
    sendHead()
  }

  function sendHead() {
    if (!head || state !== "live") return
    const key = JSON.stringify(head)
    if (key === headSent) return
    headSent = key
    seq++
    const line = JSON.stringify({ t: "head", seq, ...head }) + "\n"
    enqueue({ line, bytes: utf8(line), stub: false }, seq)
  }

  /** @returns {Head} */
  function readHead() {
    /** @type {any} */
    const r = api.route.current
    if (r?.name === "session" && typeof r.params?.sessionID === "string") {
      const id = r.params.sessionID
      const dir = api.state.session.get(id)?.directory
      return { route: "session", session_id: id, directory: typeof dir === "string" ? dir : null }
    }
    return { route: r?.name === "home" ? "home" : "other" }
  }

  /** @param {any} solid */
  function registerHead(solid) {
    let started = false
    api.slots.register({
      order: 0,
      slots: {
        app() {
          try {
            if (started || disposed) return null
            started = true
            solid.createRoot((/** @type {() => void} */ dispose) => {
              disposeRoot = dispose
              solid.createEffect(() => {
                try {
                  setHead(readHead())
                } catch {}
              })
            })
          } catch {}
          return null
        },
      },
    })
  }

  // ---- sync
  /** @param {Trigger} t */
  function trigger(t) {
    if (state !== "live") return
    if (syncing) {
      if (!pending || (pending.scope === "requests" && t.scope === "full")) pending = t
      return
    }
    if (pending) {
      if (pending.scope === "requests" && t.scope === "full") pending = t
      return
    }
    pending = t
    setTimeout(runPending, 0)
  }

  function runPending() {
    const c = sock
    const t = pending
    pending = null
    if (!t || !c || state !== "live" || syncing) return
    void runSync(c, t).catch(() => {
      if (sock === c) c.destroy()
    })
  }

  /**
   * @param {Socket} c
   * @param {Trigger} t
   */
  async function runSync(c, t) {
    const sync = ++syncs
    const asOf = seq
    syncing = true
    held = []
    heldBytes = 0
    const alive = () => sock === c && state === "live" && !disposed
    const doneAt = t.scope === "full" ? await fullSync(c, sync, t.reason, asOf, alive) : await requestsSync(c, sync, t.reason, asOf, alive)
    if (doneAt === null || !alive()) return
    let settled = false
    for (const h of held) {
      if (!settled && h.seq > doneAt) {
        send(c, { t: "settled", sync })
        settled = true
      }
      c.write(h.line)
    }
    if (!settled) send(c, { t: "settled", sync })
    held = []
    heldBytes = 0
    syncing = false
    if (pending) setTimeout(runPending, 0)
  }

  /** @param {() => Promise<any>} f */
  const call = async (f) => {
    try {
      const r = await f()
      return { ok: ok(r), data: r?.data, response: r?.response }
    } catch {
      return { ok: false, data: undefined, response: undefined }
    }
  }

  /**
   * @param {Socket} c
   * @param {number} sync
   * @param {string} reason
   * @param {number} asOf
   * @param {() => boolean} alive
   * @returns {Promise<number | null>} done_at, or null when the link went away
   */
  async function requestsSync(c, sync, reason, asOf, alive) {
    const reads = await readRequests()
    if (!alive()) return null
    send(c, { t: "sync_begin", sync, reason, scope: "requests", as_of: asOf, status: reads.status.data ?? {}, status_ok: reads.status.ok })
    const n = await sendRequests(c, sync, reads, alive)
    if (n === null) return null
    const doneAt = seq
    send(c, {
      t: "sync_end",
      sync,
      done_at: doneAt,
      permissions_ok: reads.permissions.ok,
      questions_ok: reads.questions.ok,
      lower: {},
      pages: 0,
      items: 0,
      bytes: n,
      activation,
    })
    return doneAt
  }

  const readRequests = async () => {
    const dir = directory()
    const [status, permissions, questions] = await Promise.all([
      call(() => client.session.status({ directory: dir })),
      call(() => client.permission.list({ directory: dir })),
      call(() => client.question.list({ directory: dir })),
    ])
    return { status, permissions, questions }
  }

  /**
   * @param {Socket} c
   * @param {number} sync
   * @param {Awaited<ReturnType<typeof readRequests>>} reads
   * @param {() => boolean} alive
   * @returns {Promise<number | null>} bytes sent
   */
  async function sendRequests(c, sync, reads, alive) {
    /** @type {Record<string, any>} */
    const status = reads.status.ok ? reads.status.data ?? {} : {}
    /** @type {{ kind: "permission" | "question", request: any, listed: boolean | "unknown" }[]} */
    const requests = []
    const fromStore = (/** @type {"permission" | "question"} */ kind) => {
      const seen = new Set()
      const sids = new Set([...epoch.parent.keys(), ...[...epoch.asked.values()].map((a) => a.sid), ...(head?.session_id ? [head.session_id] : [])])
      for (const s of sids) {
        try {
          for (const r of api.state.session[kind](s) ?? []) if (!seen.has(r.id)) (seen.add(r.id), requests.push({ kind, request: r, listed: "unknown" }))
        } catch {}
      }
    }
    if (reads.permissions.ok) for (const r of reads.permissions.data ?? []) requests.push({ kind: "permission", request: r, listed: true })
    else fromStore("permission")
    if (reads.questions.ok) for (const r of reads.questions.data ?? []) requests.push({ kind: "question", request: r, listed: true })
    else fromStore("question")
    let bytes = 0
    const parents = new Map()
    for (const x of requests) {
      const v = await judge(x.kind, x.request, x.listed, status, parents)
      if (!alive()) return null
      const request = withAnchor(x.request)
      const frame = { t: "sync_request", sync, kind: x.kind, request: prepare(request, CARD_FIELDS), verdict: v }
      let line = JSON.stringify(frame) + "\n"
      const nb = utf8(line)
      if (nb > FRAME_CAP) line = JSON.stringify({ t: "sync_request", sync, kind: x.kind, stub: { properties: cardIds(request), size: nb, sha256: sha256(line) }, verdict: v }) + "\n"
      bytes += utf8(line)
      c.write(line)
      if (c.writableLength > 0) await drained(c)
      if (!alive()) return null
    }
    return bytes
  }

  /**
   * @param {"permission" | "question"} kind
   * @param {any} r
   * @param {boolean | "unknown"} listed
   * @param {Record<string, any>} status
   * @param {Map<string, string | null>} parents
   * @returns {Promise<Verdict>}
   */
  async function judge(kind, r, listed, status, parents) {
    const dir = directory()
    const sid = r.sessionID
    let inStore = false
    try {
      inStore = (api.state.session[kind](sid) ?? []).some((/** @type {any} */ x) => x.id === r.id)
    } catch {}
    const s = status[sid]
    const busy = !!s && s.type !== "idle"
    const got = await call(() => client.session.get({ sessionID: sid, directory: dir }))
    let anchor = false
    const tool = r.tool
    if (tool?.messageID) {
      const m = await call(() => client.session.message({ sessionID: sid, messageID: tool.messageID, directory: dir }))
      const part = (m.data?.parts ?? []).find((/** @type {any} */ p) => p.type === "tool" && p.callID === tool.callID)
      anchor = m.ok && part?.state?.status === "running" && !m.data?.info?.error && !m.data?.info?.time?.completed
    } else if (kind === "permission" && r.permission === "doom_loop") {
      const meta = r.metadata ?? {}
      const ms = await call(() => client.session.messages({ sessionID: sid, directory: dir, limit: 4 }))
      const last = (ms.data ?? []).filter((/** @type {any} */ x) => x.info.role === "assistant").at(-1)
      const running = (last?.parts ?? []).some(
        (/** @type {any} */ p) =>
          p.type === "tool" && p.tool === meta.tool && p.state?.status === "running" && JSON.stringify(p.state?.input) === JSON.stringify(meta.input),
      )
      anchor = ms.ok && running && !last?.info?.error && !last?.info?.time?.completed
    }
    const chain = new Set(epoch.ancestors(sid))
    let cur = sid
    for (let i = 0; i < 8; i++) {
      let p = parents.get(cur)
      if (p === undefined) {
        const g = cur === sid ? got : await call(() => client.session.get({ sessionID: cur, directory: dir }))
        if (!g.ok) break
        const parent = typeof g.data?.parentID === "string" ? g.data.parentID : null
        parents.set(cur, parent)
        p = parent
      }
      if (!p) break
      chain.add(p)
      cur = p
    }
    return verdict({ listed, inStore, sessionExists: got.ok, busy, anchor, epoch: epoch.ended(r.id, sid, [...chain]) })
  }

  /**
   * @param {Socket} c
   * @param {number} sync
   * @param {string} reason
   * @param {number} asOf
   * @param {() => boolean} alive
   * @returns {Promise<number | null>}
   */
  async function fullSync(c, sync, reason, asOf, alive) {
    const dir = directory()
    const status = await call(() => client.session.status({ directory: dir }))
    if (!alive()) return null
    send(c, { t: "sync_begin", sync, reason, scope: "full", as_of: asOf, status: status.data ?? {}, status_ok: status.ok })
    const list = await call(() => client.session.list({ directory: dir }))
    if (!alive()) return null
    /** @type {any[]} */
    const infos = list.ok && Array.isArray(list.data) ? list.data : []
    for (const x of infos) if (x?.parentID && !epoch.parent.has(x.id)) epoch.parent.set(x.id, x.parentID)
    const busy = new Set(Object.entries(status.data ?? {}).filter(([, v]) => v?.type !== "idle").map(([k]) => k))
    const roots = infos.filter((x) => !x.parentID && (x.id in acked || busy.has(x.id) || firstPrompt.has(x.id)))

    /** @type {Record<string, Bound>} */
    const lower = {}
    /** @type {Record<string, "bound" | "start" | "last-user">} */
    const mode = {}
    for (const r of roots) {
      if (r.id in acked) {
        lower[r.id] = { from: acked[r.id].from, inclusive: !!acked[r.id].inclusive }
        mode[r.id] = acked[r.id].from === null ? "start" : "bound"
      } else if ((r.time?.created ?? 0) >= T0) {
        lower[r.id] = { from: null, inclusive: false }
        mode[r.id] = "start"
      } else if (firstPrompt.has(r.id)) {
        lower[r.id] = { from: /** @type {string} */ (firstPrompt.get(r.id)), inclusive: true }
        mode[r.id] = "bound"
      } else {
        lower[r.id] = { from: null, inclusive: false }
        mode[r.id] = "last-user"
      }
    }

    const ENVELOPE = encodedLength({ t: "sync_page", sync, n: 999999, items: [] }) + 1
    /** @type {any[]} */
    let page = []
    let pageBytes = 0
    let pages = 0
    let items = 0
    let bytes = 0
    const flush = async () => {
      if (!page.length) return
      const line = JSON.stringify({ t: "sync_page", sync, n: pages, items: page }) + "\n"
      page = []
      pageBytes = 0
      pages++
      bytes += utf8(line)
      c.write(line)
      if (c.writableLength > 0) await drained(c)
    }
    /** @param {any} item */
    const add = async (item) => {
      let nb = encodedLength(item) + 1
      if (ENVELOPE + nb > PAGE_CAP) {
        const s = JSON.stringify(item)
        const kind = item.part ? "part" : item.session ? "session" : "message"
        const keyed = item.part ? { sessionID: item.sessionID, part: item.part } : item.session ? { sessionID: item.session.id } : item
        item = { stub: true, kind, ids: ids(keyed), size: utf8(s), sha256: sha256(s) }
        nb = encodedLength(item) + 1
      }
      if (ENVELOPE + pageBytes + nb > PAGE_CAP) await flush()
      if (!alive()) return
      page.push(item)
      pageBytes += nb
      items++
    }
    /**
     * @param {string} root
     * @param {string} id
     * @param {string} role
     * @returns {"keep" | "last" | "stop"}
     */
    const keep = (root, id, role) => {
      const m = mode[root]
      if (m === "start") return "keep"
      if (m === "last-user") return role === "user" ? "last" : "keep"
      const b = lower[root]
      const from = /** @type {string} */ (b.from)
      return (b.inclusive ? id >= from : id > from) ? "keep" : "stop"
    }
    /**
     * @param {string} sid
     * @param {string} root
     * @param {boolean} all
     */
    const window = async (sid, root, all) => {
      /** @type {string | undefined} */
      let before
      let oldest = Infinity
      for (let n = 0; n < 100000 && alive(); n++) {
        const r = await call(() => client.session.messages({ sessionID: sid, directory: dir, limit: API_PAGE_LIMIT, ...(before ? { before } : {}) }))
        if (!alive()) return oldest
        if (!r.ok) {
          await add({ error: true, sessionID: sid })
          return oldest
        }
        /** @type {any[]} */
        const msgs = Array.isArray(r.data) ? r.data : []
        let reached = false
        for (let i = msgs.length - 1; i >= 0 && !reached; i--) {
          const m = msgs[i]
          const k = all ? "keep" : keep(root, m.info.id, m.info.role)
          if (k === "stop") {
            reached = true
            continue
          }
          if (k === "last") {
            reached = true
            lower[root] = { from: m.info.id, inclusive: true }
          }
          oldest = Math.min(oldest, m.info.time?.created ?? Infinity)
          await message(sid, m)
          if (!alive()) return oldest
        }
        const cursor = r.response?.headers?.get?.("x-next-cursor")
        if (reached || !cursor || !msgs.length) break
        before = cursor
      }
      return oldest
    }
    /**
     * A message as one item, or split across items in part order when it does not fit a page.
     * @param {string} sid
     * @param {any} m
     */
    const message = async (sid, m) => {
      const info = prepare(m.info)
      const parts = (m.parts ?? []).map((/** @type {any} */ p) => prepare(p))
      const whole = { sessionID: sid, info, parts }
      if (ENVELOPE + encodedLength(whole) + 1 <= PAGE_CAP) return add(whole)
      /** @type {any[]} */
      let chunk = []
      for (const p of parts) {
        if (chunk.length && ENVELOPE + encodedLength({ sessionID: sid, info, parts: [...chunk, p], split: true }) + 1 > PAGE_CAP) {
          await add({ sessionID: sid, info, parts: chunk, split: true })
          chunk = []
        }
        chunk.push(p)
        if (chunk.length === 1 && ENVELOPE + encodedLength({ sessionID: sid, info, parts: chunk, split: true }) + 1 > PAGE_CAP) {
          await add({ sessionID: sid, info, part: p, split: true })
          chunk = []
        }
      }
      if (chunk.length) await add({ sessionID: sid, info, parts: chunk, split: true })
    }

    const want = new Set(roots.map((r) => r.id))
    for (const r of roots) await add({ session: prepare(r) })
    /** @type {Record<string, number>} */
    const start = {}
    for (const r of roots) {
      start[r.id] = await window(r.id, r.id, false)
      if (!alive()) return null
    }
    for (const k of infos) {
      if (!k.parentID || !want.has(rootOf(k.id))) continue
      if ((k.time?.created ?? 0) < (start[rootOf(k.id)] ?? Infinity)) continue
      await add({ session: prepare(k) })
      await window(k.id, rootOf(k.id), true)
      if (!alive()) return null
    }
    await flush()
    if (!alive()) return null
    const reads = await readRequests()
    if (!alive()) return null
    const n = await sendRequests(c, sync, reads, alive)
    if (n === null) return null
    const doneAt = seq
    send(c, {
      t: "sync_end",
      sync,
      done_at: doneAt,
      permissions_ok: reads.permissions.ok,
      questions_ok: reads.questions.ok,
      lower,
      pages,
      items,
      bytes: bytes + n,
      activation,
    })
    return doneAt
  }

  // ---- connection
  function connect() {
    retryTimer = null
    if (disposed || stopped) return
    const start = readStart()
    if (!start) return retry()
    /** @type {Socket} */
    let c
    try {
      c = createConnection({ path: socketPath })
    } catch {
      return retry()
    }
    sock = c
    state = "hello"
    let buffer = ""
    const timer = setTimeout(() => {
      if (sock === c && state === "hello") c.destroy()
    }, HELLO_TIMEOUT_MS)
    c.on("connect", () => {
      if (sock !== c) return
      send(c, {
        type: "opencode_hello",
        wire: WIRE,
        nonce,
        pid: process.pid,
        start,
        activation,
        directory: directory() ?? null,
        api: { ok: check.ok, missing: check.missing, version: check.version },
      })
    })
    c.on("data", (d) => {
      if (sock !== c) return
      buffer += d.toString("utf8")
      if (buffer.length > READ_CAP) return void c.destroy()
      let i
      while ((i = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, i)
        buffer = buffer.slice(i + 1)
        try {
          receive(c, JSON.parse(line))
        } catch {}
        if (sock !== c) return
      }
    })
    c.on("error", () => {})
    c.on("end", () => c.destroy())
    c.on("close", () => {
      clearTimeout(timer)
      if (sock !== c) return
      sock = null
      state = "down"
      syncing = false
      held = []
      heldBytes = 0
      pending = null
      headSent = ""
      retry()
    })
  }

  function retry() {
    if (disposed || stopped || retryTimer) return
    retryTimer = setTimeout(connect, backoffDelay(attempt++))
  }

  /**
   * @param {Socket} c
   * @param {any} m
   */
  function receive(c, m) {
    if (state === "hello") {
      if (m?.type === "opencode_welcome") {
        attempt = 0
        acked = m.acked && typeof m.acked === "object" ? m.acked : {}
        state = "live"
        sendHead()
        trigger({ scope: "full", reason: "connect" })
      } else if (m?.type === "opencode_refused") {
        if (m.final === true) stopped = true
        c.destroy()
      }
      return
    }
    if (m?.type === "opencode_resync") {
      if (m.acked && typeof m.acked === "object") acked = m.acked
      trigger({ scope: "full", reason: "requested" })
    }
  }

  // ---- registration
  for (const type of SUBSCRIBED) {
    try {
      const off = api.event.on(/** @type {any} */ (type), (/** @type {any} */ event) => {
        try {
          forward(type, event)
        } catch {}
      })
      if (typeof off === "function") unsubscribe.push(off)
    } catch {}
  }
  try {
    api.lifecycle.onDispose(() => {
      disposed = true
      if (retryTimer) clearTimeout(retryTimer)
      retryTimer = null
      for (const off of unsubscribe.splice(0)) {
        try {
          off()
        } catch {}
      }
      try {
        disposeRoot?.()
      } catch {}
      sock?.destroy()
    })
  } catch {}

  setTimeout(() => {
    if (disposed) return
    import("solid-js")
      .then((solid) => {
        try {
          if (!disposed) registerHead(solid)
        } catch {}
      })
      .catch(() => {
        check.ok = false
        check.missing.push("solid-js")
      })
      .finally(() => {
        if (!disposed && !sock && !retryTimer) connect()
      })
  }, 0)
}

/** @type {TuiPluginModule & { id: string }} */
export default { id: "codeconnect", tui }

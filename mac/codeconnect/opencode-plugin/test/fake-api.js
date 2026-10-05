// A fake TuiPluginApi: an event bus, a route signal, a TUI store, slots, lifecycle and a client backed by an
// in-memory server (sessions, messages paged newest-first with x-next-cursor, status, permission and question
// lists). Hooks let a test act in the middle of a client call.
import { createSignal } from "solid-js/dist/solid.js"

/**
 * @param {{ version?: string, directory?: string }} [opts]
 */
export function fakeApi(opts = {}) {
  const handlers = new Map()
  const [route, setRoute] = createSignal(/** @type {any} */ ({ name: "home" }))
  const [storeTick, setStoreTick] = createSignal(0)
  const server = {
    /** @type {any[]} */ sessions: [],
    /** @type {Map<string, any[]>} */ messages: new Map(),
    /** @type {Record<string, any>} */ status: {},
    /** @type {any[]} */ permissions: [],
    /** @type {any[]} */ questions: [],
    /** @type {Set<string>} */ failing: new Set(),
    /** @type {{ method: string, args: any }[]} */ calls: [],
    /** @type {Record<string, (args: any) => void>} */ before: {},
    delay: 0,
  }
  const store = { /** @type {Map<string, any>} */ sessions: new Map(), /** @type {any[]} */ permissions: [], /** @type {any[]} */ questions: [] }
  const slots = /** @type {any[]} */ ([])
  const disposers = /** @type {(() => void)[]} */ ([])

  const respond = async (/** @type {string} */ method, /** @type {any} */ args, /** @type {() => any} */ body, headers = {}) => {
    server.calls.push({ method, args })
    await (server.delay ? new Promise((r) => setTimeout(r, server.delay)) : Promise.resolve())
    server.before[method]?.(args)
    if (server.failing.has(method)) return { error: { name: "Fail" }, response: new Response(null, { status: 500 }) }
    const data = body()
    if (data === undefined) return { error: { name: "NotFound" }, response: new Response(null, { status: 404 }) }
    return { data, response: new Response(null, { status: 200, headers }) }
  }

  const client = {
    session: {
      status: (/** @type {any} */ a) => respond("session.status", a, () => structuredClone(server.status)),
      list: (/** @type {any} */ a) => respond("session.list", a, () => structuredClone(server.sessions)),
      get: (/** @type {any} */ a) => respond("session.get", a, () => structuredClone(server.sessions.find((s) => s.id === a.sessionID))),
      message: (/** @type {any} */ a) =>
        respond("session.message", a, () => structuredClone((server.messages.get(a.sessionID) ?? []).find((m) => m.info.id === a.messageID))),
      messages: (/** @type {any} */ a) => {
        const all = server.messages.get(a.sessionID)
        if (!all) return respond("session.messages", a, () => undefined)
        const older = a.before ? all.filter((m) => m.info.id < a.before) : all
        if (!a.limit) return respond("session.messages", a, () => structuredClone(older))
        const pageItems = older.slice(Math.max(0, older.length - a.limit))
        const more = older.length > pageItems.length
        return respond("session.messages", a, () => structuredClone(pageItems), more ? { "x-next-cursor": pageItems[0].info.id } : {})
      },
    },
    permission: { list: (/** @type {any} */ a) => respond("permission.list", a, () => structuredClone(server.permissions)) },
    question: { list: (/** @type {any} */ a) => respond("question.list", a, () => structuredClone(server.questions)) },
  }

  const api = {
    app: { version: opts.version ?? "1.18.34" },
    event: {
      on(/** @type {string} */ type, /** @type {(e: any) => void} */ fn) {
        if (!handlers.has(type)) handlers.set(type, new Set())
        handlers.get(type).add(fn)
        return () => handlers.get(type).delete(fn)
      },
    },
    route: {
      get current() {
        return route()
      },
    },
    slots: {
      register(/** @type {any} */ plugin) {
        slots.push(plugin)
        plugin.slots?.app?.({}, {})
        return "slot-" + slots.length
      },
    },
    lifecycle: {
      onDispose(/** @type {() => void} */ fn) {
        disposers.push(fn)
        return () => {}
      },
    },
    state: {
      path: { directory: opts.directory ?? "/Users/ada/project" },
      session: {
        get: (/** @type {string} */ id) => (storeTick(), store.sessions.get(id)),
        permission: (/** @type {string} */ id) => store.permissions.filter((p) => p.sessionID === id),
        question: (/** @type {string} */ id) => store.questions.filter((q) => q.sessionID === id),
      },
    },
    client,
  }

  return {
    api,
    server,
    store,
    /** @param {string} type @param {any} properties */
    emit(type, properties) {
      for (const fn of handlers.get(type) ?? []) fn({ type, properties })
    },
    /** @param {string} type @param {any} event */
    emitRaw(type, event) {
      for (const fn of handlers.get(type) ?? []) fn(event)
    },
    handlerCount: () => [...handlers.values()].reduce((n, s) => n + s.size, 0),
    types: () => [...handlers.keys()].filter((k) => handlers.get(k).size),
    navigate: (/** @type {any} */ r) => setRoute(r),
    /** @param {any} s */
    storeSession(s) {
      store.sessions.set(s.id, s)
      setStoreTick((n) => n + 1)
    },
    renderSlots() {
      for (const p of slots) p.slots?.app?.({}, {})
    },
    slotCount: () => slots.length,
    dispose() {
      for (const fn of disposers.splice(0)) fn()
    },
  }
}

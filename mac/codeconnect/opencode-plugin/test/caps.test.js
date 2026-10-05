// Frame bounds as data (s13-frame-caps, s13-cutcheck-cards): the string cut and its marker, the frame stub, patch
// stripping, and approval cards forwarded whole up to the frame cap and stubbed above it.
import { describe, expect, test } from "bun:test"
import { createHash } from "node:crypto"
import { readFileSync } from "node:fs"
import { cardLine, cutString, encodedLength, eventLine, FRAME_CAP, prepare, STRING_CAP } from "../codeconnect-opencode.js"
import { fixture } from "./helpers.js"

const caps = JSON.parse(readFileSync(fixture("s13-frame-caps-1.18.34.json"), "utf8"))
const cards = JSON.parse(readFileSync(fixture("s13-cutcheck-cards-1.18.34.json"), "utf8"))
/** @param {any[]} r */
const build = (r) => r.map((p) => ("text" in p ? p.text : p.repeat.repeat(p.count))).join("")
const sha = (/** @type {string} */ s) => createHash("sha256").update(s, "utf8").digest("hex")

describe("caps", () => {
  test("the caps are the fixture's", () => {
    expect(STRING_CAP).toBe(caps.caps.string_bytes)
    expect(FRAME_CAP).toBe(caps.caps.frame_bytes)
  })

  for (const s of caps.strings) {
    test(`string: ${s.what}`, () => {
      const v = build(s.recipe)
      expect(encodedLength(v)).toBe(s.input_encoded_bytes)
      const c = cutString(v)
      if (!s.expected.cut) return expect(c).toBe(v)
      const kept = c.slice(0, c.length - s.expected.marker.length)
      expect(c.slice(kept.length)).toBe(s.expected.marker)
      expect(kept).toBe(v.slice(0, s.expected.kept_utf16_units))
      expect(Buffer.byteLength(kept)).toBe(s.expected.kept_utf8_bytes)
      expect(encodedLength(c)).toBe(s.expected.output_encoded_bytes)
      expect(encodedLength(c)).toBeLessThanOrEqual(STRING_CAP)
    })
  }

  test("frame: over the frame cap becomes a stub sized and hashed over the same line", () => {
    const r = caps.frames[0].recipe
    const props = structuredClone(r.properties)
    props.part.state.input = Object.fromEntries(Array.from({ length: 20 }, (_, i) => ["f" + i, "x".repeat(70000)]))
    const e = eventLine(r.seq, r.type, props)
    expect(e.stub).toBe(true)
    expect(e.line).toBe(caps.frames[0].expected.line)
    expect(e.bytes).toBe(caps.frames[0].expected.line_bytes)
    const full = JSON.stringify({ t: "ev", seq: r.seq, type: r.type, properties: prepare(props) }) + "\n"
    const stub = JSON.parse(e.line)
    expect(stub.size).toBe(Buffer.byteLength(full))
    expect(stub.sha256).toBe(sha(full))
  })

  test("frame: a small frame passes unchanged", () => {
    const f = caps.frames[1]
    const e = eventLine(f.input.seq, f.input.type, f.input.properties)
    expect(e).toEqual({ line: f.expected.line, bytes: f.expected.line_bytes, stub: false })
  })

  for (const p of caps.patch_bodies) {
    test(`patch bodies: ${p.what}`, () => {
      const before = JSON.stringify(p.input)
      expect(prepare(p.input)).toEqual(p.expected)
      expect(JSON.stringify(p.input)).toBe(before)
    })
  }

  for (const c of cards.cards) {
    test(`card ${c.name}: ${c.what}`, () => {
      const cmd = build(c.command_recipe)
      const t = c.card_template
      const card = { id: t.id, sessionID: t.sessionID, permission: t.permission, patterns: [cmd], always: t.always,
        metadata: { command: cmd, description: "d" }, tool: t.tool, anchor_input: { command: cmd, description: "d" } }
      const e = cardLine(1, "permission.asked", card)
      expect(e.stub).toBe(c.expected.stub)
      if (c.expected.stub) {
        expect(e.line).toBe(c.expected.line)
        expect(e.bytes).toBe(c.expected.line_bytes)
        expect(e.line.includes("rm -rf")).toBe(false)
        return
      }
      expect(e.bytes).toBe(c.expected.line_bytes)
      expect(sha(e.line)).toBe(c.expected.line_sha256)
      const f = JSON.parse(e.line).properties
      expect(f.metadata.command).toBe(cmd)
      expect(f.patterns[0]).toBe(cmd)
      expect(f.anchor_input.command).toBe(cmd)
    })
  }

  test("a question card keeps its questions whole", () => {
    const long = "q".repeat(100000)
    const e = cardLine(3, "question.asked", { id: "que_1", sessionID: "ses_1", questions: [{ question: long, options: [] }] })
    expect(JSON.parse(e.line).properties.questions[0].question).toBe(long)
    const ev = eventLine(3, "message.part.updated", { part: { text: long } })
    expect(JSON.parse(ev.line).properties.part.text.length).toBeLessThan(long.length)
  })
})

import Foundation

/// A Swift mirror of `serde_json::Value`.
///
/// Event payloads are untyped on the wire by design (`protocol/src/event.rs`:
/// "the untouched original is always kept as the event payload"), so the client
/// must be able to hold, re-render and re-serialise arbitrary JSON without
/// losing anything a newer daemon might add.
enum JSONValue: Sendable, Hashable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])
}

// MARK: - Codable

extension JSONValue: Codable {
    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
            // Int before Double: serde_json keeps integers as integers, and
            // round-tripping 134 as 134.0 would break payload-hash verification.
        } else if let value = try? container.decode(Int64.self) {
            self = .int(value)
        } else if let value = try? container.decode(Double.self) {
            self = .double(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
        } else {
            throw DecodingError.dataCorruptedError(
                in: container, debugDescription: "value is not JSON")
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case .bool(let value): try container.encode(value)
        case .int(let value): try container.encode(value)
        case .double(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        }
    }
}

// MARK: - Reading

extension JSONValue {
    subscript(key: String) -> JSONValue? {
        guard case .object(let map) = self else { return nil }
        return map[key]
    }

    subscript(index: Int) -> JSONValue? {
        guard case .array(let items) = self, items.indices.contains(index) else { return nil }
        return items[index]
    }

    var stringValue: String? {
        if case .string(let value) = self { return value }
        return nil
    }

    var intValue: Int? {
        switch self {
        case .int(let value): return Int(exactly: value)
        case .double(let value): return Int(exactly: value.rounded())
        default: return nil
        }
    }

    var boolValue: Bool? {
        if case .bool(let value) = self { return value }
        return nil
    }

    var arrayValue: [JSONValue]? {
        if case .array(let items) = self { return items }
        return nil
    }

    var objectValue: [String: JSONValue]? {
        if case .object(let map) = self { return map }
        return nil
    }

    var isNull: Bool {
        if case .null = self { return true }
        return false
    }

    /// The first non-empty string found at any of `keys`, in order.
    func firstString(_ keys: String...) -> String? {
        for key in keys {
            if let value = self[key]?.stringValue, !value.isEmpty { return value }
        }
        return nil
    }
}

// MARK: - Canonical serialisation

extension JSONValue {
    /// Byte-for-byte compatible with `serde_json::Value::to_string()` for the
    /// shapes Claude Code actually emits.
    ///
    /// The daemon hashes `"{tool_name}\n{tool_input}"` (`protocol/src/hash.rs`).
    /// Reproducing that rendering is how the phone proves the structured fields
    /// it is showing are the ones covered by `payload_hash` instead of taking
    /// the daemon's word for it.
    ///
    /// Three properties have to match `serde_json` exactly:
    ///   * key order — `Value`'s map is a `BTreeMap` (the `preserve_order`
    ///     feature is off), so keys sort by their UTF-8 bytes, *not* by Swift's
    ///     default Unicode collation;
    ///   * separators — compact, no spaces;
    ///   * escaping — only `"`, `\` and control bytes; non-ASCII stays raw and
    ///     `/` is never escaped.
    ///
    /// Known divergence: `Foundation`'s decoder turns a whole-number float
    /// (`1.0`) into an integer, which re-renders as `1` where `serde_json`
    /// writes `1.0`. Claude Code tool inputs contain no floats, and the hash
    /// check that actually gates the UI runs against `display_text` — the exact
    /// hashed string, carried on the wire — so this can only ever cost a
    /// "showing the daemon's text instead" note, never a wrong verdict.
    var canonicalJSONString: String {
        var out = ""
        writeCanonical(into: &out)
        return out
    }

    private func writeCanonical(into out: inout String) {
        switch self {
        case .null:
            out += "null"
        case .bool(let value):
            out += value ? "true" : "false"
        case .int(let value):
            out += String(value)
        case .double(let value):
            out += Self.canonicalDouble(value)
        case .string(let value):
            Self.writeEscaped(value, into: &out)
        case .array(let items):
            out += "["
            for (index, item) in items.enumerated() {
                if index > 0 { out += "," }
                item.writeCanonical(into: &out)
            }
            out += "]"
        case .object(let map):
            out += "{"
            let keys = map.keys.sorted { lhs, rhs in
                lhs.utf8.lexicographicallyPrecedes(rhs.utf8)
            }
            for (index, key) in keys.enumerated() {
                if index > 0 { out += "," }
                Self.writeEscaped(key, into: &out)
                out += ":"
                map[key]?.writeCanonical(into: &out)
            }
            out += "}"
        }
    }

    /// `serde_json` renders non-finite floats as `null`; everything else goes
    /// through a shortest-round-trip formatter, which is what Swift's own
    /// `description` provides.
    private static func canonicalDouble(_ value: Double) -> String {
        guard value.isFinite else { return "null" }
        return "\(value)"
    }

    private static let hexDigits = Array("0123456789abcdef")

    private static func writeEscaped(_ string: String, into out: inout String) {
        out += "\""
        for scalar in string.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\u{08}": out += "\\b"
            case "\u{09}": out += "\\t"
            case "\u{0A}": out += "\\n"
            case "\u{0C}": out += "\\f"
            case "\u{0D}": out += "\\r"
            default:
                if scalar.value < 0x20 {
                    let byte = UInt8(scalar.value)
                    out += "\\u00"
                    out.append(hexDigits[Int(byte >> 4)])
                    out.append(hexDigits[Int(byte & 0x0F)])
                } else {
                    out.unicodeScalars.append(scalar)
                }
            }
        }
        out += "\""
    }

    /// Human-readable rendering for disclosure rows — pretty, stable key order.
    var prettyJSONString: String {
        var out = ""
        writePretty(indent: 0, into: &out)
        return out
    }

    private func writePretty(indent: Int, into out: inout String) {
        let pad = String(repeating: "  ", count: indent)
        let innerPad = String(repeating: "  ", count: indent + 1)
        switch self {
        case .array(let items) where !items.isEmpty:
            out += "[\n"
            for (index, item) in items.enumerated() {
                out += innerPad
                item.writePretty(indent: indent + 1, into: &out)
                out += index == items.count - 1 ? "\n" : ",\n"
            }
            out += pad + "]"
        case .object(let map) where !map.isEmpty:
            out += "{\n"
            let keys = map.keys.sorted { $0.utf8.lexicographicallyPrecedes($1.utf8) }
            for (index, key) in keys.enumerated() {
                out += innerPad
                Self.writeEscaped(key, into: &out)
                out += ": "
                map[key]?.writePretty(indent: indent + 1, into: &out)
                out += index == keys.count - 1 ? "\n" : ",\n"
            }
            out += pad + "}"
        default:
            writeCanonical(into: &out)
        }
    }
}

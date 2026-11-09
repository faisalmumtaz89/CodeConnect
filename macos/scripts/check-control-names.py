#!/usr/bin/env python3
"""Every control in the Mac app can be found and read by accessibility alone. Fails, naming file and line, on a
Button, Menu, Picker, TextField, SecureField or Toggle with no accessibilityIdentifier among the modifiers after it,
or with no name: an empty title, a text field without an accessibilityLabel (its title names nothing), or a label
(the last closure) with an image but no Text, Label or accessibilityLabel. A label built from a view of the app's own
is not looked into. A menu bar command (in a CommandGroup) is AppKit's menu item, found by its title: no identifier
reaches it."""
import pathlib
import re
import sys

CONTROL = re.compile(r"\b(Button|Menu|Picker|TextField|SecureField|Toggle)\s*([({])")
COMMANDS = re.compile(r"\bCommandGroup\s*\(")
UNTITLED = re.compile(r"\s*(action|role|content|label|selection|isOn|text):")
IMAGE = re.compile(r"\bImage\(")
TEXT = re.compile(r"\b(Text|Label)\(")


def code(source):
    """The source with whole-line comments emptied and every string literal's contents blanked, quotes kept, so
    only code is read and every offset still points at its line."""
    source = "\n".join("" if line.lstrip().startswith("//") else line for line in source.split("\n"))
    out, in_string, i = [], False, 0
    while i < len(source):
        c = source[i]
        if in_string and c == "\\":
            out.append("  ")
            i += 2
            continue
        if c == '"':
            in_string = not in_string
            out.append(c)
        else:
            out.append(" " if in_string and c != "\n" else c)
        i += 1
    return "".join(out)


def close(text, i):
    """The index just past the bracket that closes the one at text[i]."""
    want = {"(": ")", "{": "}", "[": "]"}
    stack = []
    for j in range(i, len(text)):
        if text[j] in want:
            stack.append(want[text[j]])
        elif stack and text[j] == stack[-1]:
            stack.pop()
            if not stack:
                return j + 1
    return len(text)


def call(text, i, opener):
    """The call whose bracket is at i: its title (its first argument, unless that is a labelled one; None without
    one) and where it ends, its trailing closures (`{…}`, `label: {…}`) included."""
    end = close(text, i)
    title = None
    if opener == "(" and not UNTITLED.match(text, i + 1):
        title = text[i + 1 : end - 1].split(",")[0].strip()
    while found := re.match(r"\s*(\w+:\s*)?\{", text[end:]):
        end = close(text, end + found.end() - 1)
    return title, end


def label(text, i, end):
    """The call's last closure: its label, whatever the form (`Button { … } label: { … }`, `Button(action: a) { … }`,
    `Menu(content: { … }, label: { … })`); a menu's content and a button's action come before it."""
    last, j = None, i
    while j < end:
        if text[j] == "{":
            last = (j, close(text, j))
            j = last[1]
        else:
            j += 1
    return text[last[0] : last[1]] if last else ""


def chain(text, end):
    """The modifiers applied right after the call: its line's rest, and each next line that starts with a dot, as
    Swift reads them, across blank lines and the comments emptied out."""
    lines = text[end:].split("\n")
    kept = [lines[0]]
    for line in lines[1:]:
        if not line.strip():
            continue
        if not line.lstrip().startswith("."):
            break
        kept.append(line)
    return "\n".join(kept)


def named(kind, title, shown, after):
    """Whether the control has a name: `shown` is its label closure, `after` the modifiers after it."""
    if ".accessibilityLabel(" in shown + after:
        return True
    if kind in ("TextField", "SecureField"):
        return False
    if title is not None:
        return title != '""'
    return not (IMAGE.search(shown) and not TEXT.search(shown))


def check(path):
    text = code(path.read_text())
    commands = [(m.start(), call(text, m.end() - 1, "(")[1]) for m in COMMANDS.finditer(text)]
    found = []
    for m in CONTROL.finditer(text):
        if any(start <= m.start() < end for start, end in commands):
            continue
        kind = m.group(1)
        title, end = call(text, m.start(2), m.group(2))
        shown = label(text, m.start(2), end) if title is None and kind in ("Button", "Menu") else ""
        after = chain(text, end)
        if ".accessibilityIdentifier(" not in after:
            found.append((m.start(), f"{kind} with no accessibilityIdentifier"))
        if not named(kind, title, shown, after):
            found.append((m.start(), f"{kind} with no name"))
    return sorted((text.count("\n", 0, at) + 1, what) for at, what in found)


# The app's folder, wherever the script is run from; or the folder given.
root = (pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).parent.parent).resolve()
failures = [(path, line, what) for path in sorted(root.rglob("*.swift")) for line, what in check(path)]
for path, line, what in failures:
    print(f"{path.relative_to(root.parent)}:{line}: {what}")
if failures:
    print("::error::a control can't be found or read by accessibility: give it .accessibilityIdentifier and a name")
    sys.exit(1)

import XCTest

@testable import CodeConnectKit

/// A run's changed files as the Changes panel lists them (`ChangedFile.rows`): the computer's own list (minor 53) by
/// folder and then name, each joined to its part of the captured diff by its path (a deleted file's by its old one),
/// and from an older computer the files the diff holds.
final class ChangedFilesTests: XCTestCase {
    private let unified = """
        diff --git a/macos/Views/Reply.swift b/macos/Views/Reply.swift
        index 1111111..2222222 100644
        --- a/macos/Views/Reply.swift
        +++ b/macos/Views/Reply.swift
        @@ -1,2 +1,2 @@
        -old line
        +new line
         kept
        diff --git a/README.md b/README.md
        deleted file mode 100644
        index 3333333..0000000
        --- a/README.md
        +++ /dev/null
        @@ -1 +0,0 @@
        -gone
        diff --git a/kit/Old.swift b/kit/New.swift
        similarity index 100%
        rename from kit/Old.swift
        rename to kit/New.swift

        """

    private func diff(files: String?) throws -> SessionDiff {
        let list = files.map { #","files":\#($0)"# } ?? ""
        let json = #"{"session_id":"s","unified":"","captured_at":"2026-11-26T10:00:00Z"\#(list)}"#
        return try JSONDecoder().decode(SessionDiff.self, from: Data(json.utf8))
    }

    func testTheComputersListIsSortedByFolderThenNameAndJoinedToItsDiff() throws {
        let raw = try diff(files: """
            [{"path":"macos/Views/Reply.swift","status":"modified","added":1,"removed":1},
             {"path":"notes/todo.txt","status":"untracked","added":3,"removed":0},
             {"path":"README.md","status":"deleted","added":0,"removed":1},
             {"path":"kit/New.swift","old_path":"kit/Old.swift","status":"renamed","added":0,"removed":0},
             {"path":"assets/logo.png","status":"added","binary":true}]
            """)
        let rows = ChangedFile.rows(raw, UnifiedDiff.parse(unified))

        XCTAssertEqual(
            rows.map(\.path),
            ["README.md", "assets/logo.png", "kit/New.swift", "macos/Views/Reply.swift", "notes/todo.txt"],
            "by folder, the top folder first, then by name")
        XCTAssertEqual(rows.map(\.folder), ["", "assets/", "kit/", "macos/Views/", "notes/"])
        XCTAssertEqual(rows.map(\.name), ["README.md", "logo.png", "New.swift", "Reply.swift", "todo.txt"])

        let byPath = Dictionary(uniqueKeysWithValues: rows.map { ($0.path, $0) })
        XCTAssertEqual(byPath["macos/Views/Reply.swift"]?.diff?.newPath, "macos/Views/Reply.swift")
        XCTAssertEqual(byPath["README.md"]?.diff?.oldPath, "README.md", "a deleted file joins by its old path")
        XCTAssertEqual(byPath["kit/New.swift"]?.diff?.status, .renamed, "a renamed file joins by its new path")
        XCTAssertNil(byPath["notes/todo.txt"]?.diff, "an untracked file has no part of the captured diff")
        XCTAssertEqual(byPath["notes/todo.txt"]?.added, 3, "its lines are the computer's count")
        XCTAssertNil(byPath["assets/logo.png"]?.added, "a binary file's lines are not counted")
        XCTAssertNil(byPath["assets/logo.png"]?.diff)
    }

    /// A file git prints with a quoted path (its default for a name outside ASCII) is in the computer's list but joins
    /// no parsed file: its row has no diff to open, and its diff is still drawn among the blocks.
    func testEveryParsedFileIsABlockEvenWhenItsRowCannotJoinIt() throws {
        let quoted = #"""
            diff --git "a/caf\303\251.txt" "b/caf\303\251.txt"
            index 1111111..2222222 100644
            --- "a/caf\303\251.txt"
            +++ "b/caf\303\251.txt"
            @@ -1 +1 @@
            -a
            +b

            """#
        let raw = try diff(files: #"[{"path":"café.txt","status":"modified","added":1,"removed":1}]"#)
        let parsed = UnifiedDiff.parse(quoted)

        XCTAssertNil(ChangedFile.rows(raw, parsed).first?.diff, "the quoted header names no file the list names")
        XCTAssertEqual(ChangedFile.blocks(parsed).count, 1, "its diff is still a block")
        XCTAssertNotNil(ChangedFile.blocks(parsed).first?.diff)
    }

    func testAnOlderComputersRowsAreTheFilesItsDiffHolds() throws {
        let rows = ChangedFile.rows(try diff(files: nil), UnifiedDiff.parse(unified))

        XCTAssertEqual(rows.map(\.path), ["README.md", "kit/New.swift", "macos/Views/Reply.swift"])
        XCTAssertEqual(rows.map(\.added), [0, 0, 1])
        XCTAssertEqual(rows.map(\.removed), [1, 0, 1])
        XCTAssertEqual(rows.map { $0.diff?.status }, [.deleted, .renamed, .modified])
    }
}

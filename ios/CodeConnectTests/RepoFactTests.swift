import XCTest

@testable import CodeConnectKit

/// The repository a run works in, as the daemon sends it on its summary (`repo`, minor 71).
final class RepoFactTests: XCTestCase {
    private func summary(_ extra: String) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"u-1","session_id":"cc-1","cwd":"/web/.claude/worktrees/fix+login","lifecycle":"live",\
                "link":"attached","last_seq":9,"created_at":"2026-11-27T09:00:00.000Z",\
                "updated_at":"2026-11-27T09:00:00.000Z"\(extra)}
                """.utf8))
    }

    func testASummaryNamesItsRepository() throws {
        XCTAssertEqual(
            try summary(#","repo":{"name":"web","remote":"github.com/ada/web","main_path":"/web"}"#).repo,
            RepoFact(name: "web", remote: "github.com/ada/web", mainPath: "/web"))
        XCTAssertEqual(
            try summary(#","repo":{"name":"web","main_path":"/web"}"#).repo,
            RepoFact(name: "web", remote: nil, mainPath: "/web"), "no remote on GitHub")
        XCTAssertNil(try summary("").repo, "absent: outside a repository, or an older daemon")
    }
}

import UIKit
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **A photo goes as the agent can read it, by what its bytes are**: PNG and JPEG as the library holds them, HEIC as
/// JPEG, bytes that are no image not at all, and a photo the library won't hand over as a refused tile with why.
@MainActor
final class PhotoExportTests: XCTestCase {

    private func image() -> UIImage {
        UIGraphicsImageRenderer(size: CGSize(width: 4, height: 4)).image { context in
            UIColor.red.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 4, height: 4))
        }
    }

    func testAPNGGoesAsItIs() throws {
        let png = try XCTUnwrap(image().pngData())
        let photo = try XCTUnwrap(PhotoExport.prepare(png))
        XCTAssertEqual(photo.data, png)
        XCTAssertEqual(photo.name, "image.png")
    }

    func testAJPEGGoesAsItIs() throws {
        let jpeg = try XCTUnwrap(image().jpegData(compressionQuality: 0.9))
        let photo = try XCTUnwrap(PhotoExport.prepare(jpeg))
        XCTAssertEqual(photo.data, jpeg)
        XCTAssertEqual(photo.name, "photo.jpg")
    }

    /// The library's HEIC goes as JPEG bytes, whatever types the picker lists for it.
    func testAHEICGoesAsJPEG() throws {
        let heic = try XCTUnwrap(image().heicData())
        let photo = try XCTUnwrap(PhotoExport.prepare(heic))
        XCTAssertEqual(photo.name, "photo.jpg")
        XCTAssertEqual(Array(photo.data.prefix(3)), [0xFF, 0xD8, 0xFF], "JPEG's magic")
    }

    func testBytesThatAreNoImageGoNowhere() {
        XCTAssertNil(PhotoExport.prepare(Data("not an image".utf8)))
    }

    /// A photo the library won't hand over is a tile all the same: refused, saying why.
    func testAPhotoThatCantBeReadIsARefusedTile() throws {
        let draft = StartDraft()
        draft.attach(name: "photo", size: 1) { throw PhotoExport.Unreadable() }
        let file = try XCTUnwrap(draft.files.first)
        guard case .refused(let reason) = file.state else { return XCTFail("refused, not \(file.state)") }
        XCTAssertTrue(reason.contains("didn’t hand it over"), reason)
    }
}

/// **What holds what, on the iPhone, while files travel**: a file on its way holds the message, not the run's
/// settings; files for a computer that takes none hold Start, as the Mac says it.
@MainActor
final class UploadHoldsTests: XCTestCase {

    func testAFileOnItsWayLeavesTheSettingsOpen() async throws {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, sendText: true, extra: ["uploads": .bool(true)]), minor: 50)
        // The computer never answers a chunk: the file stays on its way.
        model.connection.sendStub = { _ in }
        model.connection.injectForTesting(
            .sessions([
                try JSONDecoder().decode(
                    SessionSummary.self,
                    from: Data(
                        """
                        {"session_uid":"u-1","session_id":"cc-1",
                         "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                         "last_seq":0,"created_at":"2026-08-18T09:00:00.000Z",
                         "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],"agent":"claude"}
                        """.utf8))
            ]))
        try await XCTUnwrap(model.statesForTesting["u-1"]).settleForTesting()
        model.attach(data: Data("abc".utf8), name: "a.txt", to: "u-1")
        XCTAssertNotNil(model.filesBlockedReason(for: "u-1"), "the file holds the message")
        XCTAssertFalse(HostedSettingsBar.isHeld(model, "u-1"), "but not the run's model and effort")
    }

    func testFilesForAComputerThatTakesNoneHoldStart() {
        let draft = StartDraft()
        draft.attach(name: "a.txt", size: 3) { Data("abc".utf8) }
        XCTAssertEqual(
            StartStepsSheet.filesHoldStart(
                files: draft.files, reason: draft.filesBlockedReason, takesUploads: false, machine: "Jordan’s Mac"),
            "Jordan’s Mac can’t take files.")
        XCTAssertNil(
            StartStepsSheet.filesHoldStart(
                files: draft.files, reason: draft.filesBlockedReason, takesUploads: true, machine: "Jordan’s Mac"))
        XCTAssertNil(
            StartStepsSheet.filesHoldStart(files: [], reason: nil, takesUploads: false, machine: "Jordan’s Mac"))
    }
}

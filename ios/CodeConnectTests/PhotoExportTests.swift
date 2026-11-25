import UIKit
import UniformTypeIdentifiers
import XCTest

@testable import CodeConnect

/// **A photo goes as the agent can read it**: PNG, GIF and JPEG as the library holds them, HEIC as JPEG, and
/// bytes that are no image not at all.
final class PhotoExportTests: XCTestCase {

    private func image() -> UIImage {
        UIGraphicsImageRenderer(size: CGSize(width: 4, height: 4)).image { context in
            UIColor.red.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 4, height: 4))
        }
    }

    func testAPNGGoesAsItIs() throws {
        let png = try XCTUnwrap(image().pngData())
        let photo = try XCTUnwrap(PhotoExport.prepare(png, types: [.png]))
        XCTAssertEqual(photo.data, png)
        XCTAssertEqual(photo.name, "image.png")
    }

    func testAJPEGGoesAsItIs() throws {
        let jpeg = try XCTUnwrap(image().jpegData(compressionQuality: 0.9))
        let photo = try XCTUnwrap(PhotoExport.prepare(jpeg, types: [.jpeg]))
        XCTAssertEqual(photo.data, jpeg)
        XCTAssertEqual(photo.name, "photo.jpg")
    }

    /// The library's HEIC (here any image the agent may not read, by its type) goes as JPEG bytes.
    func testAHEICGoesAsJPEG() throws {
        let png = try XCTUnwrap(image().pngData())
        let photo = try XCTUnwrap(PhotoExport.prepare(png, types: [.heic]))
        XCTAssertEqual(photo.name, "photo.jpg")
        XCTAssertEqual(Array(photo.data.prefix(3)), [0xFF, 0xD8, 0xFF], "JPEG's magic")
    }

    func testBytesThatAreNoImageGoNowhere() {
        XCTAssertNil(PhotoExport.prepare(Data("not an image".utf8), types: [.heic]))
    }
}

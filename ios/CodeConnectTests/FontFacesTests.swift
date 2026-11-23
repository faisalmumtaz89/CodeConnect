import UIKit
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// Words are set in Inter and code in Menlo: every face the type names resolves on the device at its own weight, and
/// what measures the type measures those faces, so a line's leading, an icon's lift and a code column's width are
/// the ones the text is drawn with.
final class FontFacesTests: XCTestCase {

    private func width(_ font: UIFont) -> CGFloat {
        ("Hamburgefonstiv 0123456789 sphinx of black quartz" as NSString).size(withAttributes: [.font: font]).width
    }

    /// A variable font's named instance that fell back to the default would keep its name and draw regular.
    func testInterResolvesAtEachWeight() throws {
        let regular = try XCTUnwrap(UIFont(name: "InterVariable", size: 13))
        let medium = try XCTUnwrap(UIFont(name: "InterVariable-Medium", size: 13))
        let semibold = try XCTUnwrap(UIFont(name: "InterVariable-SemiBold", size: 13))
        XCTAssertEqual([regular.fontName, medium.fontName, semibold.fontName],
                       ["InterVariable", "InterVariable-Medium", "InterVariable-SemiBold"])
        XCTAssertLessThan(width(regular), width(medium), "medium draws wider than regular")
        XCTAssertLessThan(width(medium), width(semibold), "semibold draws wider than medium")
    }

    func testMenloResolvesInBothWeights() throws {
        XCTAssertEqual(try XCTUnwrap(UIFont(name: "Menlo-Regular", size: 13)).fontName, "Menlo-Regular")
        XCTAssertEqual(try XCTUnwrap(UIFont(name: "Menlo-Bold", size: 13)).fontName, "Menlo-Bold")
    }

    /// `.ccType`'s leading and a mark's lift beside text both measure `uiFont(ofSize:)`.
    func testTheWordsMeasureInterAtTheirWeight() {
        XCTAssertEqual(CC.type.body.uiFont(ofSize: 17).fontName, "InterVariable")
        XCTAssertEqual(CC.type.micro.uiFont(ofSize: 11).fontName, "InterVariable-Medium")
        XCTAssertEqual(CC.type.headline.uiFont(ofSize: 17).fontName, "InterVariable-SemiBold")
    }

    /// Menlo has no semibold: a heavier run of code is its bold.
    func testTheCodeMeasuresMenlo() {
        XCTAssertEqual(CC.type.mono.uiFont(ofSize: 14).fontName, "Menlo-Regular")
        XCTAssertEqual(CC.type.headline.monospaced().uiFont(ofSize: 17).fontName, "Menlo-Bold")
    }

    /// The diff's columns are counted in Menlo's advance.
    func testTheDiffGridCountsMenloColumns() throws {
        let menlo = try XCTUnwrap(UIFont(name: "Menlo-Regular", size: 14))
        let zero = ("0" as NSString).size(withAttributes: [.font: menlo]).width
        XCTAssertEqual(CCDiffMetrics(fontSize: 14, availableWidth: 320).advance, zero, accuracy: 0.001)
    }
}

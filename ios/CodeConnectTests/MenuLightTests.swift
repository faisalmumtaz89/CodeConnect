import CodeConnectKit
import XCTest

/// A menu panel's one lit row (decision 287): the keys and the pointer move one light over the rows that can be
/// chosen, and Return chooses it or closes the panel.
final class MenuLightTests: XCTestCase {
    /// Rows 0, 2 and 3 can be chosen; row 1 cannot (a run's checked model, a missing worktree).
    private let rows = [true, false, true, true]

    private func pressed(_ keys: [MenuLight.Key], over enabled: [Bool], from lit: Int? = nil) -> MenuLight {
        var light = MenuLight(lit: lit)
        for key in keys { XCTAssertEqual(light.press(key, enabled: enabled), .moved, "\(key)") }
        return light
    }

    /// ↓ from none lights the first row that can be chosen and steps over one that cannot; ↓ and ↑ stop at the ends.
    func testTheArrowsStepOverRowsThatCannotBeChosenAndStopAtTheEnds() {
        XCTAssertEqual(pressed([.down], over: rows).lit, 0)
        XCTAssertEqual(pressed([.down, .down], over: rows).lit, 2, "row 1 is stepped over")
        XCTAssertEqual(pressed([.down, .down, .down, .down, .down], over: rows).lit, 3, "↓ stops at the last row")
        XCTAssertEqual(pressed([.up], over: rows, from: 2).lit, 0, "row 1 is stepped over going up too")
        XCTAssertEqual(pressed([.up, .up], over: rows, from: 2).lit, 0, "↑ stops at the first row")
    }

    /// ↑ from none lights the last row that can be chosen.
    func testUpFromNoneLightsTheLastRow() {
        XCTAssertEqual(pressed([.up], over: rows).lit, 3)
        XCTAssertEqual(pressed([.up], over: [true, true, false]).lit, 1)
    }

    /// Return chooses the lit row; with none lit, or on a lit row that can no longer be chosen (a call went in flight),
    /// it closes the panel.
    func testReturnChoosesTheLitRowElseCloses() {
        var light = MenuLight(lit: 2)
        XCTAssertEqual(light.press(.return, enabled: rows), .choose(2))
        light = MenuLight()
        XCTAssertEqual(light.press(.return, enabled: rows), .close)
        light = MenuLight(lit: 2)
        XCTAssertEqual(light.press(.return, enabled: [true, true, false]), .close, "lit, then disabled")
    }

    /// No rows, or none that can be chosen: the arrows light nothing and Return closes.
    func testAPanelWithNothingToChooseLightsNothing() {
        for enabled in [[], [false, false]] {
            XCTAssertNil(pressed([.down, .up, .down], over: enabled).lit, "\(enabled)")
            var light = MenuLight()
            XCTAssertEqual(light.press(.return, enabled: enabled), .close)
        }
    }

    /// The pointer entering a row lights it, unless it cannot be chosen: the checked row never lights.
    func testThePointerLightsOnlyARowThatCanBeChosen() {
        var light = MenuLight()
        light.enter(1, enabled: rows)
        XCTAssertNil(light.lit, "the pointer on the checked row lights nothing")
        light.enter(3, enabled: rows)
        XCTAssertEqual(light.lit, 3)
        light.enter(1, enabled: rows)
        XCTAssertEqual(light.lit, 3, "nor does it take the light from the lit row")
        light.enter(9, enabled: rows)
        XCTAssertEqual(light.lit, 3)
    }

    /// A panel takes ↓, ↑, Return and the keypad's Enter bare; with ⌘, ⌃, ⇧ or ⌥ held they are what has the keyboard's:
    /// ⇧Return and ⌥Return add a line in the reply box under an open panel.
    func testAPanelTakesOnlyBareKeys() {
        func key(_ code: UInt16, command: Bool = false, control: Bool = false, shift: Bool = false, option: Bool = false)
            -> MenuLight.Key?
        {
            MenuLight.key(code: code, command: command, control: control, shift: shift, option: option)
        }
        XCTAssertEqual(key(125), .down)
        XCTAssertEqual(key(126), .up)
        XCTAssertEqual(key(36), .return)
        XCTAssertEqual(key(76), .return, "the keypad's Enter")
        XCTAssertNil(key(53), "Esc is the panel's closing, not a key of its light")
        XCTAssertNil(key(0), "a letter goes on to the field")
        for code: UInt16 in [125, 126, 36, 76] {
            XCTAssertNil(key(code, command: true), "⌘ \(code)")
            XCTAssertNil(key(code, control: true), "⌃ \(code)")
            XCTAssertNil(key(code, shift: true), "⇧ \(code)")
            XCTAssertNil(key(code, option: true), "⌥ \(code)")
        }
    }

    /// The pointer and the keys move one light: a key goes on from where the pointer left it, and the pointer takes it
    /// from where the keys left it.
    func testThePointerAndTheKeysMoveOneLight() {
        var light = MenuLight()
        light.enter(2, enabled: rows)
        _ = light.press(.down, enabled: rows)
        XCTAssertEqual(light.lit, 3, "pointer, then a key")
        _ = light.press(.up, enabled: rows)
        light.enter(0, enabled: rows)
        XCTAssertEqual(light.lit, 0, "a key, then the pointer")
        XCTAssertEqual(light.press(.return, enabled: rows), .choose(0))
    }
}

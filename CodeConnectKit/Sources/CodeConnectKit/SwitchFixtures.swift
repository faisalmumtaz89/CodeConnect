#if DEBUG
    import Foundation

    /// **A run moved to another account, staged for the render harness and the UI tests.**
    ///
    /// `-CC_SWITCH <recording>` replays one of the daemon's own recordings of a switch (`fixtures/accounts/<recording>
    /// -wire.json`, ccd-emitted, copied into the app bundle): its events and the session lists ccd sent after each,
    /// every frame through the real `ServerMessage` decoder and the real ingest path, after the hello a minor-48 ccd
    /// sends (`minor-48-hello-wire.json`, recorded with them).
    /// `-CC_SWITCH_STEP <n>` stops after the recording's `n`th frame, so a scene shows the run at that point of its
    /// switch (waiting, moved, failed, ended); without it the whole recording plays. Nothing is rewritten.
    enum SwitchFixtures {
        static var recording: String? { UserDefaults.standard.string(forKey: "CC_SWITCH") }

        static func frames(recording: String) -> [ServerMessage] {
            guard let hello = recorded("minor-48-hello"), let recorded = recorded(recording) else { return [] }
            let step = UserDefaults.standard.object(forKey: "CC_SWITCH_STEP") == nil
                ? recorded.count : UserDefaults.standard.integer(forKey: "CC_SWITCH_STEP")
            return (hello + recorded.prefix(step)).compactMap { Fixtures.decode($0.canonicalJSONString) }
        }

        /// A recording's frames, from the app bundle.
        private static func recorded(_ name: String) -> [JSONValue]? {
            guard
                let url = Bundle.main.url(forResource: "\(name)-wire", withExtension: "json"),
                let data = try? Data(contentsOf: url)
            else { return nil }
            return (try? JSONDecoder().decode(JSONValue.self, from: data))?.arrayValue
        }
    }
#endif

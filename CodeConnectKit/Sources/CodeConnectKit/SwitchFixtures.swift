#if DEBUG
    import Foundation

    /// **A run moved to another account, staged for the render harness and the UI tests.**
    ///
    /// `-CC_SWITCH <recording>` replays one of the daemon's own recordings of a switch (`fixtures/accounts/<recording>
    /// -wire.json`, ccd-emitted, copied into the app bundle): its events and the session lists ccd sent after each,
    /// every frame through the real `ServerMessage` decoder and the real ingest path, after the fixtures' hello.
    /// `-CC_SWITCH_STEP <n>` stops after the recording's `n`th frame, so a scene shows the run at that point of its
    /// switch (waiting, moved, failed, ended); without it the whole recording plays. Nothing is rewritten.
    enum SwitchFixtures {
        static var recording: String? { UserDefaults.standard.string(forKey: "CC_SWITCH") }

        static func frames(recording: String) -> [ServerMessage] {
            guard
                let url = Bundle.main.url(forResource: "\(recording)-wire", withExtension: "json"),
                let data = try? Data(contentsOf: url),
                let recorded = (try? JSONDecoder().decode(JSONValue.self, from: data))?.arrayValue
            else { return [] }
            let step = UserDefaults.standard.object(forKey: "CC_SWITCH_STEP") == nil
                ? recorded.count : UserDefaults.standard.integer(forKey: "CC_SWITCH_STEP")
            return ([Fixtures.decode(Fixtures.helloAckJSON)] + recorded.prefix(step).map {
                Fixtures.decode($0.canonicalJSONString)
            }).compactMap { $0 }
        }
    }
#endif

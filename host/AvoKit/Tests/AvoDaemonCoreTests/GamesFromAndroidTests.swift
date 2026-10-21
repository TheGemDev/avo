import AvoKit
import AvoProtocol
import AvoTestSupport
import Foundation
import Testing
@testable import AvoDaemonCore

/// Android says which apps are games (their manifest's category): their
/// windows use the Games profile without a key mapping.
@Suite(.serialized, .timeLimit(.minutes(1))) struct GamesFromAndroidTests {
    /// The helper's `appinfo` (guest/clipboard AppInfo.java), as Android 17 printed it.
    @Test func theHelperSaysWhichAppsAreGames() throws {
        let reports = HostAgent.parseAppInfo("""
            {"package":"com.nekki.shadowfight3","label":"Shadow Fight 3","system":false,"game":true}
            {"package":"com.android.chrome","label":"Chrome","system":false,"game":false}
            {"package":"com.example.old","label":"Old Helper"}
            """)
        func info(_ package: String) throws -> Avo_V1_AppInfo {
            var info = Avo_V1_AppInfo()
            info.package = package
            HostAgent.fill(&info, from: try #require(reports[package]))
            return info
        }
        #expect(try info("com.nekki.shadowfight3").hasGame)
        #expect(try info("com.nekki.shadowfight3").game)
        #expect(try info("com.android.chrome").hasGame)
        #expect(try !info("com.android.chrome").game)
        // An older helper does not say: nothing is known.
        #expect(try !info("com.example.old").hasGame)
    }

    /// Only Android's answer changes what is known: an APK read on the Mac or
    /// a bare listing (no `game`) keeps it.
    @Test func theRegistryKeepsWhatAndroidSaid() async throws {
        let root = FileManager.default.temporaryDirectory.appending(path: "avo-games-\(UUID().uuidString.prefix(8))")
        defer { try? FileManager.default.removeItem(at: root) }
        let registry = AppRegistry(paths: AvoPaths(home: root.appending(path: "home"), temporaryDirectory: root.appending(path: "tmp")))
        var info = Avo_V1_AppInfo()
        info.package = "com.nekki.shadowfight3"
        info.label = "Shadow Fight 3"
        #expect(await registry.apply(info).record.isGame == nil)
        info.game = true
        #expect(await registry.apply(info).record.isGame == true)
        info.clearGame()
        #expect(await registry.apply(info).record.isGame == true)
        info.game = false
        #expect(await registry.apply(info).record.isGame == false)
    }

    /// App Shims learn which apps are games from the configuration snapshot.
    @Test func appShimsHearWhichAppsAreGames() async throws {
        var game = FakeBridgeAgent.app("com.nekki.shadowfight3", label: "Shadow Fight 3")
        game.game = true
        let app = FakeBridgeAgent.app("com.android.chrome", label: "Chrome")
        let harness = try Harness(apps: [game, app])
        defer { harness.cleanUp() }
        await harness.core.start()
        let (panel, _) = await harness.connect(.controlPanel)
        try await harness.core.startVM(client: panel)
        #expect(await harness.waitForShims(2))
        #expect(await harness.core.configurationSnapshot().games == ["com.nekki.shadowfight3"])
        #expect(await harness.core.apps().first { $0.package == "com.nekki.shadowfight3" }?.isGame == true)
    }
}

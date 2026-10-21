import AppKit
import AvoKeymap
import AvoKit
import CoreText
import SwiftUI

// Quick Settings: the title bar's slider button opens, under itself, the
// settings of this window whose effect shows at once, in the Library's haze
// look: this app's graphics (as on its page in the Control Panel) with
// Android's frame rate cap, the performance HUD, the key mapping, scrolling
// and Translate Screen. What needs Android to restart or applies elsewhere
// (CPU, memory, renderer, device identity, Android version) stays in the
// Control Panel. Last, where Android's GPS says it is, with the Location
// panel a click away. A click outside the overlay or Esc closes it.

// MARK: - What it shows and changes

/// The overlay's settings as its window has them now.
struct QuickSettingsState: Equatable {
    struct KeyMapping: Equatable {
        var gameMode: Bool
        var hintsVisible: Bool
        var hintsOpacity: Double
    }

    /// The device profile chosen in the Control Panel, when it has a screen.
    struct Phone: Equatable {
        var name: String
        var display: DeviceProfile.Display
    }

    var graphics = ResolvedGraphics(settings: .appsDefault, profile: .apps, reason: .none, changes: .init())
    /// The screen "Phone" gives Android; nil for Avo's own identity, which has none.
    var phone: Phone?
    /// The app has graphics of its own (Use Default Graphics Settings applies).
    var hasOwnGraphics = false
    /// Render scales below this one cannot be used on the window's screen.
    var lowestRenderScale = GraphicsSettings.renderScaleRange.lowerBound
    var fpsLimit = 60
    /// The caps to offer (FrameRates.choices).
    var fpsChoices = VMConfig.fpsChoices
    /// What the Mac's screens make of the cap, when none shows it evenly (FrameRates.screenNote).
    var fpsNote: String?
    var hudLevel = PerformanceHUDLevel.off
    /// nil while the app has no key mapping.
    var keyMapping: KeyMapping?
    var scrollMode = ScrollMode.automatic
    var translating = false
    /// Translate Screen reads the display's frames: none before it opens.
    var canTranslate = false
    /// ScreenTranslationSettings.target: nil follows the Mac's language.
    var translationTarget: String?
}

/// The window the overlay belongs to (AndroidWindowController).
@MainActor
protocol QuickSettingsTarget: AnyObject {
    var quickSettingsState: QuickSettingsState { get }
    /// Apps or Games for this app, keeping what it changed on top.
    func quickSettingsChooseProfile(_ profile: GraphicsProfile)
    /// Saved as this app's own: only what differs from its profile.
    func quickSettingsChangeGraphics(_ change: (inout GraphicsSettings) -> Void)
    func quickSettingsUseDefaultGraphics()
    /// Shows a sharpness while its slider is dragged, before it is saved;
    /// nil shows the saved one again.
    func quickSettingsPreviewSharpness(_ sharpness: Int?)
    /// Android's, for every app.
    func quickSettingsSetFrameRateLimit(_ fps: Int)
    /// This window's, as ⌥⌘P.
    func quickSettingsSetHUDLevel(_ level: PerformanceHUDLevel)
    func quickSettingsSetGameMode(_ on: Bool)
    func quickSettingsSetTranslating(_ on: Bool)
}

@MainActor
final class QuickSettingsModel: ObservableObject {
    @Published private(set) var state = QuickSettingsState()
    /// Translate Into's languages, each once, sorted by name.
    @Published private(set) var languages: [ScreenTranslationController.TargetLanguage] = []
    /// The chosen one as Translate Into lists it; nil for the Mac's language.
    @Published private(set) var chosenLanguage: String?
    /// "English", for "System Language (English)".
    @Published private(set) var systemLanguage = ""
    /// The overlay never grows taller than its screen.
    @Published var maxHeight: CGFloat = 640
    weak var target: (any QuickSettingsTarget)?

    func refresh() {
        guard let new = target?.quickSettingsState, new != state else { return }
        let targetChanged = new.translationTarget != state.translationTarget
        state = new
        if targetChanged { chosenLanguage = TranslationLanguages.chosenMenuIdentifier }
    }

    /// The languages known now, then every one once macOS has said.
    func loadLanguages() {
        showLanguages()
        Task { [weak self] in
            _ = await TranslationLanguages.supported()
            self?.showLanguages()
        }
    }

    private func showLanguages() {
        let menu = TranslationLanguages.menu
        if menu != languages { languages = menu }
        chosenLanguage = TranslationLanguages.chosenMenuIdentifier
        systemLanguage = TranslationLanguages.name(of: TranslationLanguages.systemLanguage)
    }

    /// A change in the window, then what it did.
    func change(_ body: (any QuickSettingsTarget) -> Void) {
        guard let target else { return }
        body(target)
        refresh()
    }

    func changeGraphics(_ change: (inout GraphicsSettings) -> Void) {
        self.change { $0.quickSettingsChangeGraphics(change) }
    }

    // The Window Host's own settings, the same in every window of the app.

    func setHintsVisible(_ on: Bool) {
        KeymapHints.visible = on
        refresh()
    }

    func setHintsOpacity(_ opacity: Double) {
        KeymapHints.opacity = opacity
        refresh()
    }

    func setScrollMode(_ mode: ScrollMode) {
        WheelScrollController.settings.setAppMode(mode)
        refresh()
    }

    /// Every app's (Translate Into).
    func setTranslationTarget(_ identifier: String?) {
        ScreenTranslationSettings.target = identifier
        refresh()
    }
}

// MARK: - The button and its overlay

/// The title bar's Quick Settings button and the overlay it opens under
/// itself: a transient popover, so a click outside it or Esc closes it.
@MainActor
final class QuickSettingsOverlay: NSObject, NSPopoverDelegate {
    static var title: String { String(localized: "Quick Settings", bundle: .windowHost) }

    let button = TitlebarButton(symbol: "slider.horizontal.3", label: QuickSettingsOverlay.title, action: nil)
    private let model = QuickSettingsModel()
    private var popover: NSPopover?
    private var observers: [any NSObjectProtocol] = []
    private var keyMonitor: Any?
    /// When it last closed: the click on the button that closed it must not open it again.
    private var closedAt = -Double.infinity

    init(target: any QuickSettingsTarget) {
        super.init()
        model.target = target
        button.target = self
        button.action = #selector(toggle)
    }

    var isShown: Bool { popover?.isShown == true }

    @objc func toggle() {
        if let popover, popover.isShown {
            popover.performClose(nil)
        } else if ProcessInfo.processInfo.systemUptime - closedAt > 0.3 {
            show()
        }
    }

    /// Something it shows changed in the window (the daemon's configuration, ⌘G, ⌥⌘P).
    func refresh() {
        if isShown { model.refresh() }
    }

    private func show() {
        guard let window = button.window else { return }
        model.refresh()
        model.loadLanguages()
        if let screen = window.screen { model.maxHeight = max(320, screen.visibleFrame.height - 140) }
        let popover = NSPopover()
        popover.behavior = .transient
        popover.appearance = NSAppearance(named: .darkAqua)
        popover.hasFullSizeContent = true
        let host = NSHostingController(rootView: QuickSettingsView(model: model))
        host.sizingOptions = .preferredContentSize
        popover.contentViewController = host
        popover.delegate = self
        self.popover = popover
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: button.isFlipped ? .maxY : .minY)
        // Esc and the keyboard are the overlay's now, never Android's.
        host.view.window?.makeKey()
        showOpen(true)
        for name in [KeymapHints.changed, ScreenTranslationSettings.changed] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.model.refresh() }
            })
        }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            self?.escape(event) == true ? nil : event
        }
    }

    private func escape(_ event: NSEvent) -> Bool {
        guard event.keyCode == 53, // kVK_Escape
              let popover, popover.isShown, event.window === popover.contentViewController?.view.window else { return false }
        popover.performClose(nil)
        return true
    }

    func popoverDidClose(_ notification: Notification) {
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers = []
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
        popover = nil
        closedAt = ProcessInfo.processInfo.systemUptime
        // A sharpness still being dragged goes back to the saved one.
        model.target?.quickSettingsPreviewSharpness(nil)
        showOpen(false)
    }

    private func showOpen(_ open: Bool) {
        button.state = open ? .on : .off
        button.contentTintColor = open ? .controlAccentColor : nil
    }
}

// MARK: - The overlay

private func L(_ key: String.LocalizationValue) -> String { String(localized: key, bundle: .windowHost) }

struct QuickSettingsView: View {
    @ObservedObject var model: QuickSettingsModel

    var body: some View {
        let state = model.state
        ScrollView(.vertical) {
            VStack(alignment: .leading, spacing: 8) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(verbatim: QuickSettingsOverlay.title)
                        .font(QuickHaze.hubot(15, 700))
                        .foregroundStyle(QuickHaze.text)
                        .accessibilityAddTraits(.isHeader)
                    Text(verbatim: L("Changes apply right away."))
                        .font(QuickHaze.mona(11.5))
                        .foregroundStyle(QuickHaze.textTertiary)
                }
                .padding(.horizontal, 4)
                .padding(.bottom, 2)
                QuickGraphicsCard(model: model)
                QuickGroup(id: "hud", title: L("Performance HUD"), scope: L("This window"), summary: Self.hudName(state.hudLevel)) {
                    QuickSegmented(label: L("Performance HUD"),
                                   options: [(PerformanceHUDLevel.off, L("Off")), (.fps, L("FPS Only")), (.detailed, L("Detailed"))],
                                   selection: state.hudLevel, fill: true) { level in
                        model.change { $0.quickSettingsSetHUDLevel(level) }
                    }
                }
                if let keys = state.keyMapping { keyMappingCard(keys) }
                QuickGroup(id: "scrolling", title: L("Scrolling"), scope: L("This app"), summary: ScrollModeMenu.name(of: state.scrollMode)) {
                    QuickSegmented(label: L("Scrolling"), options: ScrollMode.allCases.map { ($0, ScrollModeMenu.name(of: $0)) },
                                   selection: state.scrollMode, fill: true, help: { ScrollModeMenu.explanation(of: $0) }) { mode in
                        model.setScrollMode(mode)
                    }
                }
                translationCard(state)
                // Android's GPS: this Mac, a place or a route (LocationPanel.swift).
                QuickLocationCard()
            }
            .padding(12)
        }
        .scrollIndicators(.automatic)
        .scrollBounceBehavior(.basedOnSize)
        .frame(width: 330)
        .frame(maxHeight: model.maxHeight)
        .background { QuickHaze.background.ignoresSafeArea() }
        .environment(\.colorScheme, .dark)
    }

    private static func hudName(_ level: PerformanceHUDLevel) -> String {
        switch level {
        case .off: L("Off")
        case .fps: L("FPS Only")
        case .detailed: L("Detailed")
        }
    }

    private func keyMappingCard(_ keys: QuickSettingsState.KeyMapping) -> some View {
        let percent = Text(keys.hintsOpacity, format: .percent.precision(.fractionLength(0)))
        return QuickGroup(id: "keyMapping", title: L("Key mapping"), scope: L("This app"),
                          summary: keys.gameMode ? L("Game Mode") : L("Off")) {
            QuickToggle(label: L("Game Mode"), shortcut: "⌘G", caption: L("Keys and clicks become touches"), isOn: keys.gameMode) { on in
                model.change { $0.quickSettingsSetGameMode(on) }
            }
            QuickToggle(label: L("Show key hints"), isOn: keys.hintsVisible) { model.setHintsVisible($0) }
            QuickSliderRow(label: L("Hint visibility"), value: percent) {
                QuickSlider(label: L("Hint visibility"), value: Binding(get: { keys.hintsOpacity }, set: { model.setHintsOpacity($0) }),
                            range: KeymapHints.opacityRange, step: 0.05, valueText: percent)
            }
            .disabled(!keys.hintsVisible)
            .opacity(keys.hintsVisible ? 1 : 0.5)
        }
    }

    /// "System Language (English)", then Translate Into's languages.
    private var targetOptions: [QuickMenuOption<String?>] {
        let system = QuickMenuOption<String?>(value: nil, title: L("System Language (\(model.systemLanguage))"))
        return [system] + model.languages.map { QuickMenuOption<String?>(value: $0.identifier, title: $0.name) }
    }

    /// The language translated into, chosen or the Mac's.
    private var targetTitle: String {
        let chosen = model.chosenLanguage.flatMap { id in model.languages.first { $0.identifier == id } }
        return chosen?.name ?? model.systemLanguage
    }

    private func translationCard(_ state: QuickSettingsState) -> some View {
        QuickGroup(id: "translation", title: L("Translation"), scope: L("This window"), summary: state.translating ? targetTitle : L("Off")) {
            QuickToggle(label: L("Translate screen"), shortcut: "⌘⇧T", isOn: state.translating) { on in
                model.change { $0.quickSettingsSetTranslating(on) }
            }
            .disabled(!state.canTranslate && !state.translating)
            .opacity(state.canTranslate || state.translating ? 1 : 0.5)
            QuickRow(label: L("Translate into"), caption: L("Every app")) {
                QuickMenu(label: L("Translate into"), title: targetTitle, options: targetOptions,
                          selection: model.chosenLanguage) { model.setTranslationTarget($0) }
            }
        }
    }
}

/// This app's graphics, as on its page in the Control Panel, and Android's frame rate cap.
private struct QuickGraphicsCard: View {
    @ObservedObject var model: QuickSettingsModel
    /// The sharpness while its slider is dragged (saved when let go).
    @State private var sharpnessDraft: Double?

    var body: some View {
        let state = model.state
        let settings = state.graphics.settings
        QuickGroup(id: "graphics", title: L("Graphics"), scope: L("This app"),
                   summary: "\(state.graphics.profile == .games ? L("Games") : L("Apps")) · \(state.fpsLimit) fps") {
            QuickRow(label: L("Profile")) {
                QuickSegmented(label: L("Profile"), options: [(GraphicsProfile.apps, L("Apps")), (.games, L("Games"))],
                               selection: state.graphics.profile, fill: true) { choice in
                    model.change { $0.quickSettingsChooseProfile(choice) }
                }
                .frame(width: 150)
            }
            screenRows(settings, state: state)
            QuickToggle(label: L("MetalFX upscaling"), isOn: settings.filter == .metalFXSpatial) { on in
                model.changeGraphics { $0.filter = on ? .metalFXSpatial : .bilinear }
            }
            sharpness(settings)
            VStack(alignment: .leading, spacing: 4) {
                QuickRow(label: L("Frame rate cap"), caption: L("Every app")) {
                    // 90 and 120 always, and a screen's own rate (75 Hz); 30 saves power.
                    QuickSegmented(label: L("Frame rate cap"),
                                   options: state.fpsChoices.map { ($0, "\($0)") },
                                   selection: state.fpsLimit) { fps in
                        model.change { $0.quickSettingsSetFrameRateLimit(fps) }
                    }
                    .fixedSize()
                }
                if let note = state.fpsNote { QuickCaption(note) }
            }
            if state.hasOwnGraphics {
                Button {
                    model.change { $0.quickSettingsUseDefaultGraphics() }
                } label: {
                    Text(verbatim: L("Use Default Graphics Settings"))
                        .font(QuickHaze.mona(12.5, 600))
                        .foregroundStyle(QuickHaze.accent)
                }
                .buttonStyle(.plain)
            }
        }
    }

    /// How Android's screen is sized, that mode's own row, its shape and its orientation.
    @ViewBuilder private func screenRows(_ settings: GraphicsSettings, state: QuickSettingsState) -> some View {
        let phone = state.phone
        let mode = settings.effectiveMode(device: phone?.display)
        VStack(alignment: .leading, spacing: 4) {
            QuickRow(label: L("Screen")) {
                QuickSegmented(label: L("Screen"),
                               options: [(GraphicsSettings.Mode.followWindow, L("Window")), (.fixed, L("Fixed")), (.deviceProfile, L("Phone"))],
                               selection: mode, fill: true, help: { choice in
                    switch choice {
                    case .followWindow: L("Android renders at the window’s size.")
                    case .fixed: L("Android keeps one resolution, scaled to the window.")
                    case .deviceProfile: phone == nil ? L("Choose a phone in Avo’s Settings › Privacy & Security › Device profile.")
                                                      : L("Android gets the phone’s screen, so apps lay out as on the phone.")
                    }
                }, enabled: { $0 != .deviceProfile || phone != nil }) { choice in
                    model.changeGraphics { $0.mode = choice }
                }
                .frame(width: 190)
            }
            if let phone, mode == .deviceProfile {
                let display = phone.display
                QuickCaption("\(phone.name) · \(String(display.width)) × \(String(display.height)) · \(String(display.dpi)) dpi")
            } else if settings.mode == .deviceProfile {
                QuickCaption(L("No phone is chosen in Avo’s Settings › Privacy & Security, so Android follows the window."))
            }
        }
        switch mode {
        case .followWindow: renderScaleRow(settings, lowest: state.lowestRenderScale)
        case .fixed: resolutionRow(settings)
        case .deviceProfile: EmptyView()
        }
        aspectRatioRow(settings, mode: mode, phone: phone)
        QuickRow(label: L("Orientation")) {
            QuickSegmented(label: L("Orientation"), options: [(VMConfig.Orientation.portrait, L("Portrait")), (.landscape, L("Landscape"))],
                           selection: settings.orientation, fill: true) { orientation in
                model.changeGraphics { $0.orientation = orientation }
            }
            .frame(width: 190)
        }
    }

    private func resolutionRow(_ settings: GraphicsSettings) -> some View {
        let presets = GraphicsSettings.resolutionPresets
        // A custom resolution (the View menu's Custom…, the Control Panel) is listed too.
        let values = presets.contains(settings.shortSide) ? presets : (presets + [settings.shortSide]).sorted()
        let options = values.map { QuickMenuOption(value: $0, title: GraphicsSettings.resolutionName($0)) }
        return QuickRow(label: L("Resolution")) {
            QuickMenu(label: L("Resolution"), title: GraphicsSettings.resolutionName(settings.shortSide), options: options,
                      selection: settings.shortSide) { pixels in
                model.changeGraphics {
                    $0.mode = .fixed
                    $0.shortSide = pixels
                }
            }
        }
    }

    /// The current mode's shape; the phone's screen has the phone's.
    @ViewBuilder private func aspectRatioRow(_ settings: GraphicsSettings, mode: GraphicsSettings.Mode,
                                             phone: QuickSettingsState.Phone?) -> some View {
        if mode == .deviceProfile, let phone {
            QuickRow(label: L("Aspect ratio")) {
                QuickValue(Text(verbatim: phone.display.ratioName))
            }
        } else {
            let current = settings.aspectRatio(for: mode)
            let title = { (ratio: GraphicsSettings.AspectRatio) in ratio == .matchWindow ? L("Match window") : ratio.rawValue }
            QuickRow(label: L("Aspect ratio")) {
                QuickMenu(label: L("Aspect ratio"), title: title(current),
                          options: GraphicsSettings.AspectRatio.allCases.map { QuickMenuOption(value: $0, title: title($0)) },
                          selection: current) { ratio in
                    model.changeGraphics { $0.setAspectRatio(ratio, for: mode) }
                }
            }
        }
    }

    private func renderScaleRow(_ settings: GraphicsSettings, lowest: Int) -> some View {
        let options = GraphicsSettings.RenderScalePreset.allCases.map { preset in
            let title = GraphicsMenu.title(preset)
            // Android's density cannot go below 120 dpi, which sets a floor on this screen.
            return preset.rawValue < lowest ? QuickMenuOption(value: preset.rawValue, title: L("\(title) — not on this display"), enabled: false)
                                            : QuickMenuOption(value: preset.rawValue, title: title)
        }
        let title = GraphicsSettings.RenderScalePreset(rawValue: settings.renderScale).map(GraphicsMenu.title) ?? "\(settings.renderScale)%"
        return QuickRow(label: L("Render scale")) {
            QuickMenu(label: L("Render scale"), title: title, options: options, selection: settings.renderScale) { percent in
                model.changeGraphics {
                    $0.mode = .followWindow
                    $0.renderScale = percent
                }
            }
        }
    }

    private func sharpness(_ settings: GraphicsSettings) -> some View {
        let shown = Int((sharpnessDraft ?? Double(settings.sharpness)).rounded())
        let value = shown == 0 ? Text(verbatim: L("Off")) : Text(Double(shown) / 100, format: .percent)
        let enabled = settings.filter != .nearest
        let range = Double(GraphicsSettings.sharpnessRange.lowerBound)...Double(GraphicsSettings.sharpnessRange.upperBound)
        return QuickSliderRow(label: L("Sharpness"), value: value) {
            QuickSlider(label: L("Sharpness"), value: Binding(get: { sharpnessDraft ?? Double(settings.sharpness) }, set: { newValue in
                if sharpnessDraft == nil {
                    // A key or VoiceOver step: saved at once.
                    commitSharpness(Int(newValue.rounded()), saved: settings.sharpness)
                } else {
                    sharpnessDraft = newValue
                    model.target?.quickSettingsPreviewSharpness(Int(newValue.rounded()))
                }
            }), range: range, step: Double(GraphicsSettings.sharpnessStep), valueText: value) { editing in
                if editing {
                    sharpnessDraft = Double(settings.sharpness)
                } else if let draft = sharpnessDraft {
                    sharpnessDraft = nil
                    commitSharpness(Int(draft.rounded()), saved: settings.sharpness)
                }
            }
        }
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.5)
    }

    private func commitSharpness(_ value: Int, saved: Int) {
        guard value != saved else {
            model.target?.quickSettingsPreviewSharpness(nil)
            return
        }
        model.changeGraphics { $0.sharpness = value }
    }
}

// MARK: - The haze look, in small

/// The Control Panel's haze (HazeTheme.swift in Avo.app), the few colours,
/// type and controls the overlay uses (and the Location panel, LocationPanel.swift).
@MainActor
enum QuickHaze {
    static func rgb(_ value: UInt32, _ alpha: Double = 1) -> Color {
        Color(.sRGB, red: Double((value >> 16) & 0xFF) / 255, green: Double((value >> 8) & 0xFF) / 255,
              blue: Double(value & 0xFF) / 255, opacity: alpha)
    }

    static let base = rgb(0x0A1108)
    static let card = rgb(0xF3F1E8, 0.045)
    static let inset = Color.black.opacity(0.28)
    static let text = rgb(0xF3F1E8)
    static let textSecondary = rgb(0xC5CDB9)
    static let textTertiary = rgb(0x9AA58E)
    static let sectionLabel = rgb(0x8F9B84)
    static let accent = rgb(0xD8E8A7)
    static let onAccent = rgb(0x15210E)
    static let accentText = rgb(0xEEF5D6)
    static let segmentSelection = rgb(0xD8E8A7, 0.2)

    /// Near black with a breath of Avo green top right and bottom left.
    static var background: some View {
        ZStack {
            base
            EllipticalGradient(colors: [rgb(0xD8E8A7, 0.2), rgb(0xD8E8A7, 0)], center: UnitPoint(x: 0.92, y: -0.04),
                               startRadiusFraction: 0, endRadiusFraction: 0.75)
            EllipticalGradient(colors: [rgb(0xBCDC8F, 0.09), rgb(0xBCDC8F, 0)], center: UnitPoint(x: 0.06, y: 1.04),
                               startRadiusFraction: 0, endRadiusFraction: 0.7)
        }
    }

    // Hubot Sans and Mona Sans from Avo.app (Fonts/), else the system font.

    private enum Family: String {
        case hubot = "hubot-sans"
        case mona = "mona-sans"
    }

    private static var bases: [Family: CTFontDescriptor?] = [:]
    private static var fonts: [String: Font] = [:]

    static func hubot(_ size: CGFloat, _ weight: CGFloat = 700) -> Font { font(.hubot, size, weight) }
    static func mona(_ size: CGFloat, _ weight: CGFloat = 400) -> Font { font(.mona, size, weight) }

    private static func font(_ family: Family, _ size: CGFloat, _ weight: CGFloat) -> Font {
        let key = "\(family.rawValue)-\(size)-\(weight)"
        if let font = fonts[key] { return font }
        let font: Font
        if let base = base(family) {
            var descriptor = CTFontDescriptorCreateCopyWithVariation(base, tag("wght") as CFNumber, weight)
            descriptor = CTFontDescriptorCreateCopyWithVariation(descriptor, tag("wdth") as CFNumber, 100)
            font = Font(CTFontCreateWithFontDescriptor(descriptor, size, nil))
        } else {
            font = .system(size: size, weight: weight >= 650 ? .bold : weight >= 550 ? .semibold : .regular)
        }
        fonts[key] = font
        return font
    }

    private static func base(_ family: Family) -> CTFontDescriptor? {
        if let known = bases[family] { return known }
        // The framework is Avo.app/Contents/Frameworks/AvoWindowHost.framework.
        let resources = Bundle.windowHost.bundleURL.deletingLastPathComponent().deletingLastPathComponent().appending(path: "Resources")
        let candidates = [resources, AndroidWindowController.hostApplication?.appending(path: "Contents/Resources")].compactMap { $0 }
        let descriptor = candidates.lazy
            .compactMap { try? Data(contentsOf: $0.appending(path: "\(family.rawValue).woff2")) }
            .compactMap { (CTFontManagerCreateFontDescriptorsFromData($0 as CFData) as? [CTFontDescriptor])?.first }
            .first
        bases[family] = .some(descriptor)
        return descriptor
    }

    private static func tag(_ name: String) -> Int {
        name.utf8.reduce(0) { $0 << 8 | Int($1) }
    }
}

/// A card of Quick Settings that folds, so the overlay opens short: its
/// title and what it is set to; opened, its settings. Each app's windows
/// remember which are open.
struct QuickGroup<Content: View>: View {
    let title: String
    let scope: String
    /// What it is set to, shown while it is folded.
    let summary: String
    let content: Content
    @AppStorage private var isOpen: Bool

    init(id: String, title: String, scope: String, summary: String = "", @ViewBuilder content: () -> Content) {
        self.title = title
        self.scope = scope
        self.summary = summary
        self.content = content()
        _isOpen = AppStorage(wrappedValue: false, "QuickSettingsOpen.\(id)")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Button {
                withAnimation(.snappy(duration: 0.22)) { isOpen.toggle() }
            } label: {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 10, weight: .bold))
                        .foregroundStyle(QuickHaze.textTertiary)
                        .rotationEffect(.degrees(isOpen ? 90 : 0))
                    Text(verbatim: title)
                        .font(QuickHaze.hubot(13.5, 650))
                        .foregroundStyle(QuickHaze.text)
                    Spacer(minLength: 8)
                    Text(verbatim: isOpen || summary.isEmpty ? scope : summary)
                        .font(QuickHaze.mona(11, 500))
                        .foregroundStyle(QuickHaze.sectionLabel)
                        .lineLimit(1)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityAddTraits(.isHeader)
            .accessibilityValue(isOpen ? L("Open") : summary)
            if isOpen {
                content.transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(QuickHaze.card, in: RoundedRectangle(cornerRadius: 13, style: .continuous))
        .clipShape(RoundedRectangle(cornerRadius: 13, style: .continuous))
        .accessibilityElement(children: .contain)
    }
}

/// A card: its title, whom it applies to, and its settings.
struct QuickCard<Content: View>: View {
    let title: String
    let scope: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: title)
                    .font(QuickHaze.hubot(13.5, 650))
                    .foregroundStyle(QuickHaze.text)
                    .accessibilityAddTraits(.isHeader)
                Spacer(minLength: 8)
                Text(verbatim: scope)
                    .font(QuickHaze.mona(11, 500))
                    .foregroundStyle(QuickHaze.sectionLabel)
            }
            content
        }
        .padding(11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(QuickHaze.card, in: RoundedRectangle(cornerRadius: 13, style: .continuous))
        .accessibilityElement(children: .contain)
    }
}

struct QuickText: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(verbatim: text).font(QuickHaze.mona(13.5)).foregroundStyle(QuickHaze.text)
    }
}

struct QuickCaption: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(verbatim: text)
            .font(QuickHaze.mona(11.5))
            .foregroundStyle(QuickHaze.textTertiary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// A setting's value at the end of its row ("35%").
struct QuickValue: View {
    let text: Text
    init(_ text: Text) { self.text = text }

    var body: some View {
        text.font(QuickHaze.mona(12.5)).foregroundStyle(QuickHaze.textTertiary).monospacedDigit()
    }
}

/// A label (and a caption under it) with its control at the end of the row.
struct QuickRow<Control: View>: View {
    let label: String
    var caption: String?
    @ViewBuilder let control: Control

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                QuickText(label)
                if let caption { QuickCaption(caption) }
            }
            Spacer(minLength: 6)
            control
        }
    }
}

/// A label, its slider and the value, on one line.
struct QuickSliderRow<Slider: View>: View {
    let label: String
    let value: Text
    @ViewBuilder let slider: Slider

    var body: some View {
        HStack(spacing: 10) {
            QuickText(label).fixedSize()
            slider.frame(maxWidth: .infinity)
            QuickValue(value).frame(minWidth: 34, alignment: .trailing)
        }
    }
}

/// The haze checkbox (an Avo green square with a dark check) at the end of its row.
struct QuickToggle: View {
    let label: String
    /// Its key in the window ("⌘G"), after the label.
    var shortcut: String?
    var caption: String?
    let isOn: Bool
    let set: (Bool) -> Void

    var body: some View {
        Button {
            set(!isOn)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 1) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        QuickText(label)
                        if let shortcut {
                            Text(verbatim: shortcut).font(QuickHaze.mona(11.5)).foregroundStyle(QuickHaze.textTertiary)
                        }
                    }
                    if let caption { QuickCaption(caption) }
                }
                Spacer(minLength: 8)
                ZStack {
                    RoundedRectangle(cornerRadius: 4, style: .continuous)
                        .fill(isOn ? QuickHaze.accent : Color.white.opacity(0.92))
                        .frame(width: 18, height: 18)
                    if isOn {
                        Image(systemName: "checkmark")
                            .font(.system(size: 11, weight: .heavy))
                            .foregroundStyle(QuickHaze.onAccent)
                    }
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(Text(verbatim: label))
        .accessibilityValue(Text(verbatim: isOn ? L("On") : L("Off")))
        .accessibilityHint(Text(verbatim: caption ?? ""))
        .accessibilityAddTraits(.isToggle)
    }
}

/// The haze segmented control: options in a recessed capsule, the chosen one lit.
struct QuickSegmented<Value: Hashable>: View {
    let label: String
    let options: [(value: Value, title: String)]
    let selection: Value?
    /// Each option as wide as the others, across the row.
    var fill = false
    var segmentPadding: CGFloat = 10
    /// What an option does, under the pointer.
    var help: ((Value) -> String)?
    /// Whether an option can be chosen (all, without it).
    var enabled: ((Value) -> Bool)?
    let choose: (Value) -> Void

    var body: some View {
        HStack(spacing: 2) {
            ForEach(Array(options.enumerated()), id: \.offset) { _, option in
                let selected = option.value == selection
                Button {
                    if !selected { choose(option.value) }
                } label: {
                    Text(verbatim: option.title)
                        .font(QuickHaze.mona(12.5, selected ? 600 : 400))
                        .foregroundStyle(selected ? QuickHaze.accentText : QuickHaze.textSecondary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.8)
                        .padding(.horizontal, fill ? 4 : segmentPadding)
                        .frame(maxWidth: fill ? .infinity : nil)
                        .frame(height: 24)
                        .background {
                            if selected { RoundedRectangle(cornerRadius: 8, style: .continuous).fill(QuickHaze.segmentSelection) }
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(!(enabled?(option.value) ?? true))
                .opacity(enabled?(option.value) ?? true ? 1 : 0.45)
                .help(help?(option.value) ?? "")
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
        }
        .padding(3)
        .background(RoundedRectangle(cornerRadius: 11, style: .continuous).fill(QuickHaze.inset))
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text(verbatim: label))
    }
}

private struct QuickMenuOption<Value: Hashable>: Identifiable {
    let value: Value
    let title: String
    var enabled = true
    var id: String { title }
}

/// A pop-up choice in a recessed capsule (render scale, Translate Into).
private struct QuickMenu<Value: Hashable>: View {
    let label: String
    let title: String
    let options: [QuickMenuOption<Value>]
    let selection: Value
    let choose: (Value) -> Void

    var body: some View {
        Menu {
            ForEach(options) { option in
                Toggle(isOn: Binding(get: { option.value == selection }, set: { _ in
                    if option.value != selection { choose(option.value) }
                })) {
                    Text(verbatim: option.title)
                }
                .disabled(!option.enabled)
            }
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: title)
                    .font(QuickHaze.mona(12.5, 600))
                    .foregroundStyle(QuickHaze.accentText)
                    .lineLimit(1)
                Image(systemName: "chevron.up.chevron.down")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(QuickHaze.textTertiary)
            }
            .padding(.horizontal, 10)
            .frame(height: 28)
            .frame(maxWidth: 170)
            .background(RoundedRectangle(cornerRadius: 9, style: .continuous).fill(QuickHaze.inset))
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel(Text(verbatim: label))
        .accessibilityValue(Text(verbatim: title))
    }
}

/// The haze slider: an Avo green fill and thumb on a dark track. Drag or
/// click anywhere on it; arrow keys and VoiceOver step it.
struct QuickSlider: View {
    let label: String
    @Binding var value: Double
    let range: ClosedRange<Double>
    var step: Double = 1
    /// What VoiceOver reads for the value ("35%").
    var valueText: Text?
    /// Begins and ends a drag (for saving only when it ends).
    var onEditingChanged: (Bool) -> Void = { _ in }
    @State private var dragging = false

    private let thumb: CGFloat = 15

    var body: some View {
        GeometryReader { geometry in
            let width = max(1, geometry.size.width - thumb)
            let fraction = (min(max(value, range.lowerBound), range.upperBound) - range.lowerBound)
                / max(range.upperBound - range.lowerBound, .ulpOfOne)
            ZStack(alignment: .leading) {
                Capsule().fill(Color.black.opacity(0.35))
                    .overlay { Capsule().strokeBorder(QuickHaze.text.opacity(0.12)) }
                    .frame(height: 5)
                    .padding(.horizontal, thumb / 2)
                Capsule().fill(QuickHaze.accent)
                    .frame(width: thumb / 2 + width * fraction, height: 5)
                    .padding(.leading, thumb / 2)
                Circle()
                    .fill(QuickHaze.accent)
                    .overlay { Circle().strokeBorder(QuickHaze.onAccent.opacity(0.25)) }
                    .shadow(color: .black.opacity(0.35), radius: 2, y: 1)
                    .frame(width: thumb, height: thumb)
                    .offset(x: width * fraction)
            }
            .frame(height: geometry.size.height)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0)
                .onChanged { drag in
                    if !dragging {
                        dragging = true
                        onEditingChanged(true)
                    }
                    set(fraction: (drag.location.x - thumb / 2) / width)
                }
                .onEnded { _ in
                    dragging = false
                    onEditingChanged(false)
                })
        }
        .frame(height: 18)
        .accessibilityElement()
        .accessibilityLabel(Text(verbatim: label))
        .accessibilityValue(valueText ?? Text(value, format: .number))
        .accessibilityAdjustableAction { direction in
            switch direction {
            case .increment: value = min(range.upperBound, value + step)
            case .decrement: value = max(range.lowerBound, value - step)
            @unknown default: break
            }
        }
    }

    private func set(fraction: CGFloat) {
        let raw = range.lowerBound + Double(min(max(fraction, 0), 1)) * (range.upperBound - range.lowerBound)
        let snapped = ((raw - range.lowerBound) / step).rounded() * step + range.lowerBound
        let clamped = min(max(snapped, range.lowerBound), range.upperBound)
        if abs(clamped - value) > step / 1000 { value = clamped }
    }
}

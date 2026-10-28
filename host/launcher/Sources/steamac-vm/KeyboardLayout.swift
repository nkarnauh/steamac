import Carbon
import Combine
import Foundation

/// Settings > General "Use the Mac's keyboard layouts in Desktop Mode": the Mac's enabled
/// keyboard layouts (in macOS's order) and the one selected now go to the progress agent as
/// XKB layouts, `keyboard-layout <active index> <layout[(variant)]>...`, which has the nested
/// Plasma session use them (/usr/lib/steamac/desktop-keyboard: kxkbrc, KWin over D-Bus). Sent
/// on every input source change on the Mac and whenever Desktop Mode gets the focus (Plasma
/// starts without them). The keys stay the Mac's physical keys either way (evdev codes); the
/// layout decides what they type. Main thread only.
final class KeyboardLayoutSync {
    private let settings: LauncherSettings
    private let send: (String) -> Bool
    private var last: String?
    private var observers: [NSObjectProtocol] = []
    private var subscription: AnyCancellable?

    init(settings: LauncherSettings, send: @escaping (String) -> Bool) {
        self.settings = settings
        self.send = send
    }

    func start() {
        let dnc = DistributedNotificationCenter.default()
        for name in [kTISNotifySelectedKeyboardInputSourceChanged, kTISNotifyEnabledKeyboardInputSourcesChanged].compactMap({ $0 }) {
            observers.append(dnc.addObserver(forName: Notification.Name(name as String), object: nil, queue: .main) { [weak self] _ in
                self?.push(force: false)
            })
        }
        // @Published emits before the property changes.
        subscription = settings.$followMacKeyboard.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in
            self?.push(force: true)
        }
    }

    /// Desktop Mode got the focus: its Plasma may have just started.
    func desktopFocused() {
        push(force: true)
    }

    private func push(force: Bool) {
        guard settings.followMacKeyboard, let line = KeyboardLayoutSync.request() else { return }
        guard force || line != last else { return }
        if send(line) {
            if line != last { log("keyboard: \(line)") }
            last = line
        }
    }

    /// `keyboard-layout …` for the Mac's layouts now, nil without any.
    static func request() -> String? {
        let enabled = enabledLayouts()
        guard let current = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue() else { return nil }
        var specs: [String] = []
        for s in enabled {
            let spec = xkb(for: s)
            if !specs.contains(spec) { specs.append(spec) }
        }
        let active = xkb(for: current)
        if !specs.contains(active) { specs.append(active) }
        guard let index = specs.firstIndex(of: active) else { return nil }
        return "keyboard-layout \(index) " + specs.joined(separator: " ")
    }

    /// The enabled keyboard layouts (not input methods: their layout is the current one when
    /// they are selected), in the order of the Input menu.
    private static func enabledLayouts() -> [TISInputSource] {
        let filter = [kTISPropertyInputSourceCategory: kTISCategoryKeyboardInputSource,
                      kTISPropertyInputSourceType: kTISTypeKeyboardLayout] as CFDictionary
        guard let list = TISCreateInputSourceList(filter, false)?.takeRetainedValue() as? [TISInputSource] else { return [] }
        return list
    }

    private static func property(_ s: TISInputSource, _ key: CFString) -> AnyObject? {
        guard let p = TISGetInputSourceProperty(s, key) else { return nil }
        return Unmanaged<AnyObject>.fromOpaque(p).takeUnretainedValue()
    }

    static func xkb(for s: TISInputSource) -> String {
        let id = property(s, kTISPropertyInputSourceID) as? String ?? ""
        let languages = property(s, kTISPropertyInputSourceLanguages) as? [String] ?? []
        return xkb(id: id, languages: languages)
    }

    /// macOS keyboard layout → XKB `layout` or `layout(variant)`: the Mac layouts that XKB has
    /// as a variant by name, else the layout of the source's first language, else `us`.
    static func xkb(id: String, languages: [String]) -> String {
        let name = id.hasPrefix("com.apple.keylayout.") ? String(id.dropFirst("com.apple.keylayout.".count)) : ""
        if let spec = byName[name] { return spec }
        if let lang = languages.first.map({ String($0.prefix { $0 != "-" && $0 != "_" }) }), !lang.isEmpty {
            return byLanguage[lang] ?? (lang.count == 2 && lang.allSatisfy(\.isLetter) ? lang.lowercased() : "us")
        }
        return "us"
    }

    private static let byName: [String: String] = [
        "US": "us", "ABC": "us", "USExtended": "us", "Australian": "us", "Canadian": "us",
        "USInternational-PC": "us(intl)", "Dvorak": "us(dvorak)", "Colemak": "us(colemak)",
        "British": "gb(mac)", "British-PC": "gb", "Irish": "ie",
        "Russian": "ru(mac)", "RussianWin": "ru(winkeys)", "Russian-Phonetic": "ru(phonetic_mac)",
        "Ukrainian": "ua(macOS)", "Ukrainian-PC": "ua(winkeys)", "Byelorussian": "by",
        "German": "de(mac)", "Austrian": "at(mac)", "SwissGerman": "ch(de_mac)", "SwissFrench": "ch(fr_mac)",
        "French": "fr(mac)", "French-PC": "fr", "Belgian": "be", "Spanish": "es(mac)", "Spanish-ISO": "es",
        "Italian": "it(mac)", "Italian-Pro": "it", "Portuguese": "pt(mac)", "Brazilian": "br",
        "Swedish": "se(mac)", "Swedish-Pro": "se", "Norwegian": "no(mac)", "Danish": "dk(mac)", "Finnish": "fi(mac)",
        "Dutch": "nl(mac)", "Polish": "pl", "PolishPro": "pl", "Czech": "cz", "Czech-QWERTY": "cz(qwerty)",
        "Slovak": "sk", "Hungarian": "hu", "Turkish": "tr", "Turkish-QWERTY-PC": "tr", "Greek": "gr",
        "Hebrew": "il", "Kazakh": "kz", "Georgian-QWERTY": "ge", "Armenian-HMQWERTY": "am",
    ]

    /// ISO 639-1 language → XKB layout where the codes differ.
    private static let byLanguage: [String: String] = [
        "en": "us", "uk": "ua", "be": "by", "kk": "kz", "he": "il", "el": "gr", "cs": "cz", "da": "dk",
        "sv": "se", "ja": "jp", "ko": "kr", "zh": "cn", "ka": "ge", "hy": "am", "et": "ee", "sl": "si",
        "sr": "rs", "fa": "ir", "ar": "ara", "hi": "in", "vi": "vn", "nb": "no", "nn": "no",
    ]

    /// SettingsSelfTest: the mapping of a few layouts.
    static func selfCheck() -> [String] {
        var problems: [String] = []
        for (id, languages, want) in [("com.apple.keylayout.ABC", ["en"], "us"),
                                      ("com.apple.keylayout.Russian", ["ru"], "ru(mac)"),
                                      ("com.apple.keylayout.RussianWin", ["ru"], "ru(winkeys)"),
                                      ("com.apple.keylayout.Hawaiian", ["haw", "en"], "us"),
                                      ("com.apple.keylayout.Lithuanian", ["lt"], "lt"),
                                      ("com.apple.keylayout.Ukrainian-QWERTY", ["uk"], "ua")] {
            let got = xkb(id: id, languages: languages)
            if got != want { problems.append("keyboard layout \(id): \(got), expected \(want)") }
        }
        return problems
    }
}

// C ABI: optic_run(callback) blocks in the macOS app loop; callbacks are
// 1 = toggle buffering, 2 = save buffer, 3 = quit. Callback result is ignored
// for 1/2; for 3, false defers termination until optic_finish_quit() is called.
// optic_set_status(state, message) copies message before asynchronously updating
// UI: 0 stopped, 1 starting, 2 buffering, 3 stopping. Callbacks run on main.

import AppKit
import Carbon

private typealias OpticActionCallback = @convention(c) (Int32) -> Bool
private var opticApplicationDelegate: OpticApplicationDelegate?

public typealias OpticErrorCallback = @convention(c) (
    UnsafePointer<CChar>?, UnsafePointer<CChar>?, UnsafePointer<CChar>?
) -> Void

// Registration occurs before native operations. The callback remains valid for
// the process lifetime; the lock also makes diagnostic calls safe on workers.
private final class NativeErrorLogger: @unchecked Sendable {
    private let lock = NSLock()
    private var callback: OpticErrorCallback?

    func register(_ callback: @escaping OpticErrorCallback) {
        lock.lock()
        if self.callback == nil { self.callback = callback }
        lock.unlock()
    }

    func error(component: String, event: String, message: String) {
        lock.lock()
        let callback = self.callback
        lock.unlock()
        if let callback {
            component.withCString { componentPointer in
                event.withCString { eventPointer in
                    message.withCString { callback(componentPointer, eventPointer, $0) }
                }
            }
        } else {
            let diagnostic = "[optic] ERROR \(component) \(event): \(message)\n"
            try? FileHandle.standardError.write(contentsOf: Data(diagnostic.utf8))
        }
    }
}

private let nativeErrorLogger = NativeErrorLogger()

@_cdecl("optic_set_error_callback")
public func opticSetErrorCallback(_ callback: @escaping OpticErrorCallback) {
    nativeErrorLogger.register(callback)
}

@_cdecl("optic_run")
public func opticRun(_ callback: @convention(c) (Int32) -> Bool) {
    let application = NSApplication.shared
    let delegate = OpticApplicationDelegate(callback: callback)
    opticApplicationDelegate = delegate
    application.delegate = delegate
    application.setActivationPolicy(.accessory)
    application.run()
    opticApplicationDelegate = nil
}

@_cdecl("optic_set_status")
public func opticSetStatus(_ state: Int32, _ message: UnsafePointer<CChar>?) {
    let copiedMessage = message.map { String(cString: $0) } ?? ""
    DispatchQueue.main.async {
        opticApplicationDelegate?.setStatus(state, message: copiedMessage)
    }
}

@_cdecl("optic_finish_quit")
public func opticFinishQuit() {
    // AppKit can run a nested modal loop while termination is deferred. A
    // run-loop block also runs when the original quit came from a GCD block.
    RunLoop.main.perform(inModes: [.default, .modalPanel, .eventTracking]) {
        opticApplicationDelegate?.finishQuit()
    }
    CFRunLoopWakeUp(CFRunLoopGetMain())
}

private final class OpticApplicationDelegate: NSObject, NSApplicationDelegate {
    private let callback: OpticActionCallback
    private var state: Int32 = 0
    private var message = "Stopped"
    private var isQuitting = false
    private var awaitingDeferredTermination = false
    private var statusItem: NSStatusItem?
    private let menu = NSMenu()
    private let statusMenuItem = NSMenuItem(title: "Stopped", action: nil, keyEquivalent: "")
    private var toggleMenuItem: NSMenuItem!
    private var saveMenuItem: NSMenuItem!
    private var quitMenuItem: NSMenuItem!
    private var hotKeyHandler: EventHandlerRef?
    private var toggleHotKey: EventHotKeyRef?
    private var saveHotKey: EventHotKeyRef?
    private let hotKeySignature: OSType = 0x4F505443 // OPTC

    init(callback: @escaping OpticActionCallback) {
        self.callback = callback
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        menu.autoenablesItems = false
        statusMenuItem.isEnabled = false
        menu.addItem(statusMenuItem)
        menu.addItem(.separator())

        toggleMenuItem = menuItem("Start Buffering", action: #selector(toggleBuffering), key: "b")
        toggleMenuItem.keyEquivalentModifierMask = [.control, .option]
        menu.addItem(toggleMenuItem)

        saveMenuItem = menuItem("Save Replay", action: #selector(saveReplay), key: "s")
        saveMenuItem.keyEquivalentModifierMask = [.control, .option]
        menu.addItem(saveMenuItem)
        menu.addItem(.separator())

        menu.addItem(menuItem("Open Recordings Folder", action: #selector(openRecordingsFolder)))
        menu.addItem(.separator())
        quitMenuItem = menuItem("Quit Optic", action: #selector(quit), key: "q")
        menu.addItem(quitMenuItem)

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        item.menu = menu
        statusItem = item
        updateMenu()
        registerHotKeys()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if isQuitting {
            return awaitingDeferredTermination ? .terminateLater : .terminateNow
        }
        isQuitting = true
        if callback(3) {
            return .terminateNow
        }
        awaitingDeferredTermination = true
        message = "Finishing saved clip…"
        updateMenu()
        return .terminateLater
    }

    func finishQuit() {
        guard awaitingDeferredTermination else { return }
        awaitingDeferredTermination = false
        NSApplication.shared.reply(toApplicationShouldTerminate: true)
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let key = toggleHotKey { UnregisterEventHotKey(key) }
        if let key = saveHotKey { UnregisterEventHotKey(key) }
        if let handler = hotKeyHandler { RemoveEventHandler(handler) }
    }

    func setStatus(_ newState: Int32, message newMessage: String) {
        guard !isQuitting else { return }
        state = newState
        message = newMessage.isEmpty ? stateName : newMessage
        updateMenu()
    }

    private var stateName: String {
        switch state {
        case 1: return "Starting…"
        case 2: return "Buffering"
        case 3: return "Stopping…"
        default: return "Stopped"
        }
    }

    private func updateMenu() {
        statusMenuItem.title = message
        guard toggleMenuItem != nil else { return }
        toggleMenuItem.title = state == 0 ? "Start Buffering" : state == 3 ? "Stopping…" : "Stop Buffering"
        toggleMenuItem.isEnabled = !isQuitting && (state == 0 || state == 1 || state == 2)
        saveMenuItem.isEnabled = !isQuitting && state == 2
        quitMenuItem.isEnabled = !isQuitting
        let symbol = state == 2 && !isQuitting ? "record.circle.fill" : "record.circle"
        let description = isQuitting ? message : stateName
        let image = NSImage(systemSymbolName: symbol, accessibilityDescription: "Optic: \(description)")
        image?.isTemplate = true
        statusItem?.button?.image = image
        statusItem?.button?.toolTip = "Optic: \(message)"
    }

    private func menuItem(_ title: String, action: Selector, key: String = "") -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.target = self
        return item
    }

    @objc private func toggleBuffering() {
        guard state == 0 || state == 1 || state == 2, !isQuitting else { return }
        state = state == 0 ? 1 : 3
        message = stateName
        updateMenu()
        _ = callback(1)
    }

    @objc private func saveReplay() {
        guard state == 2, !isQuitting else { return }
        _ = callback(2)
    }

    @objc private func openRecordingsFolder() {
        let folder = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Movies", isDirectory: true)
            .appendingPathComponent("Optic", isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            if !NSWorkspace.shared.open(folder) {
                nativeErrorLogger.error(component: "ui", event: "recordings_folder.open_failed",
                                        message: "Could not open the recordings folder.")
            }
        } catch {
            let failure = error as NSError
            nativeErrorLogger.error(component: "ui", event: "recordings_folder.create_failed",
                                    message: "Could not create the recordings folder (\(failure.domain) code \(failure.code)).")
        }
    }

    @objc private func quit() {
        guard !isQuitting else { return }
        NSApplication.shared.terminate(nil)
    }

    private func registerHotKeys() {
        var eventType = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let handlerStatus = InstallEventHandler(
            GetApplicationEventTarget(),
            { _, event, context in
                guard let event = event, let context = context else { return OSStatus(eventNotHandledErr) }
                let owner = Unmanaged<OpticApplicationDelegate>.fromOpaque(context).takeUnretainedValue()
                var identifier = EventHotKeyID()
                let result = GetEventParameter(
                    event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
                    nil, MemoryLayout<EventHotKeyID>.size, nil, &identifier
                )
                guard result == noErr, identifier.signature == owner.hotKeySignature else {
                    return OSStatus(eventNotHandledErr)
                }
                switch identifier.id {
                case 1: owner.toggleBuffering()
                case 2: owner.saveReplay()
                default: return OSStatus(eventNotHandledErr)
                }
                return noErr
            },
            1, &eventType, Unmanaged.passUnretained(self).toOpaque(), &hotKeyHandler
        )
        guard handlerStatus == noErr else {
            showShortcutFailure("Global shortcuts unavailable (error \(handlerStatus))",
                                event: "shortcut.handler_failed")
            return
        }

        let modifiers = UInt32(controlKey | optionKey)
        let toggleStatus = RegisterEventHotKey(
            UInt32(kVK_ANSI_B), modifiers, EventHotKeyID(signature: hotKeySignature, id: 1),
            GetApplicationEventTarget(), 0, &toggleHotKey
        )
        if toggleStatus != noErr {
            showShortcutFailure("Shortcut unavailable: ⌃⌥B (error \(toggleStatus))",
                                event: "shortcut.register_failed")
        }
        let saveStatus = RegisterEventHotKey(
            UInt32(kVK_ANSI_S), modifiers, EventHotKeyID(signature: hotKeySignature, id: 2),
            GetApplicationEventTarget(), 0, &saveHotKey
        )
        if saveStatus != noErr {
            showShortcutFailure("Shortcut unavailable: ⌃⌥S (error \(saveStatus))",
                                event: "shortcut.register_failed")
        }
    }

    private func showShortcutFailure(_ description: String, event: String) {
        let item = NSMenuItem(title: description, action: nil, keyEquivalent: "")
        item.isEnabled = false
        menu.insertItem(item, at: 1)
        nativeErrorLogger.error(component: "ui", event: event, message: description)
    }
}

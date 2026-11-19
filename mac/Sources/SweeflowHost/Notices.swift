import AppKit
import UserNotifications

/// Sweeflow's notifications (macOS's own, in Notification Centre). One of each kind at a time:
/// a newer one of the same kind takes the older one's place. A click runs that kind's handler.
final class Notices: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notices()
    /// What a click on a notification of that kind does. Main thread.
    var onClick: [String: () -> Void] = [:]

    func start() { UNUserNotificationCenter.current().delegate = self }

    func post(_ title: String, _ body: String, kind: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert]) { granted, _ in
            guard granted else { return }
            let c = UNMutableNotificationContent()
            c.title = title
            c.body = body
            center.add(UNNotificationRequest(identifier: kind, content: c, trigger: nil))
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler done: @escaping () -> Void) {
        let kind = response.notification.request.identifier
        DispatchQueue.main.async { self.onClick[kind]?() }
        done()
    }

    // Shown even while Sweeflow is the app in front (its menu open).
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void) {
        done([.banner, .list])
    }
}

/// Says when a Linux computer starts streaming this Mac's apps, pauses and stops, so whoever is
/// at the Mac knows its apps are showing elsewhere. Said once things settle (a few seconds), so a
/// Wi-Fi blip or a quick Pause then Resume says nothing.
final class StreamNotices {
    static let shared = StreamNotices()
    private enum State { case off, streaming, paused }
    private var said = State.off
    private var saidTo = ""
    private var pending: DispatchWorkItem?
    private var observers: [NSObjectProtocol] = []

    func start() {
        for name in [Notification.Name.sweeflowSettingsChanged, .sweeflowConnectionChanged] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                self?.settleThenSay()
            })
        }
    }

    private var now: State {
        guard host.connected else { return .off }
        return settings.config.enabled ? .streaming : .paused
    }

    private func settleThenSay() {
        pending?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.say() }
        pending = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 3, execute: work)
    }

    private func say() {
        let state = now
        guard state != said else { return }
        let was = said
        said = state
        if state != .off { saidTo = host.clientName }
        let who = saidTo
        switch state {
        case .streaming where was == .paused:
            Notices.shared.post("Streaming resumed", "Your Mac apps are showing on \(who) again.", kind: "stream")
        case .streaming:
            Notices.shared.post("Streaming to \(who)", "Your Mac apps are showing on \(who).", kind: "stream")
        case .paused:
            Notices.shared.post("Streaming paused", "\(who) is still linked. Resume from Sweeflow's menu.", kind: "stream")
        case .off:
            Notices.shared.post("Streaming stopped", "Your Mac apps are no longer showing on \(who).", kind: "stream")
        }
    }
}

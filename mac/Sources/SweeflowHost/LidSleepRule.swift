import AppKit
import IOKit.ps

/// The sudoers rule that lets Sweeflow keep the Mac awake with its lid closed while it streams,
/// on battery too: `pmset disablesleep` is the only switch for that, and it needs root. The rule
/// allows this user exactly the two calls Wakefulness makes, without a password, and nothing
/// else. Installed or removed from the menu ("Stream with the lid closed"), each time with an
/// administrator's password in macOS's own dialog.
enum LidSleepRule {
    static let path = "/etc/sudoers.d/sweeflow"

    static var installed: Bool { FileManager.default.fileExists(atPath: path) }

    /// A Mac with a lid (a MacBook, which has an internal battery): the only kind the switch
    /// means anything on.
    static let hasLid: Bool = {
        guard let info = IOPSCopyPowerSourcesInfo()?.takeRetainedValue(),
              let list = IOPSCopyPowerSourcesList(info)?.takeRetainedValue() as? [CFTypeRef] else { return false }
        return list.contains { source in
            let d = IOPSGetPowerSourceDescription(info, source)?.takeUnretainedValue() as? [String: Any]
            return d?[kIOPSTypeKey] as? String == kIOPSInternalBatteryType
        }
    }()

    /// The rule's text, for `user`.
    static func rule(user: String) -> String {
        "\(user) ALL=(root) NOPASSWD: /usr/bin/pmset -a disablesleep 1, /usr/bin/pmset -a disablesleep 0"
    }

    /// The shell command (run as root) that puts the rule in place: written to a temporary
    /// file, checked by visudo (a broken sudoers file locks sudo out), then installed root-owned
    /// and read-only; or that removes it, lid-close sleep restored first.
    static func command(install: Bool, user: String) -> String {
        if install {
            return "t=$(/usr/bin/mktemp /tmp/sweeflow-sudoers.XXXXXX) && /usr/bin/printf '%s\\n' '\(rule(user: user))' > \"$t\""
                + " && /usr/sbin/visudo -cf \"$t\" >/dev/null && /usr/bin/install -m 0440 -o root -g wheel \"$t\" \(path); s=$?; /bin/rm -f \"$t\"; exit $s"
        }
        return "/usr/bin/pmset -a disablesleep 0; /bin/rm -f \(path)"
    }

    /// Installs or removes the rule, asking for the password (off the main thread: the dialog
    /// waits for the person). `done` gets whether it's now as asked, on the main thread.
    static func set(_ install: Bool, done: @escaping (Bool) -> Void) {
        let user = NSUserName()
        // Short names are letters, digits, "_", "-" and "."; anything else isn't written into a rule.
        guard !user.isEmpty, user.allSatisfy({ $0.isLetter || $0.isNumber || "_-.".contains($0) }) else { return done(false) }
        let prompt = install
            ? "Sweeflow needs an administrator to let it keep this Mac awake with its lid closed while it streams."
            : "Sweeflow needs an administrator to stop keeping this Mac awake with its lid closed."
        let escape = { (s: String) in s.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") }
        let script = "do shell script \"\(escape(command(install: install, user: user)))\" with administrator privileges with prompt \"\(escape(prompt))\""
        DispatchQueue.global(qos: .userInitiated).async {
            let ok = (try? run("/usr/bin/osascript", ["-e", script])) != nil && installed == install
            log(ok ? "lid-close rule \(install ? "installed" : "removed")" : "lid-close rule not \(install ? "installed" : "removed") (cancelled or refused)")
            DispatchQueue.main.async { done(ok) }
        }
    }
}

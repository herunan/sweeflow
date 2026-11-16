import Foundation
import AppKit
import CoreAudio
import AudioToolbox
import AVFoundation

/// Where the streamed apps' sound comes from.
protocol SoundSource: AnyObject {
    func stop()
}
extension AudioCapture: SoundSource {}

/// The streamed apps' sound through a Core Audio process tap: handed over ~10 ms after it plays
/// (ScreenCaptureKit's audio took ~55 ms). Same output as AudioCapture: 48 kHz stereo 16-bit
/// PCM, with how long after it played each piece was captured.
///
/// The tap takes every process's sound but those whose app isn't allowed (Sweeflow's own among
/// them). A helper plays for the app it works for (Safari's sound comes from a WebKit process,
/// Chrome's from a helper): it counts as that app. The list follows Core Audio's own list of
/// processes, which a process joins before it plays. The tap is read through a private
/// aggregate device of its own (see startDevice).
final class AudioTap: SoundSource {
    var onPCM: ((Data, UInt32) -> Void)?
    private let allowed: (String) -> Bool
    private let queue = DispatchQueue(label: "sweeflow.audiotap", qos: .userInteractive)
    /// Where the sound arrives. Not `queue`: stopping the device there waited for a callback
    /// queued behind it, for ever. (The format and converter it reads are only changed while
    /// the device is stopped.)
    private let ioQueue = DispatchQueue(label: "sweeflow.audiotap.io", qos: .userInteractive)
    private var tapID = AudioObjectID(kAudioObjectUnknown)
    private var tapUUID = UUID()
    private var aggregateID = AudioObjectID(kAudioObjectUnknown)
    private var procID: AudioDeviceIOProcID?
    /// The processes left out.
    private var excluded: [AudioObjectID] = []
    private var stopped = false
    private var format = AudioStreamBasicDescription()
    private var converter: AVAudioConverter?
    private var listeners: [(AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyListenerBlock)] = []
    private var peak: Float = 0
    private var lastLevelLog = Date()
    private var ageSum = 0.0, ageN = 0, ageMax = 0.0, framesSum = 0

    init(allowed: @escaping (String) -> Bool) {
        self.allowed = allowed
    }

    /// `done(false)` when there's no tap to be had (then ScreenCaptureKit's audio is used
    /// instead). On its own queue: the first time, making the tap waits for the person to answer
    /// macOS's "record your system audio" question.
    func start(_ done: @escaping (Bool) -> Void) {
        queue.async { done(self.begin()) }
    }

    private func begin() -> Bool {
        do {
            guard !stopped else { return true }
            excluded = excludedProcesses()
            let desc = tapDescription()
            var id = AudioObjectID(kAudioObjectUnknown)
            let err = AudioHardwareCreateProcessTap(desc, &id)
            guard err == noErr, id != kAudioObjectUnknown else {
                log("audio tap: not made (\(err), \(excluded.count) of \(audioProcessObjects().count) processes left out); using ScreenCaptureKit's audio")
                return false
            }
            tapID = id
            guard startDevice() else {
                AudioHardwareDestroyProcessTap(tapID)
                tapID = AudioObjectID(kAudioObjectUnknown)
                return false
            }
            listen(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyProcessObjectList) { [weak self] in
                self?.updateProcesses()
            }
            // Apps launching or quitting don't always change the process list (an app that
            // hasn't played yet isn't in it), but the allowed set may change with them.
            let nc = NSWorkspace.shared.notificationCenter
            for name in [NSWorkspace.didLaunchApplicationNotification, NSWorkspace.didTerminateApplicationNotification] {
                observers.append(nc.addObserver(forName: name, object: nil, queue: nil) { [weak self] _ in
                    self?.queue.asyncAfter(deadline: .now() + 0.5) { self?.updateProcesses() }
                })
            }
            return true
        }
    }
    private var observers: [NSObjectProtocol] = []

    func stop() {
        queue.async {
            self.stopped = true
            self.observers.forEach { NSWorkspace.shared.notificationCenter.removeObserver($0) }
            self.observers.removeAll()
            for (obj, addr, block) in self.listeners {
                var a = addr
                AudioObjectRemovePropertyListenerBlock(obj, &a, self.queue, block)
            }
            self.listeners.removeAll()
            self.stopDevice()
            if self.tapID != kAudioObjectUnknown { AudioHardwareDestroyProcessTap(self.tapID) }
            self.tapID = AudioObjectID(kAudioObjectUnknown)
        }
    }

    /// Makes a tap and drops it, so macOS asks its question (waits for the answer). True when
    /// a tap could be made.
    static func ask() -> Bool {
        let d = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
        var id = AudioObjectID(kAudioObjectUnknown)
        guard AudioHardwareCreateProcessTap(d, &id) == noErr, id != kAudioObjectUnknown else { return false }
        AudioHardwareDestroyProcessTap(id)
        return true
    }

    // MARK: processes

    private func tapDescription() -> CATapDescription {
        let d = CATapDescription(stereoGlobalTapButExcludeProcesses: excluded)
        d.uuid = tapUUID
        d.name = "Sweeflow"
        d.muteBehavior = .unmuted
        return d
    }

    private func updateProcesses() {
        guard !stopped, tapID != kAudioObjectUnknown else { return }
        let now = excludedProcesses()
        guard now != excluded else { return }
        excluded = now
        // A new tap rather than a changed one: setting its description (kAudioTapPropertyDescription)
        // stopped its sound for good on macOS 27, even through a new device.
        stopDevice()
        AudioHardwareDestroyProcessTap(tapID)
        tapID = AudioObjectID(kAudioObjectUnknown)
        tapUUID = UUID()
        var id = AudioObjectID(kAudioObjectUnknown)
        guard AudioHardwareCreateProcessTap(tapDescription(), &id) == noErr, id != kAudioObjectUnknown else {
            return log("audio tap: not made again after the process list changed")
        }
        tapID = id
        if !startDevice() { log("audio tap: no device after the process list changed") }
    }

    /// Core Audio's process objects whose app (or the app they work for) isn't allowed.
    private func excludedProcesses() -> [AudioObjectID] {
        let me = getpid()
        var apps: [pid_t: Bool] = [:]
        return audioProcessObjects().filter { obj in
            guard let pid = audioProcessPID(obj), pid != me else { return true }
            let owner = responsiblePID(pid)
            if owner == me { return true }
            if let known = apps[owner] { return !known }
            let ok = NSRunningApplication(processIdentifier: owner)?.bundleIdentifier.map(allowed) ?? false
            apps[owner] = ok
            return !ok
        }.sorted()
    }

    // MARK: device

    /// A private aggregate device of the tap alone. Not with the output device in it: with
    /// the C34J79x's sound as the output, starting it timed out (10 s, twice, and no sound
    /// until the output changed again); and the output's own inputs (the RØDE's microphone)
    /// came in ahead of the tap. Not with "tap auto-start" either: with it the device never ran
    /// (macOS 27).
    private func startDevice() -> Bool {
        let desc: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Sweeflow sound",
            kAudioAggregateDeviceUIDKey: "sweeflow-tap-\(UUID().uuidString)",
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapDriftCompensationKey: true, kAudioSubTapUIDKey: tapUUID.uuidString]],
        ]
        var agg = AudioObjectID(kAudioObjectUnknown)
        var err = AudioHardwareCreateAggregateDevice(desc as CFDictionary, &agg)
        guard err == noErr else { log("audio tap: aggregate device not made (\(err))"); return false }
        aggregateID = agg
        // ~10 ms pieces: small enough not to add delay, big enough not to flood the network.
        var frames: UInt32 = 480
        var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyBufferFrameSize, mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        AudioObjectSetPropertyData(agg, &addr, 0, nil, UInt32(MemoryLayout<UInt32>.size), &frames)
        guard let fmt = tapFormat() else { log("audio tap: no format"); stopDevice(); return false }
        format = fmt
        converter = nil
        if fmt.mSampleRate != 48_000 || fmt.mChannelsPerFrame != 2 {
            var src = fmt
            if let from = AVAudioFormat(streamDescription: &src),
               let to = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: false) {
                converter = AVAudioConverter(from: from, to: to)
            }
        }
        var proc: AudioDeviceIOProcID?
        err = AudioDeviceCreateIOProcIDWithBlock(&proc, agg, ioQueue) { [weak self] now, input, inputTime, _, _ in
            self?.received(input, now: now.pointee, at: inputTime.pointee)
        }
        guard err == noErr, let proc else { log("audio tap: no IOProc (\(err))"); stopDevice(); return false }
        procID = proc
        err = AudioDeviceStart(agg, proc)
        guard err == noErr else { log("audio tap: not started (\(err))"); stopDevice(); return false }
        log(String(format: "audio tap: %d processes left out, %.0f Hz, %d channels", excluded.count, fmt.mSampleRate,
                   fmt.mChannelsPerFrame))
        return true
    }

    private func stopDevice() {
        if aggregateID != kAudioObjectUnknown {
            if let p = procID {
                AudioDeviceStop(aggregateID, p)
                AudioDeviceDestroyIOProcID(aggregateID, p)
            }
            AudioHardwareDestroyAggregateDevice(aggregateID)
        }
        procID = nil
        aggregateID = AudioObjectID(kAudioObjectUnknown)
    }

    private func tapFormat() -> AudioStreamBasicDescription? {
        var f = AudioStreamBasicDescription()
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        var addr = AudioObjectPropertyAddress(mSelector: kAudioTapPropertyFormat, mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectGetPropertyData(tapID, &addr, 0, nil, &size, &f) == noErr,
              f.mFormatID == kAudioFormatLinearPCM, f.mFormatFlags & kAudioFormatFlagIsFloat != 0, f.mBitsPerChannel == 32 else { return nil }
        return f
    }

    // MARK: sound

    private func received(_ input: UnsafePointer<AudioBufferList>, now: AudioTimeStamp, at time: AudioTimeStamp) {
        guard !stopped else { return }
        // The tap's buffers (the last ones, should the device ever have others ahead of them).
        let nonInterleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        let channels = Int(format.mChannelsPerFrame)
        let all = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: input))
        let n = nonInterleaved ? channels : 1
        guard all.count >= n else { return }
        let abl = AudioBufferList.allocate(maximumBuffers: n)
        defer { free(abl.unsafeMutablePointer) }
        for i in 0..<n { abl[i] = all[all.count - n + i] }
        guard let first = abl.first, first.mDataByteSize > 0 else { return }
        let inFrames = Int(first.mDataByteSize) / 4 / (nonInterleaved ? 1 : channels)
        guard inFrames > 0 else { return }

        // Float32 → interleaved stereo s16le, through the converter when not 48 kHz stereo.
        var out: Data
        if let converter {
            guard let inBuf = AVAudioPCMBuffer(pcmFormat: converter.inputFormat, bufferListNoCopy: abl.unsafePointer),
                  let outBuf = AVAudioPCMBuffer(pcmFormat: converter.outputFormat,
                                                frameCapacity: AVAudioFrameCount(Double(inFrames) * 48_000 / format.mSampleRate) + 16)
            else { return }
            var given = false
            converter.convert(to: outBuf, error: nil) { _, status in
                if given { status.pointee = .noDataNow; return nil }
                given = true
                status.pointee = .haveData
                return inBuf
            }
            let n = Int(outBuf.frameLength)
            guard n > 0, let ch = outBuf.floatChannelData else { return }
            out = pcm16(frames: n) { c, i in ch[c][i] }
        } else {
            out = pcm16(frames: inFrames) { c, i in
                if nonInterleaved {
                    guard c < abl.count, let p = abl[c].mData?.assumingMemoryBound(to: Float.self) else { return 0 }
                    return p[i]
                }
                guard let p = abl[0].mData?.assumingMemoryBound(to: Float.self) else { return 0 }
                return p[i * channels + c]
            }
        }
        // How old the sound is on arrival here (host clock), as with ScreenCaptureKit's.
        var ageUs: UInt32 = 0
        if now.mFlags.contains(.hostTimeValid), time.mFlags.contains(.hostTimeValid), now.mHostTime > time.mHostTime {
            let age = hostSeconds(now.mHostTime - time.mHostTime)
            ageSum += age; ageN += 1; ageMax = max(ageMax, age); framesSum += inFrames
            ageUs = UInt32(max(0, min(age, 2)) * 1_000_000)
        }
        onPCM?(out, ageUs)
        if Date().timeIntervalSince(lastLevelLog) > 5 {
            log(String(format: "audio: peak level %.3f over the last 5s", peak))
            if ageN > 0 {
                log(String(format: "audio: captured %.0f ms after it played on average (worst %.0f), %d frames a buffer",
                           ageSum / Double(ageN) * 1000, ageMax * 1000, framesSum / ageN))
            }
            ageSum = 0; ageN = 0; ageMax = 0; framesSum = 0
            peak = 0
            lastLevelLog = Date()
        }
    }

    private func pcm16(frames: Int, _ sample: (Int, Int) -> Float) -> Data {
        var out = Data(count: frames * 4)
        out.withUnsafeMutableBytes { raw in
            let dst = raw.bindMemory(to: Int16.self)
            for i in 0..<frames {
                for c in 0..<2 {
                    let v = max(-1, min(1, sample(c, i)))
                    peak = max(peak, abs(v))
                    dst[i * 2 + c] = Int16((v * 32767).rounded()).littleEndian
                }
            }
        }
        return out
    }

    // MARK: helpers

    private func listen(_ obj: AudioObjectID, _ selector: AudioObjectPropertySelector, _ action: @escaping () -> Void) {
        var addr = AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        let block: AudioObjectPropertyListenerBlock = { _, _ in action() }
        if AudioObjectAddPropertyListenerBlock(obj, &addr, queue, block) == noErr {
            listeners.append((obj, addr, block))
        }
    }
}

private func audioProcessObjects() -> [AudioObjectID] {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyProcessObjectList, mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    let sys = AudioObjectID(kAudioObjectSystemObject)
    guard AudioObjectGetPropertyDataSize(sys, &addr, 0, nil, &size) == noErr, size > 0 else { return [] }
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(sys, &addr, 0, nil, &size, &ids) == noErr else { return [] }
    return Array(ids.prefix(Int(size) / MemoryLayout<AudioObjectID>.size))
}

private func audioProcessPID(_ obj: AudioObjectID) -> pid_t? {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioProcessPropertyPID, mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var pid: pid_t = 0
    var size = UInt32(MemoryLayout<pid_t>.size)
    return AudioObjectGetPropertyData(obj, &addr, 0, nil, &size, &pid) == noErr ? pid : nil
}

/// The app a process works for (Safari for its WebKit processes): what macOS holds responsible
/// for it. The process itself when it's on its own.
private let responsibleFor: (@convention(c) (pid_t) -> pid_t)? =
    dlsym(UnsafeMutableRawPointer(bitPattern: -2), "responsibility_get_pid_responsible_for_pid")
        .map { unsafeBitCast($0, to: (@convention(c) (pid_t) -> pid_t).self) }

private func responsiblePID(_ pid: pid_t) -> pid_t {
    let r = responsibleFor?(pid) ?? pid
    return r > 0 ? r : pid
}

private func hostSeconds(_ ticks: UInt64) -> Double {
    var tb = mach_timebase_info_data_t()
    mach_timebase_info(&tb)
    return Double(ticks) * Double(tb.numer) / Double(tb.denom) / 1e9
}

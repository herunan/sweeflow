import Foundation
import ScreenCaptureKit
import CoreMedia
import AudioToolbox

/// Captures the audio of the streamed apps (all of them, mixed) and hands out 48 kHz stereo
/// 16-bit PCM. The app list is refreshed every few seconds so newly launched apps are heard.
final class AudioCapture: NSObject, SCStreamOutput, SCStreamDelegate {
    var onPCM: ((Data) -> Void)?
    private let allowed: (String) -> Bool
    private let queue = DispatchQueue(label: "sweeflow.audio", qos: .userInteractive)
    private var stream: SCStream?
    private var apps: Set<String> = []
    private var timer: DispatchSourceTimer?
    private var stopped = false
    private var peak: Float = 0
    private var lastLevelLog = Date()

    init(allowed: @escaping (String) -> Bool) {
        self.allowed = allowed
    }

    func start() {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .seconds(3))
        t.setEventHandler { [weak self] in self?.refresh() }
        t.resume()
        timer = t
    }

    func stop() {
        queue.async {
            self.stopped = true
            self.timer?.cancel()
            self.stream?.stopCapture { _ in }
            self.stream = nil
        }
    }

    private func refresh() {
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: false) { [weak self] content, _ in
            guard let self, let content else { return }
            self.queue.async {
                // The display the streamed windows are on (the virtual one): audio capture follows
                // the filter's display.
                let vd = DispatchQueue.main.sync { host.display.displayID }
                guard !self.stopped,
                      let display = content.displays.first(where: { $0.displayID == vd }) ?? content.displays.first else { return }
                let targets = content.applications.filter { self.allowed($0.bundleIdentifier) }
                let ids = Set(targets.map(\.bundleIdentifier))
                guard ids != self.apps || self.stream == nil, !targets.isEmpty else { return }
                self.apps = ids
                let filter = SCContentFilter(display: display, including: targets, exceptingWindows: [])
                if let s = self.stream {
                    s.updateContentFilter(filter) { err in if let err { log("audio filter update: \(err)") } }
                    return
                }
                let cfg = SCStreamConfiguration()
                cfg.capturesAudio = true
                cfg.sampleRate = 48_000
                cfg.channelCount = 2
                cfg.excludesCurrentProcessAudio = true
                // Video is mandatory for an SCStream; keep it negligible.
                cfg.width = 2; cfg.height = 2
                cfg.minimumFrameInterval = CMTime(value: 1, timescale: 1)
                cfg.queueDepth = 3
                let s = SCStream(filter: filter, configuration: cfg, delegate: self)
                do { try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: self.queue) } catch {
                    log("audio output: \(error)"); return
                }
                self.stream = s
                s.startCapture { err in
                    if let err { log("audio capture failed: \(err)") } else { log("audio capture: \(ids.sorted())") }
                }
            }
        }
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, sb.isValid, let fmt = CMSampleBufferGetFormatDescription(sb),
              let asbdPtr = CMAudioFormatDescriptionGetStreamBasicDescription(fmt) else { return }
        let asbd = asbdPtr.pointee
        guard asbd.mFormatID == kAudioFormatLinearPCM, asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0,
              asbd.mBitsPerChannel == 32 else { return }
        let frames = CMSampleBufferGetNumSamples(sb)
        guard frames > 0 else { return }

        var ablSize = 0
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, bufferListSizeNeededOut: &ablSize, bufferListOut: nil,
                                                                bufferListSize: 0, blockBufferAllocator: nil,
                                                                blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: nil)
        let ablMem = UnsafeMutableRawPointer.allocate(byteCount: ablSize, alignment: 16)
        defer { ablMem.deallocate() }
        let abl = ablMem.bindMemory(to: AudioBufferList.self, capacity: 1)
        var block: CMBlockBuffer?
        guard CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb, bufferListSizeNeededOut: nil, bufferListOut: abl, bufferListSize: ablSize, blockBufferAllocator: nil,
            blockBufferMemoryAllocator: nil, flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            blockBufferOut: &block) == noErr else { return }
        let buffers = UnsafeMutableAudioBufferListPointer(abl)
        let nonInterleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        let channels = Int(asbd.mChannelsPerFrame)

        // Float32 (planar or interleaved, 1–2+ channels) → interleaved stereo s16le.
        var out = Data(count: frames * 4)
        out.withUnsafeMutableBytes { raw in
            let dst = raw.bindMemory(to: Int16.self)
            func sample(_ ch: Int, _ i: Int) -> Float {
                let c = min(ch, channels - 1)
                if nonInterleaved {
                    guard c < buffers.count, let p = buffers[c].mData?.assumingMemoryBound(to: Float.self) else { return 0 }
                    return p[i]
                }
                guard let p = buffers[0].mData?.assumingMemoryBound(to: Float.self) else { return 0 }
                return p[i * channels + c]
            }
            for i in 0..<frames {
                for ch in 0..<2 {
                    let v = max(-1, min(1, sample(ch, i)))
                    peak = max(peak, abs(v))
                    dst[i * 2 + ch] = Int16((v * 32767).rounded()).littleEndian
                }
            }
        }
        onPCM?(out)
        if Date().timeIntervalSince(lastLevelLog) > 5 {
            log(String(format: "audio: peak level %.3f over the last 5s", peak))
            peak = 0
            lastLevelLog = Date()
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        log("audio stream stopped: \(error)")
        queue.async { self.stream = nil }
    }
}

import Darwin
import Foundation

enum Diagnostics {
    /// Resident set size of this process in bytes (0 if unavailable).
    static func residentBytes() -> UInt64 {
        var info = mach_task_basic_info()
        var count = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<natural_t>.size)
        let kr = withUnsafeMutablePointer(to: &info) { ptr in
            ptr.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count)
            }
        }
        return kr == KERN_SUCCESS ? info.resident_size : 0
    }

    static func residentMB() -> Int { Int(residentBytes() / (1024 * 1024)) }
}

/// Logs the first occurrence of a repeating problem, then only every `every`-th one, so a
/// persistent fault (disk full, converter failure) can't flood stderr for hours.
/// Not thread-safe: use one instance per serial owner.
final class RateLimitedLog {
    private(set) var count = 0
    private let every: Int

    init(every: Int = 500) { self.every = every }

    func log(_ message: @autoclosure () -> String) {
        count += 1
        if count == 1 {
            meetingTrackerLog(message())
        } else if count % every == 0 {
            meetingTrackerLog("\(message()) (repeated \(count)×)")
        }
    }
}

/// Alias so `RateLimitedLog.log` can call the global logger despite the name clash.
func meetingTrackerLog(_ s: String) { log(s) }

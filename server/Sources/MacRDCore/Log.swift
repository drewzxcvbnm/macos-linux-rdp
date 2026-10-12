import Foundation

private let logQueue = DispatchQueue(label: "macrd.log")
private let formatter: DateFormatter = {
    let f = DateFormatter()
    f.dateFormat = "HH:mm:ss.SSS"
    return f
}()

public func log(_ message: @autoclosure () -> String) {
    let line = "\(formatter.string(from: Date())) \(message())\n"
    logQueue.async { FileHandle.standardError.write(Data(line.utf8)) }
}

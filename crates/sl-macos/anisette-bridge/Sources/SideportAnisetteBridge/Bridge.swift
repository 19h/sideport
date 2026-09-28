import AnisetteKit
import CryptoKit
import Darwin
import Foundation

private final class ResultBox: @unchecked Sendable {
    let semaphore = DispatchSemaphore(value: 0)
    var result: Result<String, Error>?
}

@_cdecl("sideport_anisette_get_headers")
public func sideportAnisetteGetHeaders(
    _ libraries: UnsafePointer<CChar>?,
    _ provisioning: UnsafePointer<CChar>?,
    _ identity: UnsafePointer<CChar>?,
    _ clientInfo: UnsafePointer<CChar>?,
    _ userAgent: UnsafePointer<CChar>?,
    _ output: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let libraries, let provisioning, let identity, let clientInfo, let userAgent, let output,
          let identifier = UUID(uuidString: String(cString: identity)) else {
        return -1
    }

    let libraryURL = URL(fileURLWithPath: String(cString: libraries), isDirectory: true)
    let provisioningURL = URL(fileURLWithPath: String(cString: provisioning), isDirectory: true)
    let clientInfoValue = String(cString: clientInfo)
    let userAgentValue = String(cString: userAgent)
    let box = ResultBox()

    Task.detached {
        do {
            try FileManager.default.createDirectory(at: provisioningURL, withIntermediateDirectories: true)

            let client = try AnisetteClient(
                provisioningDir: provisioningURL,
                clientInfo: clientInfoValue,
                userAgent: userAgentValue,
                provider: UnicornAnisetteDataProvider(),
                libraryDirectoryResolver: { libraryURL }
            )

            let identityBytes = withUnsafeBytes(of: identifier.uuid) { Data($0) }
            let localUserID = SHA256.hash(data: identityBytes).map { String(format: "%02X", $0) }.joined()

            var overrides = AnisetteRequestHeaders()
            overrides.localUserID = localUserID

            let (headers, _) = try await client.getAnisetteData(identifier: identifier, headers: overrides)
            let data = try JSONSerialization.data(withJSONObject: headers)

            box.result = .success(String(decoding: data, as: UTF8.self))
        } catch {
            box.result = .failure(error)
        }

        box.semaphore.signal()
    }

    box.semaphore.wait()

    switch box.result {
    case .success(let json):
        output.pointee = strdup(json)
        return 0

    case .failure(let error):
        output.pointee = strdup(String(describing: error))
        return -2

    case .none:
        return -3
    }
}

@_cdecl("sideport_anisette_free")
public func sideportAnisetteFree(_ pointer: UnsafeMutablePointer<CChar>?) {
    free(pointer)
}

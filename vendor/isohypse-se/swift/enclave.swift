import CryptoKit
import Foundation

private func loadOrCreate(_ path: String) -> SecureEnclave.P256.Signing.PrivateKey? {
    if let data = FileManager.default.contents(atPath: path) {
        return try? SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: data)
    }
    guard SecureEnclave.isAvailable,
          let key = try? SecureEnclave.P256.Signing.PrivateKey() else { return nil }
    let dir = (path as NSString).deletingLastPathComponent
    try? FileManager.default.createDirectory(
        atPath: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    FileManager.default.createFile(
        atPath: path, contents: key.dataRepresentation, attributes: [.posixPermissions: 0o600])
    return key
}

@_cdecl("iso_enclave_available")
public func iso_enclave_available() -> Int32 { SecureEnclave.isAvailable ? 1 : 0 }

@_cdecl("iso_enclave_sign")
public func iso_enclave_sign(
    _ keyPath: UnsafePointer<CChar>, _ data: UnsafePointer<UInt8>, _ dataLen: Int,
    _ out: UnsafeMutablePointer<UInt8>, _ outCap: Int
) -> Int32 {
    guard let key = loadOrCreate(String(cString: keyPath)) else { return -1 }
    let message = Data(bytes: data, count: dataLen)
    guard let sig = try? key.signature(for: message) else { return -1 }
    let raw = sig.rawRepresentation
    guard raw.count <= outCap else { return -1 }
    raw.copyBytes(to: out, count: raw.count)
    return Int32(raw.count)
}

@_cdecl("iso_enclave_verify")
public func iso_enclave_verify(
    _ keyPath: UnsafePointer<CChar>, _ data: UnsafePointer<UInt8>, _ dataLen: Int,
    _ sig: UnsafePointer<UInt8>, _ sigLen: Int
) -> Int32 {
    guard let key = loadOrCreate(String(cString: keyPath)) else { return -1 }
    let message = Data(bytes: data, count: dataLen)
    let sigData = Data(bytes: sig, count: sigLen)
    guard let signature = try? P256.Signing.ECDSASignature(rawRepresentation: sigData) else { return 0 }
    return key.publicKey.isValidSignature(signature, for: message) ? 1 : 0
}

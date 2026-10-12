import CryptoKit
import Foundation
import Security

public enum IdentityError: Error, CustomStringConvertible {
    case opensslFailed(String)
    case importFailed(OSStatus)
    case noIdentity

    public var description: String {
        switch self {
        case let .opensslFailed(msg): "openssl failed: \(msg)"
        case let .importFailed(status): "PKCS#12 import failed (\(status))"
        case .noIdentity: "PKCS#12 contained no identity"
        }
    }
}

/// Server TLS identity (self-signed, generated on first run) and connection password.
public struct ServerIdentity {
    public let identity: SecIdentity
    /// SHA-256 of the leaf certificate DER; clients pin this.
    public let fingerprint: Data

    public static var configDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/macrd", isDirectory: true)
    }

    private static let p12Passphrase = "macrd"

    public static func loadOrCreate(in dir: URL = configDirectory) throws -> ServerIdentity {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        let p12URL = dir.appendingPathComponent("identity.p12")
        if !FileManager.default.fileExists(atPath: p12URL.path) {
            try generate(into: dir, p12: p12URL)
        }
        let p12 = try Data(contentsOf: p12URL)
        var options: [CFString: Any] = [kSecImportExportPassphrase: p12Passphrase]
        if #available(macOS 15.0, *) {
            options[kSecImportToMemoryOnly] = true
        }
        var items: CFArray?
        let status = SecPKCS12Import(p12 as CFData, options as CFDictionary, &items)
        guard status == errSecSuccess else { throw IdentityError.importFailed(status) }
        guard let dicts = items as? [[CFString: Any]],
              let first = dicts.first,
              let idRef = first[kSecImportItemIdentity] else { throw IdentityError.noIdentity }
        let identity = idRef as! SecIdentity

        var cert: SecCertificate?
        SecIdentityCopyCertificate(identity, &cert)
        guard let cert else { throw IdentityError.noIdentity }
        let der = SecCertificateCopyData(cert) as Data
        return ServerIdentity(identity: identity, fingerprint: Data(SHA256.hash(data: der)))
    }

    // RSA + 3DES: the in-memory PKCS#12 importer cannot read EC keys or AES-encrypted bags written by LibreSSL.
    private static func generate(into dir: URL, p12: URL) throws {
        let key = dir.appendingPathComponent("key.pem")
        let cert = dir.appendingPathComponent("cert.pem")
        // Explicit v3 extensions: without them LibreSSL emits an X.509 v1 certificate, which rustls rejects.
        let conf = dir.appendingPathComponent("openssl.cnf")
        try """
        [req]
        distinguished_name = dn
        x509_extensions = v3
        prompt = no
        [dn]
        CN = macrd
        [v3]
        basicConstraints = critical, CA:FALSE
        keyUsage = critical, digitalSignature, keyEncipherment
        extendedKeyUsage = serverAuth
        subjectAltName = DNS:macrd
        subjectKeyIdentifier = hash
        """.write(to: conf, atomically: true, encoding: .utf8)
        defer { try? FileManager.default.removeItem(at: conf) }
        try runOpenSSL(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-config", conf.path,
                        "-days", "3650", "-keyout", key.path, "-out", cert.path])
        try runOpenSSL(["pkcs12", "-export", "-inkey", key.path, "-in", cert.path, "-out", p12.path,
                        "-passout", "pass:\(p12Passphrase)",
                        "-keypbe", "PBE-SHA1-3DES", "-certpbe", "PBE-SHA1-3DES", "-macalg", "sha1"])
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: p12.path)
        try? FileManager.default.removeItem(at: key)
        log("generated new TLS identity at \(p12.path)")
    }

    private static func runOpenSSL(_ args: [String]) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/openssl")
        p.arguments = args
        let errPipe = Pipe()
        p.standardError = errPipe
        p.standardOutput = FileHandle.nullDevice
        try p.run()
        p.waitUntilExit()
        if p.terminationStatus != 0 {
            let msg = String(data: errPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            throw IdentityError.opensslFailed(msg)
        }
    }
}

public enum Password {
    /// Returns the configured password, generating and storing a random one on first run.
    public static func loadOrCreate(in dir: URL = ServerIdentity.configDirectory) throws -> String {
        let url = dir.appendingPathComponent("password")
        if let s = try? String(contentsOf: url, encoding: .utf8) {
            let trimmed = s.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty { return trimmed }
        }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        let alphabet = Array("abcdefghjkmnpqrstuvwxyz23456789")
        var rng = SystemRandomNumberGenerator()
        let pw = String((0..<16).map { _ in alphabet.randomElement(using: &rng)! })
        try pw.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
        return pw
    }

    public static func mac(password: String, nonce: Data, fingerprint: Data) -> Data {
        var msg = nonce
        msg.append(fingerprint)
        let key = SymmetricKey(data: Data(password.utf8))
        return Data(HMAC<SHA256>.authenticationCode(for: msg, using: key))
    }

    public static func randomNonce() -> Data {
        var bytes = [UInt8](repeating: 0, count: 32)
        _ = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        return Data(bytes)
    }
}

extension Data {
    public var hexColonString: String {
        map { String(format: "%02X", $0) }.joined(separator: ":")
    }
}

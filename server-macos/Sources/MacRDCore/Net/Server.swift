import Foundation
import Network
import Security

/// Accepts connections; one authenticated session is active at a time (a new one replaces the old).
public final class Server {
    private let config: ServerConfig
    private var listener: NWListener?
    private let queue = DispatchQueue(label: "macrd.server")
    private var sessions: [UUID: Session] = [:]
    private var active: Session?
    private var clipboard: ClipboardSync?

    public init(config: ServerConfig) {
        self.config = config
    }

    public func start() throws {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        tcp.enableKeepalive = true
        tcp.keepaliveIdle = 10

        let params: NWParameters
        if config.insecure {
            params = NWParameters(tls: nil, tcp: tcp)
        } else {
            guard let identity = config.identity, let secIdentity = sec_identity_create(identity.identity) else {
                throw IdentityError.noIdentity
            }
            let tls = NWProtocolTLS.Options()
            sec_protocol_options_set_local_identity(tls.securityProtocolOptions, secIdentity)
            sec_protocol_options_set_min_tls_protocol_version(tls.securityProtocolOptions, .TLSv13)
            params = NWParameters(tls: tls, tcp: tcp)
        }
        params.allowLocalEndpointReuse = true

        let listener = try NWListener(using: params, on: NWEndpoint.Port(rawValue: config.port)!)
        listener.newConnectionHandler = { [weak self] conn in self?.accept(conn) }
        listener.stateUpdateHandler = { state in
            switch state {
            case .ready: log("listening on port \(self.config.port)\(self.config.insecure ? " (INSECURE, no TLS)" : "")")
            case let .failed(error): log("listener failed: \(error)"); exit(1)
            default: break
            }
        }
        listener.start(queue: queue)
        self.listener = listener

        let clipboard = ClipboardSync { [weak self] kind, data in
            self?.queue.async { self?.active?.send(.clipboard(kind: kind.rawValue, data: data)) }
        }
        clipboard.start()
        self.clipboard = clipboard
    }

    private func accept(_ conn: NWConnection) {
        let session = Session(connection: conn, config: config)
        session.onAuthenticated = { [weak self] s in
            guard let self else { return }
            queue.async { [self] in
                if let old = self.active, old !== s {
                    old.close(reason: "replaced by a new connection")
                }
                self.active = s
                self.clipboard?.sendCurrent()
            }
        }
        session.onClipboard = { [weak self] kind, data in
            self?.clipboard?.applyRemote(kind: kind, data: data)
        }
        session.onClose = { [weak self] s in
            guard let self else { return }
            queue.async { [self] in
                self.sessions[s.id] = nil
                if self.active === s { self.active = nil }
            }
        }
        sessions[session.id] = session
        session.start()
    }
}

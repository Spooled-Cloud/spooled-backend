# Local TLS material (development only)

The `grpc-cert.pem` / `grpc-key.pem` pair in this directory is for **local/dev** gRPC TLS experiments.

- Treat the private key as **disposable**. Do not reuse it on any production host.
- Prefer generating fresh certs per environment (see `docs/guides/grpc-server.md`).
- Never commit real production private keys. If a production key was ever copied here, rotate it on the host and replace this file with a newly generated local-only key.

Agents: do not paste key material into docs, chat, or knowledge bases.

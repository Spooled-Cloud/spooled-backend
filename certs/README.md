# Local TLS material (development only)

This directory holds no key material. The development pair that used to live
here (`grpc-cert.pem` / `grpc-key.pem`) was removed from the tree on 2026-10-12;
it stays in Git history, so treat it as public and never use it anywhere.
Production does not use it: the production Compose stack generates its own
keypair in the `grpc_tls` volume (`docker-compose.prod.yml`, `grpc-tls-init`).

For local gRPC TLS experiments, generate a throwaway pair here (`*.pem` is
ignored by Docker builds through `.dockerignore`; do not commit it):

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=localhost"   -keyout certs/grpc-key.pem -out certs/grpc-cert.pem
GRPC_TLS_ENABLED=true GRPC_TLS_CERT_PATH=certs/grpc-cert.pem GRPC_TLS_KEY_PATH=certs/grpc-key.pem cargo run
```

Never commit real production private keys.

Agents: do not paste key material into docs, chat, or knowledge bases.

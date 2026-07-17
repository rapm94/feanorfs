# Deploy the opaque relay

FeanorFS uses the same `feanorfs serve --relay` implementation for self-hosted
and future managed connectivity. The relay forwards bounded opaque pairing
frames and inner-TLS bytes; it does not terminate the private hub's TLS, receive
workspace keys or tokens, or inspect file content.

Tagged releases publish an amd64/arm64 OCI image at:

```text
ghcr.io/rapm94/feanorfs-relay:<version>
```

The image runs as UID/GID `10001`, supports a read-only root filesystem,
generates a protected bearer token inside `/var/lib/feanorfs`, and enables
periodic garbage collection. Its internal HTTP listener is intended only for a
TLS reverse proxy. Never publish port 3030 directly to an untrusted network.

## Run the container

Use a release version rather than `latest` in production:

```bash
docker volume create feanorfs-relay-data

docker run -d \
  --name feanorfs-relay \
  --restart unless-stopped \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,nodev \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  -p 127.0.0.1:3030:3030 \
  -v feanorfs-relay-data:/var/lib/feanorfs \
  ghcr.io/rapm94/feanorfs-relay:<version>
```

Podman accepts the same options. For a bind mount, make the host directory
writable only by container UID/GID `10001`; do not loosen it to a world-writable
mode. The data volume preserves the generated hub authentication identity
across restarts. Relay sessions and frames remain memory-only.

The image health check expects an unauthenticated request to the protected hub
API to return `401`. That proves the process is serving and authentication has
not been disabled:

```bash
docker inspect --format '{{.State.Health.Status}}' feanorfs-relay
```

## Terminate public TLS

Point a public DNS name at the host and reverse proxy HTTPS/WSS to the loopback
listener. Caddy's minimal configuration is:

```caddyfile
relay.example.com {
    reverse_proxy 127.0.0.1:3030
}
```

Caddy obtains and renews the public certificate and forwards WebSocket upgrades
without extra directives. Expose only ports 80/443 from the proxy. Other
proxies must support long-lived WebSockets and must not buffer binary frames.

Do not enable request-path access logs at the proxy. Pairing session IDs and
tunnel routes are high-entropy reachability capabilities; FeanorFS deliberately
omits request URIs from its own HTTP tracing, but a reverse proxy can otherwise
record them. Infrastructure metrics should use status, connection count,
duration, and byte totals without paths or query strings.

## Connect a private hub

After `https://relay.example.com` is publicly reachable:

```bash
feanorfs start --relay https://relay.example.com ~/projects/my-app
```

The private hub keeps outbound WSS offers available. **Pair Another Computer…**
in the tray, or `feanorfs pair`, then produces an `fnp2-…` capability. On the
other computer choose **Join Another Computer…**, paste it, and select a folder.
The terminal equivalent remains:

```bash
feanorfs start fnp2-… ~/projects/my-app
```

Direct LAN/mDNS connectivity remains preferred automatically. Relay fallback
retains the private hub's original hostname for inner Rustls verification and
uses its bearer token inside that encrypted stream.

## Verify the published image

Release images include an SBOM, BuildKit provenance, and a GitHub build
attestation bound to the immutable image digest:

```bash
gh attestation verify \
  oci://ghcr.io/rapm94/feanorfs-relay:<version> \
  --repo rapm94/feanorfs
```

There is not yet a project-operated default relay. This artifact makes the
existing opaque relay reproducibly deployable; it does not claim hosted
availability, account recovery, or direct NAT traversal.

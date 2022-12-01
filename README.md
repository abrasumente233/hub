# hub

Expose your TCP service behind NAT to remote host.

## todos

### functionalities

- [x] command line interface for specifying hub `ip:port` and exposed port
- [x] default exposed port should be the original service port, as provided by the spoke
- [x] simplify the handshakes code
- [ ] authentication

### would be nice if we have

- [x] prebuilt binaries for multiple platforms, including musl builds for Linux
- [ ] prebuilt binaries for arm32 and 32-bit x86? although nobody would use it, nah.
- [x] cleaned up all my shitty code and logs
- [ ] cleaned up more shitty code
- [ ] clearer error handling logic...

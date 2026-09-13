# Soul Agent

Rust host-side SIDP implementation for Soul Injector. Development starts with
SIDP v1 framing and proceeds to a USB CDC session and a local GDB RSP server.
WebSocket and graphical UI support are outside this first milestone.

## Development

```sh
cargo test
cargo doc --no-deps --open
```

The authoritative wire contract is Soul Injector's
`components/sidp_client/include/sidp_defs.hpp` and `docs/sidp-protocol.md`.
This implementation uses explicit little-endian fields, a maximum 8192-byte
frame, CRC-32/ISO-HDLC, and bounded SLIP decoding. No native structure layout
or unsafe casting is used.

## License

This project is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE.md).
Commercial use requires a separate written commercial license from the copyright holder.

# Wire protocol primitives

The `protocol` module provides transport-neutral byte codecs used by Git's
protocol versions 0, 1, and 2. It performs no socket, process, or CLI access, so
an application can carry the resulting bytes over TCP, HTTP, SSH, an in-memory
channel, or its own transport.

## Pkt-line

`PktLine` represents data plus the three control packets: flush (`0000`),
delimiter (`0001`), and response-end (`0002`). Encoding enforces Git's 65,520
byte complete-packet limit.

`PktLineDecoder` accepts arbitrary chunks and returns a packet only after all of
its declared bytes arrive. Call `finish` at transport EOF to distinguish a
clean boundary from a truncated packet.

```rust
use git_rs::{PktLine, PktLineDecoder};

let encoded = PktLine::Data(b"want 0123456789abcdef\n".to_vec()).encode()?;
let mut decoder = PktLineDecoder::new();
for chunk in encoded.chunks(3) {
    decoder.extend(chunk);
}
assert_eq!(
    decoder.next_packet()?,
    Some(PktLine::Data(b"want 0123456789abcdef\n".to_vec()))
);
decoder.finish()?;
# Ok::<(), git_rs::Error>(())
```

Lengths `0001` and `0002` are control packets, `0003` is rejected as reserved,
and lengths from `0004` through `fff0` include the four-byte header. Both cases
of hexadecimal input are accepted. Malformed, oversized, and truncated packets
return `Error::Protocol` rather than panicking.

## Sideband and capabilities

`Sideband` encodes and decodes channel 1 (pack data), channel 2 (progress), and
channel 3 (fatal remote error). Unknown channels and empty sideband packets are
rejected.

`Capability::parse_list` parses the space-separated version-0/1 capability
tail into unique `name` and optional `value` components. Capability bytes must
be ASCII and cannot contain control characters.

## Git source comparison

The implementation and tests were compared with:

- `pkt-line.h:LARGE_PACKET_MAX` for the 65,520-byte limit;
- `pkt-line.c:packet_length` and `packet_read_with_status` for hexadecimal
  lengths, control packets, invalid length 3, and truncation behavior;
- `pkt-line.c:do_packet_write` for data framing and maximum payload;
- `sideband.h` and `sideband.c:demultiplex_sideband` for channel semantics;
- `connect.c:parse_feature_value` for capability name/value handling.

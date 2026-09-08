# AirPlay 2 to a HomePod, as DeetsAirplay speaks it

The wire recipe this repo implements, pieced together from the receiver side
(shairport-sync, openairplay/airplay2-receiver) and three working senders
(OwnTone's `airplay.c`, pyatv, akustikrausch/airplay2-sender-cpp). Nothing here
is copied; it is what those projects agree on, written down once so the Rust
can be read against it. Read this before touching `src-tauri/src/airplay/`.

## 0. Prerequisites on the HomePod

Home app → the HomePod → **Allow Speaker & TV Access** must be *Everyone* or
*Anyone on the Same Network*. That is what enables HAP **transient** pairing
(feature bit 48 in the mDNS `features` TXT value). With *Only People Sharing
This Home* the speaker answers pair-setup with 470 and wants a real HomeKit
pairing, which needs keys from the Home, not from a developer account.

No Apple developer account is involved anywhere below.

## 1. Discovery (`mdns.rs`)

Query `_airplay._tcp.local` PTR on 224.0.0.251:5353. The answer set carries
PTR → instance, SRV → host + port (7000 on a HomePod), TXT, and an A record
for the host. TXT keys we read: `deviceid`, `model` (`AudioAccessory5,1` is a
HomePod 2, `AudioAccessory1,1` the original), `features` ("0xLOW,0xHIGH"),
`pk`, `pi`, `flags`, `srcvers`.

## 2. Control channel (`rtsp.rs`)

One TCP connection to the SRV port. Requests are HTTP-shaped text; the
receiver parses both `POST /pair-setup HTTP/1.1` and
`SETUP rtsp://… RTSP/1.0` on the same socket. Every request carries:

```
CSeq, User-Agent: AirPlay/550.10, DACP-ID, Active-Remote,
Client-Instance (= DACP-ID), X-Apple-Client-Name
```

After pairing, the socket flips to encrypted framing in both directions:

```
[u16 LE length][ciphertext][16-byte Poly1305 tag]
```

at most 1024 plaintext bytes per frame, the 2-byte length as AAD, nonce =
4 zero bytes + 8-byte little-endian counter, one counter per direction.
Keys: `HKDF-SHA512(K, salt "Control-Salt", info "Control-Write-Encryption-Key"
| "Control-Read-Encryption-Key", 32)`. "Write" is the sender's write key.

## 3. Pairing (`pairing.rs`, `crypto/srp.rs`)

`POST /pair-setup`, `Content-Type: application/octet-stream`, header
`X-Apple-HKP: 4` (4 = transient). TLV8 bodies:

| msg | from | TLV records |
|---|---|---|
| M1 | us | State=1, Method=0, Flags=0x10 (transient) |
| M2 | speaker | State=2, Salt (16), PublicKey B (384) |
| M3 | us | State=3, PublicKey A (384), Proof M1 (64) |
| M4 | speaker | State=4, Proof M2 (64) |

SRP-6a, RFC 5054 3072-bit group, g = 5, SHA-512, I = "Pair-Setup",
P = "3939". Apple padding: `k = H(pad(N)‖pad(g))`, `u = H(pad(A)‖pad(B))`
padded to 384 bytes; `x = H(s ‖ H("Pair-Setup:3939"))`;
`S = (B − k·g^x)^(a + u·x) mod N`; **K = H(S)** (64 bytes, S unpadded);
`M1 = H( H(N) xor H(g) ‖ H(I) ‖ s ‖ A ‖ B ‖ K )` (A, B unpadded);
`M2 = H(A ‖ M1 ‖ K)`.

Transient pairing stops at M4. There is no pair-verify. From K:

| key | derivation |
|---|---|
| control write / read | HKDF "Control-Salt" / "Control-*-Encryption-Key" |
| event read / write | HKDF "Events-Salt" / "Events-Write-…" (we READ) and "Events-Read-…" (we WRITE): swapped, the receiver initiates |
| audio (`shk`) | **first 32 bytes of K, raw, no HKDF** |

The 64-vs-32 clamp is the classic "pairs, connects, silent" bug.

## 4. Session (`session.rs`)

All over the encrypted control channel, RTSP/1.0, URI
`rtsp://<our ip>/<random u32 session id>`:

1. `GET /info` — required before SETUP; reply is a bplist of capabilities.
2. `SETUP` (session), body bplist, header `X-Apple-StreamID: 1`:
   `deviceID`, `sessionUUID`, `timingPort` (ours), `timingProtocol: "NTP"`,
   `isMultiSelectAirPlay`, `groupContainsGroupLeader: false`, `macAddress`,
   `model`, `name`, `osBuildVersion`, `osName`, `osVersion`,
   `senderSupportsRelay: false`, `sourceVersion`, `statsCollectionEnabled`.
   Reply: `eventPort`, `timingPort`.
3. TCP-connect to `eventPort`. The receiver pushes encrypted requests on it
   (`POST /command` updateInfo, etc.). Answer each with a bare
   `RTSP/1.0 200 OK` + `Server` + `CSeq` and nothing else. **No
   `Content-Length: 0`, no `Audio-Latency`**: those corrupt its realtime
   timeline. Unanswered, the session dies at ~25–30 s.
4. `RECORD` (empty). Some receivers answer 500; non-fatal.
5. `SETUP` (stream), body `{streams: [{…}]}`:
   `type: 96` (realtime), `ct: 2`, `audioFormat: 0x40000` (ALAC 44.1/16/2),
   `spf: 352`, `sr: 44100`, `shk: <32 bytes>`, `controlPort` (ours),
   `latencyMin: 11025`, `latencyMax: 88200`, `audioMode: "default"`,
   `isMedia: true`, `supportsDynamicStreamID: false`,
   `streamConnectionID: <session id>`.
   Reply: `streams[0].dataPort`, `.controlPort`.
6. `SET_PARAMETER`, `text/parameters`, body `volume: -15.000000`.
   0 % → −144 dB (mute); else `(pct·3 − 300)/10`, i.e. −30..0 dB.
7. `POST /feedback` every 2 s: the keep-alive. Its round trip is the RTT
   the panel shows.
8. `TEARDOWN` to end.

## 5. Audio (`alac.rs`, `rtp.rs`, pacer thread)

UDP to `dataPort`, one packet per 352 frames (7.98 ms):

```
12-byte RTP header  80 60|E0  seq(be16)  rtptime(be32)  ssrc(be32)
ciphertext ‖ 16-byte tag ‖ 8-byte nonce
```

`E0` = marker, first packet only. SSRC = session id. Payload before
encryption is an **uncompressed ALAC frame** (the receiver hard-codes ALAC
for type 96 and ignores `ct`): MSB-first bits
`001` `0000` `000000000000` `0` `00` `1` then 352 × {L16, R16} then `111`,
zero-padded to a byte. Encryption: ChaCha20-Poly1305 with `shk`, nonce =
counter starting at 0 (LE, padded to 12 with zero prefix), **AAD = header
bytes 4..12** (rtptime + ssrc); the 8-byte nonce is appended after the tag.

Timeline: `rtptime = latency + frames_sent`. The pacer is a wall-clock token
bucket at 44.1 kHz; it sends silence when the capture ring is empty so the
timeline never stalls.

Retransmits: the receiver sends type 0x55 `[.. .. .. .. first_seq(be16)
count(be16)]` to our control port; we answer `80 D6 seq ‖ original packet`
from a 1024-packet backlog.

## 6. Timing and sync

- **NTP timing** (our `timingPort`, UDP): the receiver sends 32-byte
  requests (type 0x52); we reply type 0x53 echoing its send time as our
  reference and stamping receive/send with NTP now. Windows Firewall must
  allow this inbound UDP; it is unsolicited from the PC's point of view.
- **Sync packets** (to the receiver's `controlPort`, once a second, first
  one with the marker bit `90`):
  `90|80 D4 0007 now−latency(be32) ntp(be64) now(be32)`
  where `now = latency + frames_sent` and
  `ntp = ts_to_ntp(start_ts + frames_sent)`, `start_ts = ntp_to_ts(NTP at
  stream start)`. Formulas: `ntp_to_ts(n) = ((n>>16)·44100)>>16`,
  `ts_to_ntp(t) = ((t<<16)/44100)<<16`.

The receiver buffers exactly `latency` frames. 11025 (250 ms) is the
advertised floor; the panel's Auto mode starts at 300 ms.

PTP (the multi-room protocol) needs UDP 319/320, privileged ports, and is
not needed for a single HomePod. We advertise NTP and never send SETPEERS.

## 7. Things that look wrong but are right

- `X-Apple-HKP: 4`, not 3, for transient.
- The event channel keys are swapped relative to the control channel.
- The audio key is the raw secret, not an HKDF output.
- `Content-Length` is sent on the HTTP-style POSTs; RTSP requests with an
  empty body omit it.
- The SRP `M1` hashes A and B **unpadded** while `k` and `u` pad them.

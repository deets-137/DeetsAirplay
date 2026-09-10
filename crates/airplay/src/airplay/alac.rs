//! Uncompressed ALAC framing. The AirPlay 2 realtime stream (type 96) is
//! decoded as ALAC by the receiver no matter what `ct` says, and ALAC has an
//! "escape" form that carries raw PCM. So there is no encoder here, only a
//! bit-packer: the receiver reads MSB-first.
//!
//! Frame layout for one stereo channel-pair element:
//!   3b element tag = 1 (CPE)  . 4b unused = 0 . 12b unknown = 0
//!   1b hasSize = 0 (the cookie's 352)  . 2b wastedBytes = 0
//!   1b isNotCompressed = 1
//!   then frames x { L16, R16 } MSB-first
//!   3b END tag = 7, zero-pad to a byte.

pub const FRAMES_PER_PACKET: usize = 352;
pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;

struct BitWriter {
    out: Vec<u8>,
    cur: u32,
    filled: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.cur = (self.cur << 1) | ((value >> i) & 1);
            self.filled += 1;
            if self.filled == 8 {
                self.out.push(self.cur as u8);
                self.cur = 0;
                self.filled = 0;
            }
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.out.push((self.cur << (8 - self.filled)) as u8);
        }
        self.out
    }
}

/// `frames` is interleaved L/R i16, exactly FRAMES_PER_PACKET * CHANNELS long.
pub fn pack_frame(frames: &[i16]) -> Vec<u8> {
    debug_assert_eq!(frames.len(), FRAMES_PER_PACKET * CHANNELS);
    let mut w = BitWriter { out: Vec::with_capacity(frames.len() * 2 + 8), cur: 0, filled: 0 };
    w.put(1, 3); // stereo channel-pair element
    w.put(0, 4); // unused
    w.put(0, 12); // unknown
    w.put(0, 1); // hasSize = 0
    w.put(0, 2); // wastedBytes = 0
    w.put(1, 1); // isNotCompressed
    for s in frames {
        w.put(*s as u16 as u32, 16);
    }
    w.put(7, 3); // END
    w.finish()
}

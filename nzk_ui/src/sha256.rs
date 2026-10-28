//! Minimal SHA-256 (FIPS 180-4), pure safe Rust, no dependencies.
//!
//! Why hand-rolled: the crate's dependency set is vendored (`vendor/` plus a
//! generated `Cargo.lock`), so pulling in `sha2` means re-vendoring the tree
//! for one 60-line function. This is not security-sensitive — it is a
//! paint↔geometry correspondence token, so a collision only means the D2 drift
//! guard accepts one wrong frame, and the input is not attacker-controlled.
//!
//! Correctness is pinned by the NIST test vectors in the `tests` module below
//! (empty string, "abc", the 56-byte two-block case, and a long input), so this
//! is measured, not asserted.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Streaming SHA-256 so a full-frame RGBA buffer never needs a second copy.
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        // Top up a partial block first.
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        // Whole blocks straight from the input.
        let mut chunks = data.chunks_exact(64);
        for block in &mut chunks {
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            self.buf[..rem.len()].copy_from_slice(rem);
            self.buf_len = rem.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self
            .state
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h].into_iter())
        {
            *s = s.wrapping_add(v);
        }
    }

    /// Finish and return the 32-byte digest.
    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        // Padding: 0x80, then zeros, so that len ≡ 56 (mod 64), then the
        // 64-bit big-endian bit length.
        self.update(&[0x80]);
        // `update` counted the padding byte; the length covers only the message.
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        let mut len_block = [0u8; 8];
        len_block.copy_from_slice(&bit_len.to_be_bytes());
        // Feed the length without letting it change the accounting.
        self.total_len = self.total_len.wrapping_sub(8);
        self.update(&len_block);

        let mut out = [0u8; 32];
        for (i, v) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    /// Lowercase hex digest, the wire form used by `@frame` rows.
    pub fn finish_hex(self) -> String {
        let d = self.finish();
        let mut s = String::with_capacity(64);
        for b in d {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }
}

/// The `@frame` correspondence digest for a painted RGBA8 buffer.
///
/// **This is FNV-1a 64-bit, not SHA-256, and that is deliberate.** The host's
/// drift gate (`app/src/nzk_ui.rs::digest_rgba`) recomputes the digest over the
/// bytes it decoded from the PNG and compares it to this value, so the two sides
/// must use the same function or every frame is reported as drift. The gate's
/// cheaper fold is the canonical wire format; the shell matches it:
///
/// ```text
/// h = 0xcbf29ce484222325
/// for each byte b: h ^= b; h = h.wrapping_mul(0x100000001b3)
/// h ^= len;             h = h.wrapping_mul(0x100000001b3)
/// format!("{h:016x}")
/// ```
///
/// The length is folded in last so trailing zeroes cannot collide. This detects
/// "are these the same pixels", not a security property; [`Sha256`] remains
/// available (and NIST-vector tested) for callers that want one.
pub fn frame_digest(rgba: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in rgba {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= rgba.len() as u64;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    format!("{h:016x}")
}

/// One-shot convenience.
pub fn hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finish_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format the host's `digest_rgba` recomputes. Changing this
    /// function without changing `app/src/nzk_ui.rs::digest_rgba` makes every
    /// frame look like drift, so the shape (16 lowercase hex) is pinned here.
    #[test]
    fn frame_digest_is_fnv1a64_with_length_fold() {
        let d = frame_digest(&[0u8; 4]);
        assert_eq!(d.len(), 16, "must be 16 hex chars, got {d:?}");
        assert!(
            d.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "must be lowercase hex: {d:?}"
        );
        // Recompute independently: the length fold is the part that is easy to
        // drop, so a plain-FNV implementation must NOT match.
        let plain = {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for _ in 0..4 {
                h ^= 0u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            format!("{h:016x}")
        };
        assert_ne!(d, plain, "length fold missing — plain FNV is not the contract");
    }

    /// The length fold must distinguish a buffer from itself plus trailing zeros;
    /// without it those two would collide and a resized frame could pass the gate.
    #[test]
    fn frame_digest_separates_trailing_zeroes() {
        assert_ne!(frame_digest(&[1, 2, 3]), frame_digest(&[1, 2, 3, 0]));
        assert_ne!(frame_digest(&[0u8; 8]), frame_digest(&[0u8; 9]));
    }

    /// Any single-bit change in the pixels must change the digest.
    #[test]
    fn frame_digest_detects_pixel_change() {
        let a = vec![0u8; 1600 * 4];
        let mut b = a.clone();
        b[1234] = 1;
        assert_ne!(frame_digest(&a), frame_digest(&b));
    }

    #[test]
    fn nist_vectors() {        // FIPS 180-4 / NIST examples.
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn one_million_a() {
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            h.finish_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// A frame-sized buffer exercises the multi-block path (1.6k x 1k x 4 = 6.4MB)
    /// and pins that chunked updates equal a single update.
    #[test]
    fn streaming_matches_one_shot_on_frame_sized_input() {
        let data: Vec<u8> = (0..(1600 * 1000 * 4) as u32).map(|i| (i % 251) as u8).collect();
        let one = hex(&data);
        let mut h = Sha256::new();
        for chunk in data.chunks(7919) {
            h.update(chunk);
        }
        assert_eq!(one, h.finish_hex());
    }

    /// Every block alignment and tail length must agree with the padded path.
    #[test]
    fn all_tail_lengths_agree() {
        let data: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
        for n in 0..=data.len() {
            let one = hex(&data[..n]);
            let mut h = Sha256::new();
            // Split at an awkward point to exercise the partial-block top-up.
            let (a, b) = data[..n].split_at(n / 3);
            h.update(a);
            h.update(b);
            assert_eq!(one, h.finish_hex(), "length {n}");
        }
    }
}

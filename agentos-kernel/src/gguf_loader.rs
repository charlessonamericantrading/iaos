use crate::kprintln;
use crate::serial_println;
use crate::tensor_engine::TensorEngine;
use alloc::string::String;
use alloc::vec::Vec;

pub const GGUF_MAGIC: u32 = 0x46554747; // "GGUF" in Little Endian

// Q4_K keeps GGML's real name (underscore included, matching Q4_0/Q4_1/
// Q8_0 already here) rather than the "Q4K" rustc's non_camel_case_types
// lint would prefer - real GGUF/GGML terminology over cosmetic style.
#[allow(dead_code, non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GgufGtype {
    F32 = 0,
    F16 = 1,
    Q4_0 = 2,
    Q4_1 = 3,
    Q8_0 = 8,
    Q2_K = 10,
    Q3_K = 11,
    Q4_K = 12,
    Q5_K = 13,
    Q6_K = 14,
    Q8_K = 15,
}

impl GgufGtype {
    fn from_u32(raw: u32) -> Result<Self, &'static str> {
        match raw {
            0 => Ok(GgufGtype::F32),
            1 => Ok(GgufGtype::F16),
            2 => Ok(GgufGtype::Q4_0),
            3 => Ok(GgufGtype::Q4_1),
            8 => Ok(GgufGtype::Q8_0),
            10 => Ok(GgufGtype::Q2_K),
            11 => Ok(GgufGtype::Q3_K),
            12 => Ok(GgufGtype::Q4_K),
            13 => Ok(GgufGtype::Q5_K),
            14 => Ok(GgufGtype::Q6_K),
            15 => Ok(GgufGtype::Q8_K),
            _ => Err("GGUF: unknown tensor gtype"),
        }
    }
}

pub struct GgufTensorInfo {
    pub name: String,
    pub dimensions: [usize; 2],
    pub gtype: GgufGtype,
    pub offset: usize,
}

impl GgufTensorInfo {
    /// Parses one tensor-info entry from `bytes` (starting at its own
    /// first byte, not the file start) - a real GGUF tensor-info
    /// entry's actual shape (length-prefixed name, dimensions,
    /// quantization type, a byte offset to that tensor's own data
    /// elsewhere in the file), simplified in two ways for this kernel:
    /// `u32` fields throughout (real GGUF uses `u64` - more range than
    /// anything this kernel's own toy tensors will ever need) and fixed
    /// at exactly 2 dimensions (matching this struct's own `[usize; 2]`
    /// shape). Returns the entry AND how many bytes it consumed, so a
    /// caller parsing several entries in sequence knows where the next
    /// one starts - not exercised yet (this kernel only ever constructs
    /// one), but free to support given the entry's own length is
    /// already known once parsed.
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), &'static str> {
        if bytes.len() < 4 {
            return Err("GGUF: tensor-info buffer too small for name length");
        }
        let name_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let fields_start = 4 + name_len;
        if bytes.len() < fields_start + 16 {
            return Err("GGUF: tensor-info buffer too small for name + fields");
        }

        let name = core::str::from_utf8(&bytes[4..fields_start])
            .map_err(|_| "GGUF: tensor name is not valid UTF-8")?;

        let read_u32 = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let dim0 = read_u32(fields_start) as usize;
        let dim1 = read_u32(fields_start + 4) as usize;
        let gtype = GgufGtype::from_u32(read_u32(fields_start + 8))?;
        let offset = read_u32(fields_start + 12) as usize;

        Ok((
            GgufTensorInfo {
                name: String::from(name),
                dimensions: [dim0, dim1],
                gtype,
                offset,
            },
            fields_start + 16,
        ))
    }

    /// Parses `count` consecutive tensor-info entries in sequence,
    /// starting at `bytes`' first byte - the real multi-tensor case every
    /// actual GGUF file has (a model with any real number of tensors, not
    /// just one), using `parse`'s own "bytes consumed" return value to
    /// advance through the buffer each call rather than assuming a fixed
    /// entry size (entries are variable-length: each has its own name).
    /// Returns the parsed entries alongside the TOTAL bytes consumed
    /// across all of them, so a caller knows exactly where the tensor-
    /// info section ends and tensor data begins - `parse` already
    /// returned this same kind of value per-entry specifically so this
    /// case could be built on top of it without changing `parse` itself.
    pub fn parse_many(bytes: &[u8], count: usize) -> Result<(Vec<Self>, usize), &'static str> {
        let mut infos = Vec::with_capacity(count);
        let mut offset = 0;
        for _ in 0..count {
            let remaining = bytes
                .get(offset..)
                .ok_or("GGUF: tensor-info offset past end of buffer")?;
            let (info, consumed) = GgufTensorInfo::parse(remaining)?;
            offset += consumed;
            infos.push(info);
        }
        Ok((infos, offset))
    }
}

/// Converts an IEEE 754 half-precision (binary16) bit pattern to `f32` -
/// needed because Q8_0's block scale is stored as a real GGML `ggml_fp16_t`
/// and this `#![no_std]` build has no `f16` type or conversion available
/// from std/an external crate. Hand-verified against each of binary16's
/// defined cases (zero, subnormal, normal, infinity) independently, not
/// just a single from-memory formula: a wrong exponent bias or subnormal-
/// handling bug would silently produce a plausible-looking but wrong
/// float, not a crash - the same class of silent-failure risk Fase 47's
/// checksum work was written to guard against.
///
/// Layout: 1 sign bit, 5 exponent bits (bias 15), 10 mantissa bits.
/// Subnormals (exp16 == 0, mantissa != 0) are normalized by shifting left
/// until the implicit leading bit appears, tracking the resulting
/// exponent - binary16's entire subnormal range (down to 2^-24) fits well
/// within f32's normal exponent range, so the result is always a normal
/// f32, never itself subnormal.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = (bits & 0x8000) as u32;
    let exp16 = (bits >> 10) & 0x1F;
    let mant16 = (bits & 0x3FF) as u32;

    let (exp32, mant32): (u32, u32) = if exp16 == 0 {
        if mant16 == 0 {
            (0, 0) // zero
        } else {
            let mut m = mant16;
            let mut e: i32 = -14;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            ((e + 127) as u32, (m & 0x3FF) << 13)
        }
    } else if exp16 == 0x1F {
        (0xFF, mant16 << 13) // infinity (mant16=0) or NaN (mant16!=0)
    } else {
        (exp16 as u32 + (127 - 15), mant16 << 13) // normal
    };

    f32::from_bits((sign << 16) | (exp32 << 23) | mant32)
}

/// Extracts sub-block `j`'s (0..8) 6-bit scale and 6-bit min out of Q4_K's
/// packed `scales[12]` array - GGML's own `get_scale_min_k4`, ported
/// verbatim (down to the exact index arithmetic) rather than re-derived,
/// since matching the real function bit-for-bit is the whole point here.
/// The packing itself: bytes `0..4` hold sub-blocks `0..4`'s scales in
/// their low 6 bits; bytes `4..8` hold sub-blocks `0..4`'s mins in their
/// low 6 bits; bytes `8..12` hold sub-blocks `4..8`'s scales (low nibble)
/// and mins (high nibble) in their low 4 bits each - and sub-blocks
/// `4..8`'s own missing top 2 bits are smuggled into the otherwise-unused
/// top 2 bits of bytes `0..4`/`4..8` (which the `& 63` mask above never
/// touches). Twelve bytes hold exactly sixteen 6-bit values (96 bits)
/// with zero bits wasted or reused.
///
/// Independently hand-verified with a complete round trip (all 8 sub-
/// blocks, both branches of the `j < 4` split) before being trusted:
/// encoding `sc = [7,14,21,28,35,42,49,56]`, `m = [56,49,42,35,28,21,14,7]`
/// into `scales = [135,142,213,220,120,113,42,35,195,90,225,120]` by
/// inverting this exact formula, then re-decoding those bytes through it,
/// recovered every original value exactly - not just plausible, but
/// bit-for-bit confirmed self-consistent, given an earlier fetch of this
/// struct's own field sizes (`scales[8]`/`qs[64]`) turned out to be wrong
/// (the real sizes are `scales[12]`/`qs[128]`, confirmed via GGML's own
/// `K_SCALE_SIZE`/`QK_K` constants) - a demonstrated real error rate that
/// made trusting this specific function's fetched logic without an
/// independent check the wrong call.
pub fn get_scale_min_k4(j: usize, q: &[u8]) -> (u8, u8) {
    if j < 4 {
        (q[j] & 63, q[j + 4] & 63)
    } else {
        let d = (q[j + 4] & 0x0F) | ((q[j - 4] >> 6) << 4);
        let m = (q[j + 4] >> 4) | ((q[j] >> 6) << 4);
        (d, m)
    }
}

pub struct GgufModelLoader {
    pub magic: u32,
    pub version: u32,
    pub tensor_count: usize,
    pub kv_count: usize,
    pub architecture: &'static str,
    pub context_length: usize,
    pub embedding_length: usize,
}

impl GgufModelLoader {
    pub fn parse_header(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < 24 {
            return Err("Header buffer too small");
        }

        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != GGUF_MAGIC {
            return Err("Invalid GGUF Magic Header (expected 'GGUF')");
        }

        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let tensor_count = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]) as usize;

        let kv_count = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]) as usize;

        kprintln!("[GGUF PARSER] Magic: 0x{:X} (Valid GGUF)", magic);
        kprintln!(
            "[GGUF PARSER] Version: {}, Tensors: {}, KV Metadata: {}",
            version,
            tensor_count,
            kv_count
        );
        serial_println!(
            "[GGUF PARSER] Magic OK. Version: {}, Tensors: {}",
            version,
            tensor_count
        );

        Ok(GgufModelLoader {
            magic,
            version,
            tensor_count,
            kv_count,
            architecture: "llama",
            context_length: 4096,
            embedding_length: 2048,
        })
    }

    /// Decodes a byte buffer into little-endian `f32` values - the raw
    /// tensor data a `GgufTensorInfo`'s own `offset` field points to.
    /// Doesn't know or care about quantization: every value here is
    /// assumed already `F32`. Any trailing bytes that don't form a
    /// complete 4-byte group are silently dropped rather than erroring -
    /// the caller already knows how many values it expects from the
    /// tensor's own parsed dimensions.
    pub fn decode_f32_le(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| f32::from_le_bytes(c))
            .collect()
    }

    /// Decodes `bytes` as real GGML/GGUF Q8_0-quantized data - the
    /// simplest of GGUF's actual quantized formats (real GGUF's whole
    /// point as a format), and the first one this kernel decodes for
    /// real rather than assuming everything is `F32`. Each 34-byte block
    /// is GGML's own `block_q8_0` layout (`{ ggml_fp16_t d; int8_t
    /// qs[32]; }`, `QK8_0 = 32`) - a 2-byte half-precision scale `d`
    /// followed by 32 signed 8-bit quantized values, verified against
    /// GGML's actual struct definition rather than relied on from memory.
    /// Each real value is `qs[i] as f32 * d` - one shared scale per
    /// 32-value block, not a per-value scale. Any trailing bytes that
    /// don't form a complete 34-byte block are silently dropped, same
    /// convention as `decode_f32_le`.
    pub fn decode_q8_0(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 34; // 2-byte f16 scale + 32 i8 values
        const VALUES_PER_BLOCK: usize = 32;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * VALUES_PER_BLOCK);
        for block in blocks {
            let scale = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));
            for &byte in &block[2..BLOCK_SIZE] {
                values.push((byte as i8) as f32 * scale);
            }
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q4_0-quantized data - 4-bit,
    /// nibble-packed, the smallest of GGUF's quantized formats. Each
    /// 18-byte block is GGML's own `block_q4_0` layout (`{ ggml_fp16_t
    /// d; uint8_t qs[16]; }`, `QK4_0 = 32`) - a 2-byte half-precision
    /// scale followed by 16 bytes packing 32 signed 4-bit values, two per
    /// byte. **Critically, packing is split-half, not interleaved**:
    /// byte `qs[j]`'s low nibble is `value[j]` and its high nibble is
    /// `value[j+16]`, for `j` in `0..16` - verified directly against
    /// GGML's actual `dequantize_row_q4_0` source rather than assumed
    /// from a naive guess (an interleaved `value[2j]`/`value[2j+1]`
    /// packing would have been an equally plausible-looking but wrong
    /// assumption, silently scrambling every decoded tensor rather than
    /// erroring). Each real value is `((nibble as i32) - 8) as f32 *
    /// scale` - the unsigned 0-15 nibble recentered to a signed -8..7
    /// range before scaling. Any trailing bytes that don't form a
    /// complete 18-byte block are silently dropped, same convention as
    /// `decode_f32_le`/`decode_q8_0`.
    pub fn decode_q4_0(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 18; // 2-byte f16 scale + 16 bytes (32 packed nibbles)
        const HALF: usize = 16;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * HALF * 2);
        for block in blocks {
            let scale = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));
            let qs = &block[2..BLOCK_SIZE];
            let mut block_values = [0.0f32; HALF * 2];
            for j in 0..HALF {
                let low = (qs[j] & 0x0F) as i32 - 8;
                let high = (qs[j] >> 4) as i32 - 8;
                block_values[j] = low as f32 * scale;
                block_values[j + HALF] = high as f32 * scale;
            }
            values.extend_from_slice(&block_values);
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q4_1-quantized data - the same
    /// 4-bit, split-half nibble packing as `Q4_0`, but with a full affine
    /// (scale AND offset) dequantization instead of `Q4_0`'s symmetric
    /// `(nibble-8)*scale`. Each 20-byte block is GGML's own `block_q4_1`
    /// layout (`{ ggml_fp16_t d; ggml_fp16_t m; uint8_t qs[16]; }`) - two
    /// half-precision fields (a scale `d` and a minimum `m`) followed by
    /// the same 16-byte split-half nibble packing `Q4_0` uses. Verified
    /// against GGML's actual `dequantize_row_q4_1` source rather than
    /// assumed to carry over unchanged from `Q4_0` - it does, but `Q4_0`'s
    /// own packing order was itself non-obvious enough to have needed
    /// verification, so this wasn't taken on faith either. Each real
    /// value is `nibble as f32 * scale + min` - the raw unsigned 0-15
    /// nibble used directly (no `-8` recentering; `min` supplies whatever
    /// offset the block actually needs, letting `Q4_1` represent
    /// asymmetric value distributions `Q4_0`'s fixed symmetric range
    /// can't). Any trailing bytes that don't form a complete 20-byte
    /// block are silently dropped, same convention as the other decoders.
    pub fn decode_q4_1(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 20; // 2-byte scale + 2-byte min + 16 bytes (32 packed nibbles)
        const HALF: usize = 16;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * HALF * 2);
        for block in blocks {
            let scale = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));
            let min = f16_to_f32(u16::from_le_bytes([block[2], block[3]]));
            let qs = &block[4..BLOCK_SIZE];
            let mut block_values = [0.0f32; HALF * 2];
            for j in 0..HALF {
                let low = qs[j] & 0x0F;
                let high = qs[j] >> 4;
                block_values[j] = low as f32 * scale + min;
                block_values[j + HALF] = high as f32 * scale + min;
            }
            values.extend_from_slice(&block_values);
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q4_K-quantized data - a "K-quant"
    /// super-block format, structurally different from (and meaningfully
    /// more complex than) `Q8_0`/`Q4_0`/`Q4_1` above: each 144-byte block
    /// represents 256 values (not 32), split into 8 sub-blocks of 32
    /// values each, and - the real point of the format - each sub-block
    /// gets its OWN 6-bit scale and 6-bit min rather than one scale for
    /// the whole block, letting different regions of a 256-value run be
    /// quantized more accurately than a single shared scale ever could.
    ///
    /// Struct layout (144 bytes, verified against GGML's actual
    /// `block_q4_K` - `K_SCALE_SIZE=12`, `QK_K=256`): 2-byte `d` (super-
    /// block scale) + 2-byte `dmin` (super-block min-scale) +
    /// `scales[12]` (16 packed 6-bit values: 8 sub-block scales and 8
    /// sub-block mins) + `qs[128]` (256 4-bit values, two per byte,
    /// split-half within each 32-byte chunk - the same low/high-nibble
    /// convention `decode_q4_0`/`decode_q4_1` already use, just applied
    /// per-32-byte-chunk instead of across the whole array).
    ///
    /// An earlier fetch of this same struct's field sizes turned out to
    /// contain wrong numbers (`scales[8]`/`qs[64]` instead of the real
    /// `scales[12]`/`qs[128]` - caught by cross-checking against GGML's
    /// own `K_SCALE_SIZE`/`QK_K` constants, which don't agree with those
    /// smaller sizes). Given that discovery, `get_scale_min_k4`'s own
    /// intricate 6-bit-packing logic (see its own doc) was independently
    /// hand-verified with a complete round trip - not just re-fetched a
    /// second time and trusted - before any of this was implemented.
    ///
    /// Each real value is `(d * sub_scale) * nibble - (dmin * sub_min)` -
    /// an affine dequantization like `Q4_1`'s, but with the scale AND min
    /// themselves varying per 32-value sub-block rather than fixed for
    /// the whole 256-value super-block.
    pub fn decode_q4_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 144; // 4 (d+dmin) + 12 (scales) + 128 (qs)
        const SCALES_LEN: usize = 12;
        const QS_LEN: usize = 128;
        const SUB_BLOCK: usize = 32;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * 256);
        for block in blocks {
            let d = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));
            let dmin = f16_to_f32(u16::from_le_bytes([block[2], block[3]]));
            let scales = &block[4..4 + SCALES_LEN];
            let qs = &block[4 + SCALES_LEN..4 + SCALES_LEN + QS_LEN];

            let mut q_offset = 0;
            for is in (0..8).step_by(2) {
                let (sc1, m1) = get_scale_min_k4(is, scales);
                let (sc2, m2) = get_scale_min_k4(is + 1, scales);
                let d1 = d * sc1 as f32;
                let dm1 = dmin * m1 as f32;
                let d2 = d * sc2 as f32;
                let dm2 = dmin * m2 as f32;

                let chunk = &qs[q_offset..q_offset + SUB_BLOCK];
                for &byte in chunk {
                    values.push(d1 * (byte & 0x0F) as f32 - dm1);
                }
                for &byte in chunk {
                    values.push(d2 * (byte >> 4) as f32 - dm2);
                }
                q_offset += SUB_BLOCK;
            }
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q6_K-quantized data - another
    /// "K-quant" super-block format (256 values per block), but
    /// structurally unlike every format above in three ways, each
    /// checked rather than assumed to follow the established pattern:
    ///
    /// 1. **`d` (the single super-block scale) comes LAST in the struct,
    ///    not first.** Every prior format here (`Q8_0`, `Q4_0`, `Q4_1`,
    ///    `Q4_K`) put its scale field(s) at the start of the block - Q6_K
    ///    breaks that pattern, verified against GGML's actual
    ///    `block_q6_K` definition rather than carried over by habit.
    /// 2. **The 6-bit quantized value is split across TWO separate
    ///    arrays**, `ql` (low 4 bits) and `qh` (high 2 bits), not packed
    ///    into one nibble-pair array like `Q4_0`/`Q4_1`/`Q4_K`.
    /// 3. **`scales[16]` are plain signed `i8` bytes**, not 6-bit-packed
    ///    like `Q4_K`'s `scales[12]` - one full byte per sub-block scale,
    ///    no bit-extraction needed; purely multiplicative (`d * sc * q`),
    ///    with no separate "min" term the way `Q4_1`/`Q4_K`'s affine
    ///    dequantization has.
    ///
    /// Struct layout (210 bytes total, `QK_K=256`): `ql[128]` (`QK_K/2`),
    /// `qh[64]` (`QK_K/4`), `scales[16]` (`QK_K/16`, signed), and `d`
    /// (2-byte f16, LAST) - summing to 128+64+16+2=210. Cross-checked
    /// for internal consistency against GGML's actual
    /// `dequantize_row_q6_K` pointer arithmetic before trusting the
    /// fetched struct size: that function advances
    /// `ql += 64`, `qh += 32`, `sc += 8` per 128-value half-block, over
    /// 2 half-blocks per 256-value block - totalling 128/64/16 bytes
    /// respectively, exactly matching `ql[128]`/`qh[64]`/`scales[16]`.
    ///
    /// Within each 128-value half-block, a 32-iteration inner loop
    /// produces FOUR values per iteration (`q1..q4`, written to `y[l]`,
    /// `y[l+32]`, `y[l+64]`, `y[l+96]`): all four share one `qh[l]` byte,
    /// each taking a different non-overlapping 2-bit field (bits 0-1,
    /// 2-3, 4-5, 6-7 - four 2-bit slices exactly filling one byte),
    /// combined with `ql[l]`/`ql[l+32]`'s low and high nibbles, then
    /// recentered by `-32` (the unsigned 0-63 range becomes signed
    /// -32..31). Hand-traced with concrete byte values
    /// (`ql[0]=0xAB, ql[32]=0xCD, qh[0]=0xE4`) before trusting the
    /// formula: decomposing `0xE4` into its four 2-bit fields (0, 1, 2, 3)
    /// and combining with `ql`'s nibbles gave `q1=-21, q2=-3, q3=10,
    /// q4=28`, independently confirmed by re-summing each field's
    /// weighted contribution back to the original byte.
    pub fn decode_q6_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 210; // 128 (ql) + 64 (qh) + 16 (scales) + 2 (d, LAST)
        const QL_LEN: usize = 128;
        const QH_LEN: usize = 64;
        const SCALES_LEN: usize = 16;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * 256);
        for block in blocks {
            let ql_all = &block[0..QL_LEN];
            let qh_all = &block[QL_LEN..QL_LEN + QH_LEN];
            let scales = &block[QL_LEN + QH_LEN..QL_LEN + QH_LEN + SCALES_LEN];
            let d = f16_to_f32(u16::from_le_bytes([
                block[QL_LEN + QH_LEN + SCALES_LEN],
                block[QL_LEN + QH_LEN + SCALES_LEN + 1],
            ]));

            let mut out = [0.0f32; 256];
            for half in 0..2 {
                let ql = &ql_all[half * 64..half * 64 + 64];
                let qh = &qh_all[half * 32..half * 32 + 32];
                let sc = &scales[half * 8..half * 8 + 8];
                let y = &mut out[half * 128..half * 128 + 128];

                for l in 0..32 {
                    let is = l / 16;
                    let q1 = ((ql[l] & 0x0F) | ((qh[l] & 3) << 4)) as i8 - 32;
                    let q2 = ((ql[l + 32] & 0x0F) | (((qh[l] >> 2) & 3) << 4)) as i8 - 32;
                    let q3 = ((ql[l] >> 4) | (((qh[l] >> 4) & 3) << 4)) as i8 - 32;
                    let q4 = ((ql[l + 32] >> 4) | (((qh[l] >> 6) & 3) << 4)) as i8 - 32;

                    y[l] = d * (sc[is] as i8) as f32 * q1 as f32;
                    y[l + 32] = d * (sc[is + 2] as i8) as f32 * q2 as f32;
                    y[l + 64] = d * (sc[is + 4] as i8) as f32 * q3 as f32;
                    y[l + 96] = d * (sc[is + 6] as i8) as f32 * q4 as f32;
                }
            }
            values.extend_from_slice(&out);
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q8_K-quantized data - the LAST
    /// defined K-quant (`GGML_TYPE_Q8_K = 15`, confirmed against GGML's
    /// real `ggml.h`), and, despite being listed last, structurally the
    /// SIMPLEST of the entire K-quant family by a wide margin: no bit-
    /// packing at all. Confirmed against GGML's actual `ggml-common.h`
    /// struct (fetched directly, not assumed from the format's name or
    /// number - the same discipline `Q2_K`'s own research applied after
    /// getting burned once by an early wrong guess for `Q4_K`):
    /// ```c
    /// typedef struct {
    ///     float   d;              // delta
    ///     int8_t  qs[QK_K];       // quants
    ///     int16_t bsums[QK_K/16]; // sum of quants in groups of 16
    /// } block_q8_K;
    /// ```
    /// Two genuinely distinguishing details worth naming explicitly,
    /// since both would be easy to get wrong by pattern-matching against
    /// every earlier K-quant instead of checking this one's own real
    /// layout: (1) `d` is a plain 4-byte `f32` here, NOT the 2-byte
    /// `ggml_half`/f16 every other K-quant (`Q2_K` through `Q6_K`) uses -
    /// GGML's own comment ("this is only used for intermediate
    /// quantization and dot products") explains why: `Q8_K` exists to
    /// quantize float ACTIVATIONS on the fly for fast dot products
    /// against already-quantized weights, not to compress a model file
    /// for storage, so it has no need to save the extra 2 bytes/block an
    /// f16 delta would. (2) `qs` is `[i8; 256]` - full SIGNED bytes, one
    /// per value, no packing of multiple values into a shared byte at
    /// all - the plainest possible representation, a direct consequence
    /// of the same "speed over size" design goal.
    ///
    /// Dequantization is correspondingly trivial: `value[i] = d *
    /// qs[i] as f32` for all 256 values - a single super-block-wide
    /// scale, no sub-block scale/min lookup, no recentering, no
    /// bit-shifting. `bsums[16]` (precomputed per-16-element sums,
    /// GGML's own fast-dot-product SIMD optimization) is deliberately
    /// NOT used here - it exists purely to skip re-summing `qs` on every
    /// dot product in GGML's own optimized kernels, not to reconstruct
    /// `value[i]` itself, so ignoring it is correct for basic
    /// decode-to-`f32`, not a shortcut that loses information this
    /// decoder actually needs.
    ///
    /// Struct layout (292 bytes, `QK_K=256`): `d` (4-byte f32, FIRST -
    /// unlike `Q3_K`/`Q2_K`/`Q6_K`'s scale-last layout) + `qs[256]`
    /// (`QK_K`, one signed byte per value) + `bsums[16]` (`QK_K/16`,
    /// 2-byte signed sums, present in the struct but unused by this
    /// decoder) - summing to 4+256+32=292.
    pub fn decode_q8_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 292; // 4 (d) + 256 (qs) + 32 (bsums, unused)
        const QS_LEN: usize = 256;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * QS_LEN);
        for block in blocks {
            let d = f32::from_le_bytes([block[0], block[1], block[2], block[3]]);
            let qs = &block[4..4 + QS_LEN];
            for &q in qs {
                values.push(d * (q as i8) as f32);
            }
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q2_K-quantized data - the
    /// FIFTH K-quant format decoded here, and (after `Q3_K`'s SWAR
    /// scale trick) a genuinely welcome return to simplicity: no bit-
    /// packing scheme to derive at all. Each of `scales[16]`'s bytes
    /// holds BOTH a sub-block's scale (low nibble, `sc & 0xF`) AND its
    /// min (high nibble, `sc >> 4`) directly, unpacked, one byte per
    /// sub-block - confirmed against GGML's actual `dequantize_row_
    /// q2_K`, not assumed from the format's name alone (a wrong
    /// assumption here would have been an easy one to reach for, given
    /// `Q3_K`'s scales needed real bit-shuffling to unpack). The
    /// quantized value itself is a plain 2 bits, used directly and
    /// unsigned (`0..3`, no recentering, no companion high-bit array
    /// the way `Q3_K`'s `hmask`/`Q5_K`'s `qh` need) - dequantized
    /// affinely: `value = q_2bits * (d*scale) - (dmin*min)`, the same
    /// shape as `Q4_K`'s formula, just with a trivial scale/min lookup
    /// instead of 6-bit-packed ones.
    ///
    /// Struct layout (84 bytes, `QK_K=256`): `scales[16]` (`QK_K/16`,
    /// one packed byte per sub-block) + `qs[64]` (`QK_K/4`, four 2-bit
    /// values per byte) + `d`+`dmin` (2+2 bytes, LAST - like `Q3_K`/
    /// `Q6_K`, unlike `Q4_K`/`Q5_K`'s scale-first layout). The outer
    /// loop structure (an `n`/`j`/`is`/`shift` interplay, `q` advancing
    /// 32 bytes per `n`-iteration over 2 iterations, `is` running 0..16
    /// across both, `shift` cycling `0,2,4,6` and resetting each `n`)
    /// is structurally identical to `Q3_K`'s own outer loop - the same
    /// pattern, just without a second `hmask`-style array or bit-mask
    /// to track alongside it.
    ///
    /// Verified computationally rather than by hand-tracing alone,
    /// continuing the discipline `Q3_K` introduced: a scratch script
    /// implementing this exact `n`/`j`/`is`/`shift` loop, run against a
    /// deliberately distinct `scales` byte per sub-block (`scale=i`,
    /// `min=15-i` for `i` in `0..16`, using the full 0-15 nibble range)
    /// paired with a uniform `qs` byte (`0xE4`, the same convenient
    /// four-distinct-2-bit-fields byte `Q6_K`'s own test uses) - the
    /// 16 resulting dequantized values were confirmed by the script
    /// before any Rust was written, not derived by hand alone.
    pub fn decode_q2_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 84; // 16(scales)+64(qs)+2(d)+2(dmin), d/dmin LAST
        const SCALES_LEN: usize = 16;
        const QS_LEN: usize = 64;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * 256);
        for block in blocks {
            let scales = &block[0..SCALES_LEN];
            let qs = &block[SCALES_LEN..SCALES_LEN + QS_LEN];
            let d = f16_to_f32(u16::from_le_bytes([
                block[SCALES_LEN + QS_LEN],
                block[SCALES_LEN + QS_LEN + 1],
            ]));
            let dmin = f16_to_f32(u16::from_le_bytes([
                block[SCALES_LEN + QS_LEN + 2],
                block[SCALES_LEN + QS_LEN + 3],
            ]));

            let mut is = 0;
            let mut q_offset = 0;
            for _n in 0..2 {
                for j in 0..4 {
                    let shift = j * 2;

                    let sc = scales[is];
                    is += 1;
                    let dl = d * (sc & 0x0F) as f32;
                    let ml = dmin * (sc >> 4) as f32;
                    for l in 0..16 {
                        let q_bits = (qs[q_offset + l] >> shift) & 3;
                        values.push(dl * q_bits as f32 - ml);
                    }

                    let sc = scales[is];
                    is += 1;
                    let dl = d * (sc & 0x0F) as f32;
                    let ml = dmin * (sc >> 4) as f32;
                    for l in 0..16 {
                        let q_bits = (qs[q_offset + 16 + l] >> shift) & 3;
                        values.push(dl * q_bits as f32 - ml);
                    }
                }
                q_offset += 32;
            }
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q3_K-quantized data - the
    /// FOURTH K-quant format decoded here, and structurally the most
    /// intricate yet: 16 sub-blocks of 16 elements each (not 8 sub-
    /// blocks of 32 like every K-quant above), a 3-bit signed value
    /// (not 4, 5, or 6 bits), and - the genuinely new part - a scale-
    /// reconstruction scheme that is NOT `get_scale_min_k4` (`Q3_K` has
    /// no separate "min" field at all, just one signed scale per
    /// sub-block, closer in spirit to `Q6_K`'s single-scale design than
    /// to `Q4_K`/`Q5_K`'s affine scale-and-min).
    ///
    /// Struct layout (110 bytes, `QK_K=256`): `hmask[32]` (`QK_K/8`,
    /// one high-bit per value) + `qs[64]` (`QK_K/4`, two low bits per
    /// value, four values per byte) + `scales[12]` (16 packed 6-bit
    /// signed scales) + `d` (2-byte f16, LAST - like `Q6_K`, unlike
    /// `Q4_K`/`Q5_K`'s scale-first layout).
    ///
    /// **The scale-unpacking is a real "SWAR" (SIMD-within-a-register)
    /// bit trick, genuinely different from `get_scale_min_k4` and
    /// independently derived and verified, not assumed to be a
    /// variant of it**: GGML's real `dequantize_row_q3_K` treats the
    /// 12-byte `scales` array as three `u32` words, then reconstructs a
    /// FOURTH virtual word by combining each word's own spare high bits
    /// with the third word's bits - a per-byte-lane trick (shift-then-
    /// mask across a 4-byte-wide word) whose result, decomposed back
    /// into individual bytes, gives 16 separate 6-bit unsigned scales.
    /// Reduced to a plain per-index formula (avoiding the SIMD-style
    /// packed-word trick entirely, since a byte-at-a-time formula is
    /// both equivalent and far easier to verify): for `j` in `0..4`,
    /// letting `raw` be the original 12-byte `scales` array,
    /// ```text
    /// scales[j]    = (raw[j]   & 0x0F) | ((raw[8+j] >> 0 & 3) << 4)
    /// scales[4+j]  = (raw[4+j] & 0x0F) | ((raw[8+j] >> 2 & 3) << 4)
    /// scales[8+j]  = (raw[j]   >> 4)   | ((raw[8+j] >> 4 & 3) << 4)
    /// scales[12+j] = (raw[4+j] >> 4)   | ((raw[8+j] >> 6 & 3) << 4)
    /// ```
    /// each reconstructed scale is then used as `scale as i32 - 32`
    /// (the unsigned 0..63 range recentered to signed -32..31, the same
    /// numeric recentering `Q6_K` uses, though there for the VALUE, not
    /// the scale). This formula was NOT trusted from derivation alone:
    /// cross-checked by literally re-implementing GGML's real packed-
    /// word logic in a scratch script and comparing outputs across
    /// 2000 random byte inputs - exact match every time - before
    /// writing any of this Rust.
    ///
    /// The 3-bit value itself: 2 low bits from `qs` (`(qs[l] >> shift)
    /// & 3`, `shift` cycling `0,2,4,6` across a block's 4 iterations,
    /// same non-advancing-pointer-plus-shifting-mask idea `Q5_K` uses
    /// for its own high bit) combined with 1 high bit from `hmask`
    /// (`hmask[l] & m`, `m` a SINGLE shifting bit-mask - unlike `Q5_K`'s
    /// two simultaneous masks - cycling through all 8 bit positions
    /// across a block's 8 total sub-iterations) - recentered by `-4`:
    /// `(2-bit value) - (hmask bit set ? 0 : 4)`, equivalent to
    /// building a full unsigned 3-bit value (`hmask bit as bit 2`) and
    /// subtracting 4, just computed without an intermediate step.
    /// Independently confirmed this whole extraction-and-dequantize
    /// loop, not just the scale reconstruction, against a scratch
    /// reimplementation before trusting it.
    pub fn decode_q3_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 110; // 32(hmask)+64(qs)+12(scales)+2(d, LAST)
        const HMASK_LEN: usize = 32;
        const QS_LEN: usize = 64;
        const SCALES_LEN: usize = 12;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * 256);
        for block in blocks {
            let hmask = &block[0..HMASK_LEN];
            let qs = &block[HMASK_LEN..HMASK_LEN + QS_LEN];
            let raw = &block[HMASK_LEN + QS_LEN..HMASK_LEN + QS_LEN + SCALES_LEN];
            let d = f16_to_f32(u16::from_le_bytes([
                block[HMASK_LEN + QS_LEN + SCALES_LEN],
                block[HMASK_LEN + QS_LEN + SCALES_LEN + 1],
            ]));

            let mut scales = [0u8; 16];
            for j in 0..4 {
                let a_lo = raw[j] & 0x0F;
                let a_hi = raw[j] >> 4;
                let b_lo = raw[4 + j] & 0x0F;
                let b_hi = raw[4 + j] >> 4;
                let byte8 = raw[8 + j];
                scales[j] = a_lo | ((byte8 & 0x03) << 4);
                scales[4 + j] = b_lo | (((byte8 >> 2) & 0x03) << 4);
                scales[8 + j] = a_hi | (((byte8 >> 4) & 0x03) << 4);
                scales[12 + j] = b_hi | (((byte8 >> 6) & 0x03) << 4);
            }

            let mut is = 0;
            let mut m: u8 = 1;
            let mut q_offset = 0;
            for _n in 0..2 {
                for j in 0..4 {
                    let shift = j * 2;

                    let dl = d * (scales[is] as i32 - 32) as f32;
                    is += 1;
                    for l in 0..16 {
                        let q_bits = (qs[q_offset + l] >> shift) & 3;
                        let hm_bit = (hmask[l] & m) != 0;
                        let value = q_bits as i32 - if hm_bit { 0 } else { 4 };
                        values.push(dl * value as f32);
                    }

                    let dl = d * (scales[is] as i32 - 32) as f32;
                    is += 1;
                    for l in 0..16 {
                        let q_bits = (qs[q_offset + 16 + l] >> shift) & 3;
                        let hm_bit = (hmask[16 + l] & m) != 0;
                        let value = q_bits as i32 - if hm_bit { 0 } else { 4 };
                        values.push(dl * value as f32);
                    }

                    m <<= 1;
                }
                q_offset += 32;
            }
        }
        values
    }

    /// Decodes `bytes` as real GGML/GGUF Q5_K-quantized data - a third
    /// K-quant super-block format, structurally much closer to `Q4_K`
    /// than to `Q6_K`: `d`/`dmin` come FIRST in the struct (like `Q4_K`,
    /// unlike `Q6_K`'s scale-last layout), and its `scales[12]` are
    /// 6-bit-packed via the exact same `get_scale_min_k4` `Q4_K` already
    /// uses - reused here completely unchanged, not re-derived, since
    /// the packing scheme itself didn't change at all between the two
    /// formats (confirmed directly from GGML's own `dequantize_row_q5_K`
    /// source, which calls `get_scale_min_k4` the identical way
    /// `dequantize_row_q4_K` does). The only genuinely new piece is a
    /// 5th bit per value: each value is a 4-bit nibble from `qs[128]`
    /// (same split-half low/high convention as every nibble-packed
    /// format above) plus `+16` if a corresponding bit in `qh[32]` is
    /// set - an unsigned 0..31 range, no `-32`/`-8`-style recentering
    /// (unlike `Q4_0`/`Q6_K`) - dequantized affinely (`d*scale*value -
    /// dmin*min`), the same shape as `Q4_K`'s formula, just with a
    /// wider per-value range.
    ///
    /// Struct layout (176 bytes, `QK_K=256`): `d`(2) + `dmin`(2) +
    /// `scales[12]` + `qh[32]` (`QK_K/8` - ONE bit per value) +
    /// `qs[128]` (`QK_K/2`, same nibble packing as every format above).
    /// Cross-checked against `dequantize_row_q5_K`'s own pointer/index
    /// arithmetic before trusting it: `qs` advances 32 bytes per outer
    /// iteration over 4 iterations (128 bytes total, matching
    /// `qs[128]`), while **`qh` never advances at all** - all 4
    /// iterations index the SAME 32-byte `qh` array, extracting a
    /// DIFFERENT single bit position each time via `u1`/`u2` masks that
    /// shift left by 2 every iteration (`u1`: bits 0,2,4,6; `u2`: bits
    /// 1,3,5,7 - together covering all 8 bits of each `qh` byte across
    /// the 4 iterations, matching `qh[32]`'s `QK_K/8` sizing exactly:
    /// 32 bytes × 8 bits = 256 individual high-bits, one per value).
    /// This non-advancing-pointer-plus-shifting-mask scheme is a
    /// genuinely different design from `Q6_K`'s two-separate-advancing-
    /// arrays approach, not a variant of it - checked explicitly rather
    /// than assumed to generalize from the other K-quant formats above.
    ///
    /// Hand-traced with concrete byte values before writing any Rust:
    /// with `qh` byte `0xAA` (bits, LSB first: 0,1,0,1,0,1,0,1) and four
    /// distinct `qs` nibble-pair bytes (`0xAB`, `0xCD`, `0xEF`, `0x12`)
    /// for the four 32-byte chunks, the eight extracted values (low/high
    /// nibble of each chunk, with `+16` applied exactly when that
    /// sub-block's `u1`/`u2` bit is set) are `11, 26, 13, 28, 15, 30, 2,
    /// 17` - independently confirmed by re-deriving which of `0xAA`'s
    /// bits are 0 vs 1 at each shifted mask position.
    pub fn decode_q5_k(bytes: &[u8]) -> Vec<f32> {
        const BLOCK_SIZE: usize = 176; // 2(d)+2(dmin)+12(scales)+32(qh)+128(qs)
        const SCALES_LEN: usize = 12;
        const QH_LEN: usize = 32;
        const QS_LEN: usize = 128;

        let (blocks, _remainder) = bytes.as_chunks::<BLOCK_SIZE>();
        let mut values = Vec::with_capacity(blocks.len() * 256);
        for block in blocks {
            let d = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));
            let dmin = f16_to_f32(u16::from_le_bytes([block[2], block[3]]));
            let scales = &block[4..4 + SCALES_LEN];
            let qh = &block[4 + SCALES_LEN..4 + SCALES_LEN + QH_LEN];
            let qs = &block[4 + SCALES_LEN + QH_LEN..4 + SCALES_LEN + QH_LEN + QS_LEN];

            let mut is = 0;
            let mut u1: u8 = 1;
            let mut u2: u8 = 2;
            let mut q_offset = 0;
            for _ in 0..4 {
                let (sc1, m1) = get_scale_min_k4(is, scales);
                let (sc2, m2) = get_scale_min_k4(is + 1, scales);
                let d1 = d * sc1 as f32;
                let dm1 = dmin * m1 as f32;
                let d2 = d * sc2 as f32;
                let dm2 = dmin * m2 as f32;

                for l in 0..32 {
                    let byte = qs[q_offset + l];
                    let low = (byte & 0x0F) as u32 + if qh[l] & u1 != 0 { 16 } else { 0 };
                    values.push(d1 * low as f32 - dm1);
                }
                for l in 0..32 {
                    let byte = qs[q_offset + l];
                    let high = (byte >> 4) as u32 + if qh[l] & u2 != 0 { 16 } else { 0 };
                    values.push(d2 * high as f32 - dm2);
                }

                q_offset += 32;
                is += 2;
                // Final iteration's shifts overflow u8 (64<<2=256,
                // 128<<2=256) and wrap - harmless, matching GGML's own
                // C behavior, since neither value is read again after
                // the loop ends.
                u1 <<= 2;
                u2 <<= 2;
            }
        }
        values
    }

    /// Decodes a byte buffer as raw, unquantized half-precision (`F16`)
    /// values - unlike every quantized format above, there's no block
    /// structure or shared scale here at all: each value is simply its
    /// own 2 raw bytes, decoded via `f16_to_f32` (already built and
    /// verified for the quantized formats' block scales). Any trailing
    /// byte that doesn't form a complete pair is silently dropped, same
    /// convention as every other decoder here.
    pub fn decode_f16_le(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| f16_to_f32(u16::from_le_bytes(c)))
            .collect()
    }

    /// Decodes `bytes` according to a tensor's own parsed `gtype` -
    /// dispatches to the right format-specific decoder instead of
    /// silently assuming `F32`, closing a real gap: `GgufTensorInfo`
    /// already parses and stores every tensor's real gtype, but nothing
    /// before this ever branched on it to decide *how* to read the
    /// tensor's data - every existing caller just called `decode_f32_le`
    /// directly regardless of what gtype it had actually parsed. Every
    /// `GgufGtype` this kernel recognizes has a real decoder now.
    pub fn decode_tensor(bytes: &[u8], gtype: GgufGtype) -> Result<Vec<f32>, &'static str> {
        match gtype {
            GgufGtype::F32 => Ok(Self::decode_f32_le(bytes)),
            GgufGtype::F16 => Ok(Self::decode_f16_le(bytes)),
            GgufGtype::Q8_0 => Ok(Self::decode_q8_0(bytes)),
            GgufGtype::Q4_0 => Ok(Self::decode_q4_0(bytes)),
            GgufGtype::Q4_1 => Ok(Self::decode_q4_1(bytes)),
            GgufGtype::Q2_K => Ok(Self::decode_q2_k(bytes)),
            GgufGtype::Q3_K => Ok(Self::decode_q3_k(bytes)),
            GgufGtype::Q4_K => Ok(Self::decode_q4_k(bytes)),
            GgufGtype::Q5_K => Ok(Self::decode_q5_k(bytes)),
            GgufGtype::Q6_K => Ok(Self::decode_q6_k(bytes)),
            GgufGtype::Q8_K => Ok(Self::decode_q8_k(bytes)),
        }
    }

    /// Perform forward inference using loaded GGUF tensor weights
    pub fn execute_gguf_layer_pass(
        &self,
        weights: &[f32],
        inputs: &[f32],
        outputs: &mut [f32],
        in_dim: usize,
        out_dim: usize,
    ) {
        let dummy_bias = [0.0f32; 16];
        kprintln!(
            "[GGUF ENGINE] Executing native layer pass for arch '{}'...",
            self.architecture
        );
        TensorEngine::matmul_layer(weights, inputs, &dummy_bias, outputs, in_dim, out_dim);
    }
}

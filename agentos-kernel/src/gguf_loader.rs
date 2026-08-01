use crate::kprintln;
use crate::serial_println;
use crate::tensor_engine::TensorEngine;
use alloc::string::String;
use alloc::vec::Vec;

pub const GGUF_MAGIC: u32 = 0x46554747; // "GGUF" in Little Endian

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GgufGtype {
    F32 = 0,
    F16 = 1,
    Q4_0 = 2,
    Q4_1 = 3,
    Q8_0 = 8,
}

impl GgufGtype {
    fn from_u32(raw: u32) -> Result<Self, &'static str> {
        match raw {
            0 => Ok(GgufGtype::F32),
            1 => Ok(GgufGtype::F16),
            2 => Ok(GgufGtype::Q4_0),
            3 => Ok(GgufGtype::Q4_1),
            8 => Ok(GgufGtype::Q8_0),
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

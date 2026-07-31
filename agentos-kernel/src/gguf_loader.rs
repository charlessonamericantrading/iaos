use crate::kprintln;
use crate::serial_println;
use crate::tensor_engine::TensorEngine;

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

pub struct GgufTensorInfo {
    pub name: &'static str,
    pub dimensions: [usize; 2],
    pub gtype: GgufGtype,
    pub offset: usize,
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

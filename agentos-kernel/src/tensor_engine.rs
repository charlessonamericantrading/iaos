/// Native Tensor Engine for bare-metal matrix multiplication & SIMD vector operations
pub struct TensorEngine;

impl TensorEngine {
    /// Perform native dot product between weight vector W and activation vector X
    pub fn dot_product(w: &[f32], x: &[f32]) -> f32 {
        let len = w.len().min(x.len());
        let mut sum = 0.0f32;

        // Loop unrolling for hardware pipeline optimization
        let mut i = 0;
        while i + 4 <= len {
            sum += w[i] * x[i] + w[i + 1] * x[i + 1] + w[i + 2] * x[i + 2] + w[i + 3] * x[i + 3];
            i += 4;
        }

        while i < len {
            sum += w[i] * x[i];
            i += 1;
        }

        sum
    }

    /// Perform a forward pass layer multiplication Y = W * X + B
    pub fn matmul_layer(
        weights: &[f32],
        inputs: &[f32],
        bias: &[f32],
        outputs: &mut [f32],
        in_dim: usize,
        out_dim: usize,
    ) {
        for row in 0..out_dim {
            let row_start = row * in_dim;
            let row_weights = &weights[row_start..row_start + in_dim];
            let dot = Self::dot_product(row_weights, inputs);
            let b = if row < bias.len() { bias[row] } else { 0.0 };
            outputs[row] = Self::relu(dot + b);
        }
    }

    #[inline(always)]
    fn relu(val: f32) -> f32 {
        if val > 0.0 {
            val
        } else {
            0.0
        }
    }
}

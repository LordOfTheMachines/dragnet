// SPDX-License-Identifier: AGPL-3.0-only
//! Simetrik int8 nicemleme: `x ≈ q * scale`, `scale = max|x| / 127`.
//!
//! Vektörler L2-normalize olduğundan kosinüs benzerliği `scale_a * scale_b * dot_i32(qa, qb)`
//! ile hesaplanır; 500k×768'de tek çekirdekte ~150 ms (bkz. ARCHITECTURE §7.3).

/// f32 vektörü int8'e nicemler; `(q, scale)` döner. Sıfır vektör → scale 0.
pub fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 || !max.is_finite() {
        return (vec![0; v.len()], 0.0);
    }
    let scale = max / 127.0;
    let inv = 127.0 / max;
    let q = v
        .iter()
        .map(|x| (x * inv).round().clamp(-127.0, 127.0) as i8)
        .collect();
    (q, scale)
}

/// int8 vektörü geri açar (test/teşhis için).
pub fn dequantize(q: &[i8], scale: f32) -> Vec<f32> {
    q.iter().map(|&x| x as f32 * scale).collect()
}

/// İki int8 vektörün tamsayı iç çarpımı (derleyici otomatik vektörize eder).
#[inline]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = 0i32;
    // Chunk'lı toplama: LLVM için vektörizasyon-dostu.
    for (x, y) in a.iter().zip(b) {
        acc += *x as i32 * *y as i32;
    }
    acc
}

/// f32 kosinüs (test/karşılaştırma için).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    d / (na.sqrt() * nb.sqrt() + 1e-9)
}

/// Vektörü L2-normalize eder (yerinde).
pub fn l2_normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_cosine_close_to_f32() {
        let mut a: Vec<f32> = (0..256)
            .map(|i| ((i * 31 % 97) as f32 - 48.0) / 50.0)
            .collect();
        let mut b: Vec<f32> = (0..256)
            .map(|i| ((i * 17 % 89) as f32 - 40.0) / 45.0)
            .collect();
        l2_normalize(&mut a);
        l2_normalize(&mut b);
        let (qa, sa) = quantize(&a);
        let (qb, sb) = quantize(&b);
        let approx = sa * sb * dot_i8(&qa, &qb) as f32;
        let exact = cosine(&a, &b);
        assert!(
            (approx - exact).abs() < 0.01,
            "approx={approx} exact={exact}"
        );
        // Kendisiyle ~1.0
        let selfsim = sa * sa * dot_i8(&qa, &qa) as f32;
        assert!((selfsim - 1.0).abs() < 0.02, "{selfsim}");
    }

    #[test]
    fn zero_vector_is_safe() {
        let (q, s) = quantize(&[0.0; 8]);
        assert_eq!(s, 0.0);
        assert!(q.iter().all(|&x| x == 0));
        assert_eq!(dequantize(&q, s), vec![0.0; 8]);
    }
}

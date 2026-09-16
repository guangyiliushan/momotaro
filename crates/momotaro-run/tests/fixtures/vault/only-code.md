# Kernel Functions

A reference implementation of the Gaussian (RBF) kernel follows.

```rust
fn rbf(x: &[f64], y: &[f64], gamma: f64) -> f64 {
    let sq_dist: f64 = x.iter().zip(y.iter()).map(|(a, b)| (a - b).powi(2)).sum();
    (-gamma * sq_dist).exp()
}

fn polynomial(x: &[f64], y: &[f64], degree: u32, coef0: f64) -> f64 {
    let dot: f64 = x.iter().zip(y.iter()).map(|(a, b)| a * b).sum();
    (dot + coef0).powi(degree as i32)
}

fn sigmoid(x: &[f64], y: &[f64], alpha: f64, coef0: f64) -> f64 {
    let dot: f64 = x.iter().zip(y.iter()).map(|(a, b)| a * b).sum();
    (alpha * dot + coef0).tanh()
}

fn laplacian(x: &[f64], y: &[f64], gamma: f64) -> f64 {
    let abs_dist: f64 = x.iter().zip(y.iter()).map(|(a, b)| (a - b).abs()).sum();
    (-gamma * abs_dist).exp()
}

fn anova(x: &[f64], y: &[f64], gamma: f64, degree: u32) -> f64 {
    let mut total = 0.0;
    for (a, b) in x.iter().zip(y.iter()) {
        total += (-gamma * (a - b).abs()).exp().powi(degree as i32);
    }
    total
}
```

Each kernel maps inputs into an implicit feature space where inner products
become cheap to evaluate. The RBF kernel is translation invariant and its
feature space is infinite-dimensional.

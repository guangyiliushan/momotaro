# Principal Component Analysis

PCA finds orthogonal directions of maximum variance in a dataset.

## The procedure

1. Center the data matrix X.
2. Compute the covariance matrix C = X^T X / n.
3. Eigendecompose C; eigenvectors are principal axes.

## Explained variance

The ratio of each eigenvalue to the trace gives the fraction of variance
explained by the corresponding component.

| Component | Eigenvalue | Explained |
|-----------|------------|-----------|
| PC1       | 3.2        | 64%       |
| PC2       | 1.1        | 22%       |
| PC3       | 0.7        | 14%       |

## Connection to SVD

PCA is equivalent to the SVD of the centered data matrix; singular values
squared divided by n are the eigenvalues of the covariance matrix.

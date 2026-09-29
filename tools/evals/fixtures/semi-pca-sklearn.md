# sklearn PCA 调参备忘

`PCA(n_components=0.95)` 按累计 explained_variance_ratio_ 自动选维数；
`components_` 是主成分载荷，`transform` 之后得到降维坐标。sklearn 的默认
SVD 求解器打在中心化后的协方差矩阵上，特征先做标准化再进 Pipeline，避免
验证集泄漏。调参用网格搜索加交叉验证，绘图用 pairplot 看分离度。

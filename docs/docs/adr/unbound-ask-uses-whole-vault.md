---
sidebar_position: 7
sidebar_label: ADR 0007
---

# ADR 0007 · With no Project, Ask's P1 is the whole Vault

P1/P2 is a context-pipeline seam, not a retrieval filter. Projects ship in v0.5; Ask's DoD is v0.3. Refusing unbound Ask, or treating P1 as empty, makes v0.3 retrieval_empty. Until a Session binds a Project, P1 = P2 = the Vault and the split is a no-op.

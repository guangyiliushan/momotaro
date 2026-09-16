---
sidebar_position: 18
sidebar_label: ADR 0018
---

# ADR 0018 · 0.x Spark actions are pin and dismiss

Status is `proposed | pinned | dismissed`. Pin does not write the vault, does not change origin class, and does not by itself index the card. Promote-to-annotation is a later explicit command, not a pin side effect. Auto-writing an owner note is already refused.

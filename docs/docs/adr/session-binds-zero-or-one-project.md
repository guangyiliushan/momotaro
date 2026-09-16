---
sidebar_position: 3
sidebar_label: ADR 0003
---

# ADR 0003 · A Session binds zero or one Project; Runs copy that binding

Project is the desk (sources). Session is the conversation. `sessions.project_id` is optional; when set, it is the default P1 scope. The binding is immutable after create — switch desks by opening a new Session. Each Run copies `project_id` at accept. Making them independent would leave Project unused on the Ask path; nesting Sessions inside Projects would entangle two lifecycles the product already keeps separate (deleting a Project must not delete Sources, and must not be forced to delete conversations). With no Project, P1 is the whole Vault (Ask in v0.3 cannot wait for Projects in v0.5).

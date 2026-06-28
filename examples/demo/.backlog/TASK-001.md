---
id: TASK-001
type: impl
status: wip
rfcs: [RFC-003]
prs:
  - repo: org/parser
    pr: 0
    branch: feat/task-001
constraints:
  - text: "nao tocar em src/legacy/"
    enforce: hook
  - text: "manter API estavel"
    enforce: review
---

## Objetivo
Implementar enum aberto.

# jaum-logs

Orquestrador de backlog sobre o **Claude Code** (CLI `claude`). Gerencia um
backlog em markdown e dirige sessões do Claude Code de forma controlada, com
duas garantias centrais:

1. **Constraints não são esquecidas.** Diretrizes "não faça X" são reinjetadas a
   cada iteração da sessão (não só na abertura).
2. **Escopo tem borda.** Escopo extra vira `deferred` + um novo backlog, em vez
   de inflar a task atual.

## Princípios

- O diretório **`.backlog/` (markdown) é a única fonte de verdade**. GitHub/`gh`
  é downstream — número de PR e estado de merge são *lidos*, nunca duplicados.
- A ferramenta é uma **casca fina e tool-agnóstica**: não escreve código de
  feature, não escreve RFC/ADR, não faz merge. Ela orquestra.
- O **Claude Code é um executor plugável** atrás de um trait; trocar de
  ferramenta = trocar só o adapter.
- **Core reaproveitável** (`jaum-core`) separado dos **adapters** (git, gh,
  executor, ui).

## Estrutura

```
jaum-logs/
  Cargo.toml            # workspace
  crates/
    core/               # jaum-core: modelo de dados + store do .backlog/
      src/{model,store,error}.rs
      tests/store.rs
    cli/                # jaum: binário (TUI ratatui entra na fase 7)
      src/main.rs
```

## Garantias

A ferramenta classifica o que é garantido como **hard**, **detectivo** e
**apenas sinalizado** — e é explícita sobre cada nível.

### Mecânico (hard) — bloqueado de fato

- **Não-merge**: a ferramenta nunca executa `git merge`/`gh pr merge`. Merge é
  comando do usuário, fora da ferramenta.
- **Terminal, não API**: usa só o CLI `claude`, nunca a API da Anthropic.
- **PR-only**: o play abre PR (`gh pr create`), jamais mergeia.
- **Paralelo entre repos diferentes**: worktree por repo linkado.
- **Constraints `enforce: hook`**: viram PreToolUse hook (regex) que bloqueia
  preventivamente em toda chamada (caminho, comando, migration, merge).

### Detectivo (obrigatório, não opcional)

- **Constraints `enforce: review`** (semânticas: abstração nova, API estável,
  refactor): o hook não as pega, então o review é **obrigado** a checá-las item a
  item. `is_clean` falha se alguma reprovar.

### Apenas sinalizado

- **Overlap no mesmo repo**: tasks `wip` tocando o mesmo recurso são sinalizadas,
  não bloqueadas.
- **Disciplina de doc lifecycle** (RFC/ADR).
- **Projeto infinito**: contido via `deferred`, mas não impedido.

## Modelo de dados

Cada task é um markdown com frontmatter YAML em `.backlog/TASK-NNN.md`:

```markdown
---
id: TASK-012
type: impl            # impl | spike
status: wip           # backlog | ready | wip | review | merged
rfcs: [RFC-003, RFC-007]
adrs: [ADR-011]
prs:
  - repo: tono-lang/parser
    pr: 142             # 0 = ainda não criado (lido do gh)
    branch: feat/task-012
deferred:
  - "primitivo decimal fica pra TASK-024"
constraints:
  - text: "nao tocar em src/legacy/"
    enforce: hook       # mecânica  -> bloqueio PREVENTIVO via PreToolUse hook
  - text: "manter API estavel"
    enforce: review     # semântica -> checagem DETECTIVA obrigatória no review
---

## Objetivo
...

## Criterio de aceite
- [ ] ...
```

`type: spike` gera documento (RFC/ADR), **não** tem PR nem play. Relatórios de
review ficam em `.backlog/TASK-NNN.review.md`.

## Stack

`ratatui` + `crossterm` (TUI) · `portable-pty` + `tui-term` (sessão `claude`
embutida) · `tokio` (subprocessos) · `gray_matter` + `serde` + `serde_yaml_ng`
(frontmatter) · `anyhow` + `thiserror` (erros) · `git`/`gh` via `std::process`.

## Status de implementação

- [x] Fase 1 — `store` + modelo de dados + parse/escrita de frontmatter (testes)
- [x] Fase 2 — adapters `git` e `gh`
- [x] Fase 3 — `executor` trait + impl Claude Code (oneshot, depois PTY)
- [x] Fase 4 — `play` (prompt, guard flags, hook PreToolUse + reinjeção via UserPromptSubmit)
- [x] Fase 5 — `review` (contexto cheio, report, check semântico, is_clean)
- [x] Fase 6 — `conflict` + `finish`
- [ ] Fase 7 — TUI ratatui

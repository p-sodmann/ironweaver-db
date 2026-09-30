# ADR 0001: Record architecture decisions

Status: accepted
Date: 2026-09-30

## Context

Ironweaver DB is built in ordered steps by several contributors, including coding agents. Decisions about formats, dependencies and guarantees need to outlive the conversation or PR in which they were made.

## Decision

We record significant design decisions as short Architecture Decision Records in `documentation/adr/`, named `NNNN-short-title.md` with a four-digit, increasing number. An ADR is never rewritten after it is accepted; a later ADR supersedes it and both link to each other.

Template:

```markdown
# ADR NNNN: Title

Status: proposed | accepted | superseded by ADR NNNN
Date: YYYY-MM-DD

## Context
What forces are at play and why a decision is needed.

## Decision
What we decided, stated in full sentences.

## Consequences
What becomes easier or harder, and what follow-up work it creates.
```

## Consequences

- Reviewers can find the reason behind a format, dependency or guarantee without reading old PRs.
- Changing a recorded decision needs a new ADR, which makes such changes visible.

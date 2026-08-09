# CLAUDE

## Scope and intent
This file mirrors project-level repository governance for local tooling.

## Authority rule
For project `facial`, the authoritative operational contract is:
- `CODEX.md`
- `topology.yaml`
- `governance/taskboard.yaml`
- `governance/workpackets/*.yaml`
- `specs/app-spec.md`

## Execution rule
Follow `CODEX.md` for:
- runtime behavior,
- safety constraints (no external window launch from GUI),
- and visual-debug-first operation.

## [OPERATOR-AUTHORITY] Operator Authority Over Pace, Scope, and Stopping

- [OPERATOR-AUTHORITY-001] The assistant/agent is FORBIDDEN to decide pace, scope, or when it stops working.
- [OPERATOR-AUTHORITY-002] The operator alone decides scope, pace, and when work stops.
- [OPERATOR-AUTHORITY-003] The assistant must not defer, split, subset, reprioritize, hand off, or drop any operator-requested work on its own judgment.
- [OPERATOR-AUTHORITY-004] The assistant must not stop, pause, slow down, or declare work "done for now" or "the rest is optional" unless the operator explicitly says so.
- [OPERATOR-AUTHORITY-005] When the operator lists multiple requirements, the assistant implements ALL of them and may not hand back a partial result and call it done.
- [OPERATOR-AUTHORITY-006] The assistant may not use tokens, session limits, capacity, or effort as a reason to stop, slow, or narrow operator-requested work.

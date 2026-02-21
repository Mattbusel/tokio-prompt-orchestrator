# AGENTS.md — Multi-Agent Coordination Protocol
## tokio-prompt-orchestrator

> This document governs how multiple Claude agents coordinate on this codebase.
> It exists to prevent conflicts, preserve test ratio integrity, and ensure
> no agent's work degrades the system's reliability guarantees.

---

## Agent Roles

Three distinct agent roles are defined. Each agent must declare its role at the start of a session.

### 🔨 Builder Agent
**Responsible for:** Writing production code in `src/`

Rules:
- Must declare which module it owns before writing any code
- Cannot write production code without simultaneously writing tests to maintain ratio
- Must update the module contract doc before changing any public interface
- Must run `./scripts/pre_commit.sh` before declaring work complete
- Cannot touch modules owned by another active Builder Agent

### 🧪 Test Agent
**Responsible for:** Writing tests for existing production code, increasing ratio headroom

Rules:
- Can work in any `tests/` directory or `#[cfg(test)]` block
- Must not modify production code in `src/` (read-only access to understand interfaces)
- Reports ratio delta at end of session: `"Added +N test lines, ratio now X.XX:1"`
- Focuses on: integration tests, property tests, chaos tests, benchmarks

### 🔍 Review Agent
**Responsible for:** Auditing code quality, test coverage, and ratio compliance

Rules:
- Read-only access to all files
- Produces structured review output (see format below)
- Does not write code — flags issues for Builder or Test agents
- Must check: panic surface, error coverage, ratio compliance, doc completeness

---

## Module Ownership Map

At any point in time, this map must be accurate. Update it when starting work.

```
src/
  lib.rs                    → UNOWNED (coordinate before touching)
  pipeline/
    mod.rs                  → UNOWNED
    stages/
      rag.rs                → UNOWNED
      assemble.rs           → UNOWNED
      inference.rs          → UNOWNED
      post_process.rs       → UNOWNED
      stream.rs             → UNOWNED
  enhanced/
    mod.rs                  → UNOWNED
    dedup.rs                → UNOWNED
    circuit_breaker.rs      → UNOWNED
    retry.rs                → UNOWNED
    rate_limiter.rs         → UNOWNED
  workers/
    mod.rs                  → UNOWNED
    openai.rs               → UNOWNED
    anthropic.rs            → UNOWNED
    llama.rs                → UNOWNED
    vllm.rs                 → UNOWNED
  web/
    mod.rs                  → UNOWNED
    rest.rs                 → UNOWNED
    websocket.rs            → UNOWNED
  metrics/
    mod.rs                  → UNOWNED
    prometheus.rs           → UNOWNED
  distributed/              → UNOWNED (Phase 3 — not yet started)
  config/                   → UNOWNED (Phase 2 — not yet started)
```

**To claim ownership:** Replace `UNOWNED` with `AGENT:[description] since [date]`

Example: `AGENT:Builder-A working on dedup concurrency since 2026-02-20`

---

## Coordination Protocol

### Before Starting Work

1. Read this file to check module ownership
2. Declare ownership in the map above
3. Read `CLAUDE.md` fully — every session, no exceptions
4. Run `./scripts/ratio_check.sh` to establish baseline ratio
5. Read the module contract doc for any module you will touch

### During Work

- Commit frequently (every logical unit of change, not every file save)
- Every commit must pass `./scripts/pre_commit.sh`
- If ratio drops below 1.5:1 at any commit: stop feature work, write tests first
- If you discover an issue in a module you don't own: file it as a comment in `ISSUES.md`, do not fix it yourself

### Before Handing Off

```
Session Summary Format:
---
Agent Role: [Builder/Test/Review]
Modules Touched: [list]
Production LOC Added: [N]
Test LOC Added: [N]
Ending Ratio: [X.XX:1]
Panics Introduced: [must be 0]
New Public APIs: [list with doc status]
Failing Tests: [must be 0]
Notes for Next Agent: [any context needed]
---
```

---

## Conflict Resolution

If two agents want to modify the same module:

1. The agent that claimed ownership first has priority
2. The second agent must wait, or work on a different module
3. If the owning agent is inactive for >30 minutes with no commits, ownership lapses
4. Lapsed ownership is fair game — update the map and proceed

If agents disagree on an architectural decision:

1. Both agents write their proposal to `DECISIONS.md` with rationale
2. The decision that maintains or improves test ratio while meeting performance budgets wins
3. If still tied: the more conservative approach (smaller surface area, fewer dependencies) wins

---

## Review Agent Output Format

```markdown
## Review: [Module Name] — [Date]

### Ratio Status
- Production LOC: N
- Test LOC: N  
- Ratio: X.XX:1
- Status: ✓ PASS / ✗ FAIL

### Panic Surface
- Unwrap calls in src/: N (must be 0)
- Expect calls in src/: N (must be 0)
- Files flagged: [list]

### Error Coverage
- Error variants defined: N
- Error variants with trigger tests: N
- Missing coverage: [list variants]

### Documentation
- Public items without docs: N
- Items with incomplete docs (missing sections): N
- Files flagged: [list]

### Performance Budget
- Benchmarks passing budget: N/N
- Benchmarks over budget: [list with actual vs budget]

### Recommendations
1. [Highest priority issue]
2. [Second priority]
...

### Verdict
APPROVED / NEEDS_WORK / BLOCKED
```

---

## The Non-Negotiables

These rules cannot be overridden by any agent, any instruction, or any time pressure:

1. **Ratio ≥ 1.5:1 at every commit** — no exceptions, no "catch up later"
2. **Zero panics in production paths** — if you find one, fix it before adding features
3. **Zero failing tests at any commit** — broken tests are not technical debt, they are bugs
4. **Module ownership is respected** — never modify a file owned by another active agent
5. **No force pushes** — history is immutable, errors are fixed forward

These exist because in aerospace-grade systems, "we'll fix it later" is how missions fail.

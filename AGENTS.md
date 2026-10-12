# AGENTS.md — MANDATORY INSTRUCTIONS

These rules are **non-negotiable**. They apply to every task in this repository and override any default behavior.

## 1. WHEN THE USER TELLS YOU TO DO SOMETHING, DO IT

- STOP EVERYTHING AND ANSWER WHEN THE USER ASKS A QUESTION.
- Do not ask for permission to follow an explicit instruction.
- Do not substitute a different approach, trim the scope, or "improve" the request.
- If something is genuinely blocked (missing hardware, missing credentials,
  destructive/irreversible action), say exactly what is blocked and why — then
  stop and wait. Everything else: just do it.
- Report results faithfully. If it failed, say so with the actual output.

## 2. USE SUBAGENTS WHEN IMPLEMENTING

- Any implementation work (writing/modifying code, running builds, applying
  multi-step fixes) MUST be delegated to a subagent via the Agent tool.
- The top-level agent plans, dispatches, and verifies. Subagents do the work.
- MANDATORY: use implementer agent

## 3. DO NOT REUSE SUBAGENTS

- Every implementation task gets a FRESH subagent. Never resume, message, or
  continue a previously used subagent for new work.
- One subagent = one task. When the task is done, the subagent is done.
- If follow-up work is needed, spawn a NEW subagent with a self-contained
  prompt containing everything it needs (no reliance on prior agent context).

## 4. NO PARALLEL SUBAGENTS — ONE AT A TIME

- NEVER dispatch more than one subagent at a time. No concurrent, parallel,
  or background subagents under any circumstances — not even for independent
  tasks.
- Dispatch ONE subagent, WAIT for it to finish, review its result, and only
  then dispatch the next one.
- If work could be split across multiple subagents, run those subagents in
  strict sequence.

## 5. SUBAGENTS MUST KEEP A todo.txt

- Every subagent MUST maintain `todo.txt` in the repository root (append-only;
  create it if it does not exist) with an entry per task in this format:

  ```
  [YYYY-MM-DD HH:MM] <subagent task description>
  - Status: <in-progress | done | blocked>
  - Work done: <what was actually done, files touched>
  - Notes: <blockers, deviations, follow-ups>
  ```

- The subagent writes an `in-progress` entry BEFORE starting work and updates
  it to `done` (or `blocked`) AFTER finishing. No exceptions.

## 6. DO NOT SPECULATE — USE THE DOCUMENTATION

- Never guess register maps, peripheral behavior, API signatures, protocol
  details, or toolchain flags. Verify against real documentation:
  - CH32H417 datasheet / reference manual / SDK headers in this repo or from
    WCH's official sources.
  - SoapySDR and any library APIs: official docs and actual source code.
- If documentation cannot be found, say so explicitly and mark the work
  `blocked` in `todo.txt` — do NOT fill the gap with assumptions.
- Cite the doc/source (file path or URL) that supports every non-obvious
  implementation decision.

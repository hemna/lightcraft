# SDD ledger — plan: plan/specs/2026-10-08-immich-integration-design.md

Setup: rebased feat/immich-import (3b559ba) onto origin/main c435d14 -> 8950e07.
Conflicts: xtask/src/layers.rs (kept main's denoise/heif rows + immich L0), Cargo.lock (took main's, regenerated + amend).
`cargo test -p lightcraft-immich` -> 18 passed. Clean tree.
Ruling: plan is a design spec, not an SDD task file — task-start/task-done scripts not applicable; ledger maintained manually, spec §8 PR1 is the contract, AGENTS.md gates (cargo xtask ci) are the CI authority.

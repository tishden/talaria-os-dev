## What this changes and why

<!-- The mechanism, not the diff: what was wrong, what makes it right now. If it
     is a performance change, the number it moves and on which stand. -->

## How it is judged

<!-- CONTRIBUTING.md: behaviour visible from a workload comes with a gate. Name
     it: the mode, the assertion, or why no gate is possible here. -->

- [ ] `kernel/test.sh` — FAIL=0 (say which assertions are new)
- [ ] `cd kernel-unit && cargo test` — green; new pure logic came with tests
- [ ] `cargo clippy -Zjson-target-spec --release -- -D warnings` — clean in both kernel configurations
- [ ] `kernel/validate.sh` — run, or explained why it does not apply
- [ ] Docs updated where the change makes them wrong (numbers live in `README.md` only)
- [ ] The CLA is signed — one comment on this pull request, the line is in `CLA.md` (first-time contributors only)

## What was NOT done

<!-- Known gaps, deliberate omissions, things left for a separate change. A gap
     named here is a decision; a gap found later is a defect. -->

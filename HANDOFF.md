# Handoff: boot-fuzz (BOOT-8 embassy-boot-fuzz), 22 Oct 2026

## Done (planning step)

- Plan for the person: `~/git/tpm/hobby/P2625-chunky-ota/design/upstream-embassy-boot-fuzz.md`, with the cited zoo research in `upstream-embassy-boot-fuzz-zoo-{stm32-legacy,stm32-ecc,nrf-rp}.md` next to it.
- Embassy: blobless clone `~/git/embassy` on omarchy-m1 (there was none here), worktree `/tmp/wt/boot-fuzz/embassy` on local branch `upstream/boot-fuzz` from origin/main 97c4d7e8a. No commits there yet. Embassy's default branch is `main`.
- Tasks on P2625, release `^embassy-boot-fuzz`: #22oct-2053 (piece 1 core, top, this session), -2053b zoo, -2053c findings, -2053d async+signed, -2053e cargo-fuzz, -2054 campaign (gpd-omarchy), -2054c packaging, -2054b Roman asks maintainers. BOOT-8 claimed and bound to #22oct-2053.
- FEATURES.md: BOOT-8 planned.

## Next

- Wait for Roman's OK on the plan (session parked). Then piece 1 in the embassy worktree.

## Decisions and gotchas

- No new dependency in embassy-boot (dirbaio removed `rand` on 19 Oct 2026): own byte reader instead of proptest/arbitrary; libfuzzer-sys only in a separate `embassy-boot/fuzz/` crate.
- Integration test, public API only; nothing in `src/` changes.
- Existing embassy-boot `cargo test` takes 75 s debug here, mostly PR 7329's 2^16-subset tests.
- PR text, commit messages and replies are Roman's (embassy AI policy): bullet facts only.

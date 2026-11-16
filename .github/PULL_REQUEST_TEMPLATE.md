## Summary

<!-- What does this change and why? Link the issue: "Fixes #123". -->

## How was it tested?

<!-- Commands you ran, platforms (Windows build, iOS device/simulator), manual checks. -->

## Checklist

- [ ] Title follows [Conventional Commits](https://www.conventionalcommits.org/) (`feat(ovhost): ...`, `fix: ...`)
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `bash scripts/e2e_relay.sh` and `bash scripts/e2e_lan.sh` (changes to the protocol, transport, host or viewer)
- [ ] iOS builds and its tests pass (`ios/build_ios.sh --test`) if `ovffi`, `ovcore`, `ovtransport`, `ovapp` or `ios/` changed
- [ ] Docs updated (`docs/`, flag tables in `docs/USAGE.md`, `CHANGELOG.md` under *Unreleased*)
- [ ] No secrets, PINs, device IDs or personal data in code, logs, screenshots or tests

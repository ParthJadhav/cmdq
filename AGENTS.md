# Local build and installation

- After making any changes, build and install the current checkout before finishing so the user's local setup picks them up.
- Run `cargo install --path . --force --locked` from the repository root. This builds the release binary and replaces the locally installed `cmdq`; `cargo build` alone is not sufficient.
- Verify that `command -v cmdq` resolves to the installed binary and that `cmdq --version` succeeds.
- If building or installing fails, fix the failure or clearly report the blocker. Do not claim the local setup is updated until installation succeeds.
- Existing running sessions keep their loaded binary; start a new `cmdq` session to use the update.

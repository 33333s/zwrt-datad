# Repository Workflow

- Whenever the user asks to commit or publish repository changes, increment the
  patch component of `version.json` by `0.0.1` in the same change.
- Build the stripped static aarch64-musl binary after the change is merged.
- Publish a GitHub Release named `v<version>` and attach the compiled binary
  using the asset name declared in `version.json`.
- A requested commit is not complete until the matching GitHub Release and
  binary asset have been published successfully.

# MacroToolbox repository instructions

These instructions apply to the entire repository.

## Changelogs are required for every user-facing change

Releases are cut by `scripts/package-release.ps1` and `.github/workflows/release.yml`.
Both hard-fail if `changelog/v<version>.md` is missing: the packaging script throws
`Changelog not found...`, and the release workflow's verification step exits non-zero.

Whenever you make a user-facing code change, record it in the changelog for the next
unreleased version as part of the same change.

### Never modify a released version's changelog

A version is released if it has a `vX.Y.Z` Git tag or a `releases/vX.Y.Z/` directory. Its
changelog describes exactly what shipped in that binary. Never add, edit, or remove entries in
a released version's changelog; put the change in the next version instead.

### Determine the target version every time

1. Find the highest released version: the greatest `vX.Y.Z` represented by a Git tag
   (`git tag --sort=-v:refname`) or a `releases/vX.Y.Z/` directory.
2. Classify the change according to semantic versioning:
   - **Major** (`v<MAJOR+1>.0.0`): a breaking change. Existing saved configuration (`db.json`),
     scopes, profiles, or hotkeys stop working or require migration, or a feature or command is
     removed or renamed.
   - **Minor** (`v<MAJOR>.<MINOR+1>.0`): a new backward-compatible user-facing capability, such
     as a setting, button, overlay item, or command. Existing setups continue to work.
   - **Patch** (`v<MAJOR>.<MINOR>.<PATCH+1>`): a bug fix or small correction with no new feature.
   If a batch mixes change types, use the highest-ranked one: major, then minor, then patch.
3. Increment the selected part of the highest released version and reset every lower part to
   zero. For example, from `v1.0.6`, a fix targets `v1.0.7`, a feature targets `v1.1.0`, and a
   breaking change targets `v2.0.0`. The file is `changelog/v<target>.md`.
4. Confirm that the target has neither a tag nor a `releases/` directory. If it does, it is
   already released; repeat these rules using that release as the baseline.

`src-tauri/tauri.conf.json` is not a reliable released-version signal. The packaging script
updates it during a build, before the tag exists. Trust tags and `releases/` directories.

### Write the changelog entry

- Keep one next-version changelog. If an unreleased `changelog/v*.md` exists above the highest
  released version, append to it. If the new change requires a higher semantic-version bump,
  rename that file to the higher target and append there; never leave competing next-version
  changelogs.
- If no next-version changelog exists, create `changelog/v<target>.md`:

  ```markdown
  # MacroToolbox v<target>

  ## Changes

  - <one user-facing, past-tense bullet per change>
  ```

- If the file exists and is unreleased, append a bullet under `## Changes`.
- Describe the effect for the user, not the implementation.
- Skip pure internal churn—formatting, comments, renames, and editor or agent configuration—
  unless it changes application behavior.

# Code signing for Windows (free, through SignPath Foundation)

Without a signature, Windows shows "Windows protected your PC / Unknown publisher" the
first time someone runs `mouse-rs.exe`. The README tells users to click **More info →
Run anyway**. A signature replaces "Unknown publisher" with a real one.

## Why SignPath

SignPath Foundation signs open-source projects for free. mouse-rs qualifies once:

- it is under an OSI-approved licence: **Apache-2.0** since 1 Nov 2026 ✅
- its source is public on GitHub ✅ (from the 1.0.0 launch)
- its binaries are built by CI from that repository: `.github/workflows/build.yml` ✅
- it has no proprietary part ✅

Paid alternatives, for comparison (prices as of Nov 2026, check before buying):
- an OV certificate costs about $150–300 a year, plus a hardware token or cloud key;
- Certum's open-source certificate starts at about €69;
- Microsoft's Azure Artifact Signing (~$10/month) is open to individuals only in the US and Canada.

No certificate gives instant trust any more. SmartScreen may still warn on the first
downloads of a new version until enough people have run it.

## What the owner does (one time)

1. Turn on **two-factor authentication** on GitHub (Settings → Password and
   authentication), if it isn't already on. SignPath requires it for everyone on the project.
2. Apply at <https://signpath.org/apply> with:
   - project: mouse-rs, <https://github.com/prashantpal8980/mouse-rs>
   - licence: Apache-2.0
   - what is signed: `mouse-rs.exe`, built by GitHub Actions (`build.yml`)
3. Wait for approval (days to weeks). SignPath then creates the project. Its
   organisation ID and an API token come with it.
4. In the GitHub repo: **Settings → Secrets and variables → Actions**
   - secret `SIGNPATH_API_TOKEN` = the token
   - variable `SIGNPATH_ORGANIZATION_ID` = the organisation ID
5. Ask SignPath for a signing policy named `release-signing` for the project `mouse-rs`,
   or change those two names in `build.yml` to whatever they create.

## After that

Push a tag (`git tag v1.0.1 && git push origin v1.0.1`). The workflow builds, tests, and
sends `mouse-rs.exe` to SignPath, which returns it signed. Attach the signed exe to the
release in place of the locally built one, then run `scripts/sign-release.sh` so
`latest.json` names its size and SHA-256.

The certificate is issued to **SignPath Foundation**, so Windows shows them as the
publisher. That is how their free service works.

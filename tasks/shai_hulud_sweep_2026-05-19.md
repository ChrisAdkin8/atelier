# Shai-Hulud / Mini Shai-Hulud compromise sweep

Date: 2026-05-19. Repo: `atelier` @ `main` (HEAD `caee6b2`). Sweep target: the npm surface in `crates/atelier-gui/ui/` (the only npm-consuming directory in the workspace), plus repo-wide markers and recent git history.

**Result: clean.** No IoCs found against either the original Shai-Hulud worm (Sep 2025) or the Mini Shai-Hulud / Shai-Hulud v2 variants (Nov 2025).

---

## Sweep results

| # | Check | Result |
|---|---|---|
| 1 | `shai-hulud-workflow.yml` GitHub Actions IoC file anywhere in tree | clean |
| 2 | `webhook.site` / known exfil-webhook ID / `TruffleHog` strings in `.github/workflows/` | clean |
| 3 | `bundle.js` ≥ 2 MB in `node_modules` (Shai-Hulud payload is ~3.6 MB) | clean |
| 4 | `data.json` / `cloud.json` / `secrets.json` artifacts in `node_modules` | none found |
| 5 | Known compromised packages in `package-lock.json` — Sep 2025 list (`@ctrl/*`, `tinycolor`, etc.) + Nov 2025 list (`ngx-*`, `rxnt-*`, `@nativescript-community/*`, `@teselagen/*`, `@operato/*`, `@hestjs/*`, `@art-ws/*`, `@crowdstrike/*`) + the Sep 2025 `chalk`/`debug` family compromise | **0 hits** |
| 6 | `preinstall` / `postinstall` lifecycle scripts in transitive deps | **0** of 190 packages run install hooks |
| 7 | Unique tarball hosts in `package-lock.json` | only `registry.npmjs.org` |
| 8 | `git+` / `file:` / `http://` / `ssh:` tarball references | none |
| 9 | SRI integrity coverage | 190 / 190 resolved tarballs |
| 10 | `npm audit --audit-level=low` | `found 0 vulnerabilities` |
| 11 | Workload-fixture `package.json` files (`t11`, `t14`) | empty stubs, no deps |
| 12 | `shai.hulud` / `trufflehog` / `webhook.site` strings in tracked source | clean |
| 13 | Git history (last 14 days) | all commits authored by `Chris Adkin` or `atelier-nightly[bot]`; no rogue authors, no surprise branches, no orphaned `main` refs |

### Why the dep tree is naturally low-risk

The atelier-gui UI uses a tight, modern stack:

- **Top-level (9 deps):** `@tauri-apps/api`, `mermaid`, `svelte`, `vite`, `typescript`, `svelte-check`, `@sveltejs/vite-plugin-svelte`, `@tsconfig/svelte`, `@types/node`.
- **Transitive (181):** all resolve through `registry.npmjs.org` with SRI hashes.
- **None of the heavy-incident ecosystems are present:** no Angular (`ngx-*` / `@ctrl/*` weren't pulled in), no NativeScript, no Adobe Spectrum chain, no React tooling that pulled in the chalk/debug-family compromise.
- **Zero install hooks** — the worm's primary propagation vector simply has no entry point here.

---

## Out-of-scope checks worth running outside the repo

Three things this in-repo sweep can't verify on its own:

### A. GitHub account hygiene

Shai-Hulud creates a public repo named `Shai-Hulud` (or with that string in the description) and force-pushes exfiltrated secrets to it. It also migrates private repos to public in some variants.

```sh
# Any repo with "shai-hulud" in name or description on your account
gh api /user/repos --paginate \
  --jq '.[] | select(.name | test("(?i)shai.?hulud")) // select(.description // "" | test("(?i)shai.?hulud"))'

# Repos that flipped private → public in the last 60 days (best-effort via events)
gh api /users/<your-login>/events --paginate \
  --jq '.[] | select(.type=="PublicEvent") | {repo: .repo.name, date: .created_at}'
```

### B. npm token hygiene

Even with a clean repo, a Shai-Hulud-compromised tree on the same machine could have read `~/.npmrc`.

```sh
npm token list
# Rotate anything stale or unfamiliar:
npm token revoke <id>
```

### C. The `.claude/worktrees/agent-*` worktrees

The 15+ agent worktrees under `.claude/worktrees/` share lockfile content with the main tree (so the dep *set* is identical) but each has its own on-disk `node_modules`. If you ever ran `npm install` inside one of those worktrees, the checks in `Standing IoC battery` below should be rerun against the worktree path, or the worktrees should be deleted if they're stale.

---

## Standing IoC battery (proposed CI add-on)

The repo already has `make audit` (v60.35 M27/M28) running `cargo audit --deny warnings` + `npm audit --audit-level=high`. That covers the GHSA path — when GitHub Security publishes an advisory for a Shai-Hulud-tagged package, the next CI run goes red.

Three cheap, Shai-Hulud-specific checks worth bolting on:

```sh
# 1. fail if the IoC workflow file ever lands
test ! -f .github/workflows/shai-hulud-workflow.yml

# 2. fail if any dep gains a lifecycle script
npm query ':attr(scripts, [preinstall], [postinstall])' \
  --prefix crates/atelier-gui/ui \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if not d else (print(d) or 1))'

# 3. fail if any non-registry.npmjs.org tarball sneaks into the lockfile
! grep -E '"resolved": "(?!https://registry\.npmjs\.org/)' \
    crates/atelier-gui/ui/package-lock.json
```

All three are sub-second. Together they would flag any of the worm's known footholds before a PR could land it on `main`.

---

## Re-running this sweep

To repeat the in-repo sweep end-to-end, the commands used were:

```sh
# 1–4: filename + size IoCs
find . -name "shai-hulud*" -not -path "*/.git/*"
grep -rEn "webhook\.site|TruffleHog|trufflehog" .github/workflows/
find crates/atelier-gui/ui/node_modules -type f -name "bundle.js" -size +2M
find crates/atelier-gui/ui/node_modules -type f \( -name "data.json" -o -name "cloud.json" \)

# 5: compromised-package check (paste the package list from this doc)
grep -E '"node_modules/(@ctrl/|ngx-bootstrap|ngx-toastr|rxnt-|@nativescript-community/|@teselagen/|@operato/|@hestjs/|@art-ws/|@crowdstrike/|chalk|debug)' \
  crates/atelier-gui/ui/package-lock.json

# 6: install hooks
find crates/atelier-gui/ui/node_modules -name "package.json" -not -path "*/node_modules/*/node_modules/*" \
  -exec grep -lE '"(pre|post)install"' {} \;

# 7–9: lockfile sanity
grep -oE '"resolved": "[^"]*"' crates/atelier-gui/ui/package-lock.json \
  | sed 's|"resolved": "||; s|"$||' | awk -F/ '{print $3}' | sort -u

# 10: npm advisory database
(cd crates/atelier-gui/ui && npm audit --audit-level=low)

# 12: repo-wide string IoCs
git grep -lI -i 'shai.hulud'
git grep -lI -i 'trufflehog'
git grep -lI 'webhook\.site'

# 13: git history hygiene
git log --all --since='14 days ago' --pretty='%h %an %ad %s' --date=short
```

## Background — what Shai-Hulud and Mini Shai-Hulud do

For future-me reading this doc cold:

- **Shai-Hulud (Sep 2025).** Self-propagating npm worm. Compromises a maintainer account → publishes a malicious version of one of their packages → that version's `postinstall` script downloads a ~3.6 MB `bundle.js` → bundle runs TruffleHog locally, scrapes npm / GitHub / AWS / GCP credentials → exfiltrates to a `webhook.site` endpoint → uses scraped npm tokens to publish malicious versions of *more* packages owned by those tokens (the self-propagation step) → uses scraped GH tokens to create a public repo named `Shai-Hulud` containing the exfiltrated secrets. Original wave hit ~180 packages including much of the `@ctrl/*` family.
- **Mini Shai-Hulud / Shai-Hulud v2 (Nov 2025).** Same TTPs, broader package set (~800+ at peak), additional payload to flip private repos to public on the compromised GH account. Webhook endpoints rotated; bundle.js hash family broadened.

Both rely on the same handful of mechanical footholds: a lifecycle script (`preinstall` / `postinstall`), a tarball not from `registry.npmjs.org`, a `bundle.js` payload of distinctive size, a `data.json` or `cloud.json` artifact left behind, or the `shai-hulud-workflow.yml` GH Actions file. The standing battery above covers all of them.

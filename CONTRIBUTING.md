# Contributing to forkd

## Branch workflow

- Open feature, fix, documentation, and dependency pull requests against
  `dev`. GitHub uses `dev` as the repository's default branch.
- `main` is the stable release line. Pull requests to `main` are accepted
  only from this repository's `dev` branch.
- Both branches run the full CI suite. A pull request must pass the required
  `rust` check before it can merge.
- Do not force-push or delete `dev` or `main`.

Maintainers promote a tested `dev` revision to `main` with a pull request,
then fast-forward `dev` to the resulting merge commit so the branches do not
accumulate avoidable divergence.

## Commits

Use focused commits with an imperative subject. Sign off every commit for the
[Developer Certificate of Origin](https://developercertificate.org/) with:

```bash
git commit --signoff
```

By signing off, you certify that you have the right to submit the contribution
under the project's license.

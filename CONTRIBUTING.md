# Contributing to Nitely

Issues and pull requests are welcome. Nitely tracks its own build in Nitely (the NIT project), so a
pull request may be picked up, reviewed and landed by an agent working through that backlog;
a person still approves anything the project's rules send to a human.

## Before you open a pull request

- Open an issue first for anything larger than a fix, so the approach can be agreed before you
  write it.
- Run the gate, the same four commands CI runs on every pull request (`.nitely/playbook.toml`):

  ```
  cargo test --workspace
  cargo clippy --workspace -- -D warnings
  npm test
  npm run build
  ```

- For a change to a view, check it in the browser preview (`npm run dev`) in light and dark, wide
  and narrow, as [docs/visualization.md](docs/visualization.md) describes.
- Use made-up examples in code, tests, docs and commit messages (`C:\Code\my-app`, "Acme", project
  key `APP`), never names, paths or accounts from your own other projects.
- If you work with a coding agent, [AGENTS.md](AGENTS.md) is how agents work on this repo.

## Sign your commits (DCO)

Every commit in your pull request must carry a `Signed-off-by` line with your real name and email:

```
Signed-off-by: Sam Lee <sam@example.com>
```

`git commit -s` adds it. By signing off you state that you wrote the change, or otherwise have the
right to submit it under the project's license, as the Developer Certificate of Origin 1.1 says
(https://developercertificate.org). A pull request with an unsigned commit is not merged until
it is signed; `git rebase --signoff main` signs every commit on your branch.

## License

Nitely is licensed under the [Apache License 2.0](LICENSE). Contributions are accepted under the same
license (section 5 of the license), with no separate contributor agreement.

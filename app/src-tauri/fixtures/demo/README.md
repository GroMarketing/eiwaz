# Demo project for Eiwaz's tests

A tiny bakery site. `src/tests.rs` copies this folder to a temp directory and
checks Changes (save points, undo), Preview (file server, probe) and Guide
(project scan) against it. Tests assert on its contents, so edit with care:

- `index.html` contains "Fresh bread"
- `img/logo.png` is a real PNG (range requests are checked on it)
- `PLAN.md` contains a mermaid diagram
- `package.json` has a `dev` script and a `stripe` dependency (Guide's only signal)
- there is deliberately no CLAUDE.md

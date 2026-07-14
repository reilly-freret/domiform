# Contributing to Domiform

This is my first attempt at maintaining open-source software, and I'm excited to see where it goes!
Here's how you can help the project succeed:

## Slop policy

No slop! LLM-generated code is acceptable (mainly because I can't realistically commit to detecting and
policing it), but you gotta put some effort into your contributions/issues/comments. As the project
evolves, I'd like to develop a practical framework for slop detection, since
[traditional proxy measures are broken](https://blog.happyfellow.dev/simulacrum-of-knowledge-work/).

## Issues

If you want to change something about Domiform, open [a new issue](https://github.com/reilly-freret/domiform/issues/new).
Use GitHub's tag feature to mark your issue as `bug`, `feature`, `docs` or `question`. I don't have templates
set up yet, but in general, issues should look like this:

### Bug reports

- expected behavior
- actual behavior
- attempted remedies/workarounds
- runtime logs (ideally with the `-v` verbose flag)

### Feature requests

- what you want the program to do
- what you've tried instead
- suggested intervention point (struct, file, etc.)

### Questions

No template here. Note that a question issue is not enough to justify a PR; if discussion in
the comments of a question leads to a bug report or feature request, please open an issue
with the appropriate tag.

## Pull requests

> ⚠️ a PR without a corresponding issue won't be reviewed!

After opening a `bug`, `feature`, or `docs` issue, you're encouraged to open a PR containing the changes implied
by the issue. Take the following steps:

1. fork the repo
2. create in the new repo a branch with a descriptive name (e.g. `feat/apple_hap_adapter`)
3. open a PR against this repo
4. title the PR with a [conventional commit](https://www.conventionalcommits.org/en/v1.0.0/#summary) message
(e.g. `fix: prevent CASE disconnections in rs-matter bridge`)
5. include a reference to the corresponding issue in the PR description

## Style

Thankfully, Rust [decides this](https://doc.rust-lang.org/beta/style-guide/index.html) for us. I plan to add
opinionated formatters/linters for other file types if the need arises (you may notice that there's currently
no enforcement of, say, line-width in markdown files).

## Code of conduct

For now, just be civil and courteous. I'll do the same.

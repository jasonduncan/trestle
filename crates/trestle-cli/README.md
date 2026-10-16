# trestle-cli

The `trestle` command. `trestle new NAME` scaffolds a project. Inside a
project, every other command is passed to the project's own binary (`serve`,
`check`, `migrate`, `makemigrations`, `openapi`, ...).

```sh
cargo install trestle-cli
trestle new notes && cd notes
```

New projects depend on the published `trestle-framework` crate by default.
`--git URL --tag TAG` or `--trestle-path DIR` override that.

Part of [Trestle](https://github.com/jasonduncan/trestle). Licensed under
either of Apache License, Version 2.0 or MIT license, at your option.

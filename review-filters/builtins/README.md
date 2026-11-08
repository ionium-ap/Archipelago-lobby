# Builtin review filters

Each `.json` file here is a filter that every room's review tab has, on unless the room's
reviewers turn it off. Its file name, without `.json`, is its ID: rooms remember the builtins
they turned off by it, so don't rename a file once it's deployed.

A file holds one filter, in the format of the "Export" button of a room's filter. To make a
reviewer's filter builtin, export it and put the file here. `cargo test -p review-filters`
checks every file.

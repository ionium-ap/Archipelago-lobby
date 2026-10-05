# Archipelago world manager

This repository contains a library as well as tools to manage archipelago
worlds based on an index file.

## Index file

The index file is made of a base file and then `world` ones.

### Base file

The base file should be named `index.toml` and contains information to be
displayed about the index as well as the base version for archipelago.


```toml
archipelago_repo = "https://github.com/ArchipelagoMW/Archipelago.git"
archipelago_version = "0.5.0"
index_homepage = "https://github.com/ionium-ap/Archipelago-index"
index_dir = "index"
```

The `archipelago_repo` and `archipelago_version` will be used to download
supported worlds so it's important that they point to a proper git repository
and a proper git ref.

The `homepage` is just a way for users of the index to trace it back to
something.

It points to an `index_dir` directory containing different worlds files.

### World file

Every world should be contained in its own file, named `{world_name}.toml`. The
world name **must** match the apworld name.

For example, for pokemon crystal, you'd have an `index/pokemon_crystal.toml` file with the following content:
```toml
name = "Pokemon Crystal"
home = "https://discord.com/channels/731205301247803413/1057476528419647572"

[versions]
"2.0.0" = { "url" = "https://github.com/AliceMousie/Archipelago/releases/download/2.0.0/pokemon_crystal.apworld" }
"2.1.0" = { "url" = "https://github.com/AliceMousie/Archipelago/releases/download/2.1.0/pokemon_crystal.apworld" }
```

- `name`: The visible name for the APWorld, this could be anything but should probably be the title of the game
- `home`: A URL to where people can find information about the apworld. This can be a github repo, a discord thread link...

Note that the versions must be semver compliant.

#### Templating URLs

Because URLs are usually the same and the version is the only change, you can
have a `default_url` for a world containing `{{version}}`.
The toml above for pokemon crytsal would thus become:

```toml
name = "Pokemon Crystal"
home = "https://discord.com/channels/731205301247803413/1057476528419647572"
default_url = "https://github.com/AliceMousie/Archipelago/releases/download/{{version}}/pokemon_crystal.apworld"
[versions]
"2.0.0" = {}
"2.1.0" = {}
```

#### Default Versions

You can specify a default version for a world as follows:

```toml
name = "Pokemon Crystal"
home = "https://discord.com/channels/731205301247803413/1057476528419647572"
default_url = "https://github.com/AliceMousie/Archipelago/releases/download/{{version}}/pokemon_crystal.apworld"
default_version = "2.0.0"
[versions]
"2.0.0" = {}
"2.1.0" = {}
```

This instructs any consumer of this world to treat that version as default, if
it is not provided then the latest version will be used.

Additionally, `default_version` can also be:
 - `"latest"`: Uses the latest version.
 - `"latest_supported"`: Uses the latest supported version. Only valid for supported worlds.
 - `"disabled"`: Disables the world by default.

## The `apwm` tool

Built with the `cli` feature. Every subcommand takes the directory holding `index.toml` as `-i`.

| Subcommand | What it does |
|---|---|
| `update -i <index>` | Downloads what `index.lock` doesn't know yet and rewrites the lock. Covers the releases of every base. |
| `download -i <index> -d <dir>` | Downloads every release of every base, or one with `-p <apworld>:<version>`, or the added ones of a `changes.json` with `--from-changes`. |
| `changes -i <index> -f <git remote> [-r <ref>] -o <dir>` | Compares the index with the one at that remote and writes `changes.json` and the added apworlds. |
| `install -i <index> -a <apworlds dir> -d <dir> [--base <version>]` | Copies the latest release of each world for a base, the legacy one by default. |
| `lint -i <index>` | Reports what would break the lobbies reading this index. Exits with 1 on any error. |

### `changes.json`

For each world that changed, `added_versions` and `removed_versions` list the versions that
became available, or stopped being available, on at least one base. `added_on` and
`removed_from` say on which bases, per version. `checksums` holds the hash of each added
version, or `"supported"` for a core world.

`bases` lists the bases of the index, and `added_bases` the ones it didn't declare before.
Declaring a base changes no world by itself: the previous index is compared as if it had always
had that base, so only real differences show up, such as a world entering core on it.

### `lint`

An index that doesn't parse is reported as such. On one that does, the errors are:

- a `[versions]` entry with more than one key, or with a key other than `url` or `local`;
- a release in `[versions]` that `[releases]` says can't run on the legacy base;
- a world with nothing in `[versions]` that is neither `supported` nor `disabled` at the top
  level;
- a release whose `min_ap_version` is above its `max_ap_version`;
- a default version that a base doesn't have, or `latest_supported` on a world that isn't
  supported on a base;
- a key that isn't known, in `index.toml`, in a world, in a `[releases]` entry or in a `[base]`
  table. Unknown keys are ignored when parsing, so a misspelled constraint would otherwise mean
  "every base".

And the warnings: a `[versions]` entry that isn't a table, a release that the legacy base could
run but that is only in `[releases]`, a `[releases]` entry that changes nothing, a `[base]` table
that matches no declared base, and a `[bases]` table that leaves out the legacy base.

It can't tell whether a release really runs on the bases it claims, or whether a world marked
`supported` is in fact a core world of the legacy base. Those need the release to be loaded on
each base.

## Several Archipelago versions

An index can describe more than one Archipelago version at once. Each one is called a base, and
every base gets its own view of the index: its own list of worlds, and for each world its own
list of releases. `IndexSet` parses the index and holds one `Index` per base.

Everything in this section is ignored by lobbies from before this was supported. They keep
reading `archipelago_version` and `[versions]` and nothing else, so those two describe the
oldest base, the legacy base, and have to stay correct for it.

The index's CI holds that line. Its `legacy-index` jobs run `legacy-index-guard`, a small program
in the `Archipelago-index-ci` repository built against this crate as it was at commit `029bde7`,
the last one before multi-base support. It reads the index the way those lobbies do and fails if
they would not get a working view of the legacy base. That commit is pinned on purpose and must
never follow this crate.

### Declaring bases

```toml
archipelago_repo = "https://github.com/ArchipelagoMW/Archipelago.git"
archipelago_version = "0.6.7"
index_homepage = "https://github.com/ionium-ap/Archipelago-index"
index_dir = "index"

[bases."0.6.7"]
[bases."0.6.8"]
```

`archipelago_version` is the legacy base. It is always a base, listed or not, and an index
without a `[bases]` table has only that one.

### Releases that depend on the base

A release in `[versions]` is available on every base. To restrict one, or to add one that the
legacy base can't run, use `[releases]`:

```toml
name = "Pokemon Crystal"
home = "https://discord.com/channels/731205301247803413/1365127145709502575"
default_url = "https://github.com/gerbiljames/Archipelago/releases/download/{{version}}/pokemon_crystal.apworld"

[versions]
"5.4.6" = {}
"6.0.0" = {}

[releases]
"5.4.6" = { max_ap_version = "0.6.7" }
"7.0.0" = { min_ap_version = "0.6.8" }
"7.0.1" = { min_ap_version = "0.6.8", url = "https://github.com/gerbiljames/Archipelago/releases/download/7.0.1-hotfix/pokemon_crystal.apworld" }
```

The constraints, and the releases in `[releases]`, are made up for the example.

- `min_ap_version` and `max_ap_version` are both inclusive and both optional.
- A release can be in both tables. `[versions]` keeps it visible to older lobbies, `[releases]`
  says which bases can run it. Here 5.4.6 is offered on 0.6.7 only.
- A release that's only in `[releases]` takes `url` or `local` there, or uses `default_url`.
- A release that needs a base newer than the legacy one must not be in `[versions]`: older
  lobbies would pick it as the latest.
- A `[versions]` entry must keep a single key. Older lobbies silently drop its `url` if it has a
  second one, which is why the constraints live in their own table.

### Worlds that depend on the base

The top-level `supported`, `disabled` and `default_version` of a world describe the legacy base.
A `[base."<requirement>"]` table overrides any of the three for the bases matching its semver
requirement:

```toml
# A custom world that became a core world in 0.6.8
name = "Gauntlet Legends"
default_url = "https://example.com/gl-{{version}}/gl.apworld"

[versions]
"2.1.7" = {}

[releases]
"2.1.7" = { max_ap_version = "0.6.7" }

[base.">=0.6.8"]
supported = true
default_version = "latest_supported"
```

At most one `[base]` table of a world may match a given base.

A world isn't part of a base's view when it's disabled there or when none of its releases can run
there. Older lobbies only know the first of those two: a world they must not offer has to be
`disabled = true` at the top level, and enabled again with `[base."<requirement>"]` for the bases
that have it.

Archipelago lobby
=================

This project provides a lobby to collect yaml files from players to be able to
host archipelagoes easily.

# Running this project

```
cp docker-compose.yml.example docker-compose.yml
docker compose build
./start.sh
```

The first start will require you to create a Discord application for OAuth2 (see [Discord OAuth](#discord-oauth) below); `start.sh` will prompt you and write `Rocket.toml` for you.
The first start will also download all apworlds in the index, which might take a while.

For anything beyond a local dev instance, see [Configuring a real deployment](#configuring-a-real-deployment) before bringing services up.

## Stylesheets

The lobby's stylesheets are the Sass files in `lobby/static/sass/`. They are compiled to CSS when the lobby is built, by its `build.rs`, and the CSS is served from the binary under `/static/css/`. There is no compiled CSS in the repository and nothing to install or run by hand: edit a `.sass` file and build. A file whose name starts with `_` is a partial and isn't compiled on its own.

# Configuring a real deployment

The example compose file ships with `changeme` placeholders for every secret. Before deploying:

1. `cp docker-compose.yml.example docker-compose.yml`
2. If running community/webhost mode: `cp taskcluster/docker/ap-worker/webhost-config.yaml.example taskcluster/docker/ap-worker/webhost-config.yaml`
3. Replace every `changeme` and adjust the deployment-specific fields listed below.
4. Set up `Rocket.toml` (interactive via `./start.sh`, or write it by hand — see [Discord OAuth](#discord-oauth)).

Both `docker-compose.yml` and `taskcluster/docker/ap-worker/webhost-config.yaml` are gitignored once copied, so future `git pull`s won't clobber your edits.

## Required secrets

Generate strong random values for each row. `openssl rand -hex 32` or `openssl rand -base64 32` are both fine. Use a fresh value per row unless **Notes** says to share.

| Variable | Where (in `docker-compose.yml`) | Notes |
|---|---|---|
| `POSTGRES_PASSWORD` | `postgres` service env | Must also be reflected in the lobby's `DATABASE_URL`. |
| `DATABASE_URL` | `lobby` service env | Form: `postgres://postgres:<POSTGRES_PASSWORD>@postgres:5432/aplobby`. Keep the password in sync with `POSTGRES_PASSWORD`. |
| `ROCKET_SECRET_KEY` | `lobby` service env | Signs encrypted session cookies. Must be exactly 44 base64 chars (32 raw bytes), 88 base64 chars (64 raw bytes), or 64 hex chars (32 raw bytes). Generate with `openssl rand -base64 32`. Other lengths fail at startup with `InvalidLength`. |
| `ADMIN_TOKEN` | `lobby` service env | Auth for admin endpoints (`X-Api-Key` header / Basic Auth). |
| `LOBBY_API_KEY` | `generator` service env | **Must equal `ADMIN_TOKEN`** — the generator worker authenticates back to the lobby API with this. |
| `YAML_VALIDATION_QUEUE_TOKEN` | `lobby` and `yaml-checker` services | Same value in both places (queue auth between lobby and worker). |
| `GENERATION_QUEUE_TOKEN` | `lobby` and `generator` services | Same value in both places. |
| `OPTIONS_GEN_QUEUE_TOKEN` | `lobby` and `option-generator` services | Same value in both places. |
| `SECRET_KEY` (in `webhost-config.yaml`) | community/webhost only | Skip if not running community mode. |

## Other deployment-specific config

| Variable | Purpose |
|---|---|
| `VALKEY_URL` | Connection string for valkey/redis. |
| `APWORLDS_INDEX_REPO_URL` | Your fork of the apworld index repo. |
| `APWORLDS_INDEX_REPO_BRANCH` | Branch to track on the index repo. |
| `GENERATION_OUTPUT_DIR` | Path inside the lobby container where generated worlds are written. |

## Optional

| Variable | Effect |
|---|---|
| `SENTRY_DSN` | Enables Sentry error reporting. |
| `OTLP_ENDPOINT` | Enables OpenTelemetry / OTLP tracing. |
| `RUST_LOG` | Log filter, e.g. `info,ap_lobby=debug`. The compose example sets `debug`. |
| `SKIP_APWORLDS_UPDATE` | If set, skips fetching the apworld index on startup (useful for offline dev). |
| `PRELOAD_OPTIONS_DEFS` | If set, eagerly preloads option schemas into Valkey at startup, skipping the ones already there. |
| `AP_BASES` | The Archipelago versions to offer, for example `0.6.8,0.6.7`. Unset means the index's own version only. See [Choosing which bases to offer](#choosing-which-bases-to-offer). |
| `AP_DEFAULT_BASE` | The Archipelago version of a new room. Unset means the newest of `AP_BASES`. |
| `ADMIN_ROOMS_ONLY` | Restricts room creation to admins. Non-admins lose the "Create new room" links and the `/create-room` handlers reject them; existing rooms and everything else about them are unaffected. Accepts `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`, case-insensitive. Unset or empty means off; **any other value aborts startup**. |

## Discord OAuth

Configured in `Rocket.toml` (gitignored). On first run, `./start.sh` will prompt for credentials and write the file. To do it by hand:

```toml
[default.oauth.discord]
provider = "Discord"
client_id = "<your discord app's client id>"
client_secret = "<your discord app's client secret>"
redirect_uri = "https://<your-deployment-host>/auth/oauth"
admins = [<your discord user id>, ...]
banned_users = []   # optional
```

The `redirect_uri` must exactly match a redirect URI registered in your Discord developer application.

## Running more than one lobby process

Several lobby processes can serve the same deployment.

| State | Where it lives | With several processes |
|---|---|---|
| Rooms, YAMLs, validation results, generations | Postgres | Shared. |
| Job queues, sessions | Valkey | Shared. A worker's result is handled by whichever process the worker is connected to. |
| Option definitions for the options page | Valkey, for 24 hours | Shared. To drop them sooner, for example after fixing the options generator, delete the `options_def:*` keys. The ones of the index's own Archipelago version are `options_def:<apworld>:<version>`, as they always were; another base's are `options_def:base:<base>:<apworld>:<version>`. |
| Downloaded apworlds | `APWORLDS_PATH` | Shared if the directory is. |
| The index | A checkout of the index repository per process (`APWORLDS_INDEX_DIR`), read into its memory | Each process has its own, and they keep each other on the same commit, see below. Two processes must not share one `APWORLDS_INDEX_DIR`. |

The process that serves `/worlds/refresh` loads the newest commit of the index, records it in Valkey under `index:announced`, and publishes it. Every other process loads that same commit when it hears of it, normally well within a second, and checks the key every 30 seconds in case it didn't hear. The open rooms are looked at once, by the process that served the request.

A process that starts reads the newest commit, as it always did. If that is newer than the announced one, it announces it and the running processes follow, so restarting one process moves all of them. To see which commit a process is on, look for `Loaded the index at commit` in its log.

If Valkey is unreachable, a refresh still updates the process that served it and then answers with an error, since the others were not told. They catch up within 30 seconds of Valkey coming back if the refresh is run again.

# Archipelago versions

The workers (`yaml-checker`, `generator`, `option-generator`) run Archipelago itself, and a worker image is built for exactly one Archipelago version, called its base. Every request a worker makes to the lobby's queues names the base it runs, and the lobby only hands a worker the jobs of that base.

A room is on one base, kept in `rooms.ap_version`. Everything about the room follows from it: the worlds and releases it can use are the ones the index gives that base, its YAMLs are validated by a worker of that base, and so is its generation. A job carries its base as `ap_version`, and a worker refuses a job that names another base than its own.

The options pages (`/options...`) and the game API (`/api/games`, `/api/games/<apworld>/options`) are about one base as well: the default one, or the one named with `?base=<version>`. The worlds, their releases and their options are that base's. Option definitions are generated by a worker of that base and cached for it alone. The "Create YAML" links in a room's world list lead to the room's base.

Each queue keeps its waiting jobs in one list per base. The index's own version uses the keys the queue always had (`wq:<queue>:queue`), so jobs that are waiting or running across an upgrade or a rollback are not lost; any other base uses `wq:<queue>:partition:<base>:queue`. A worker that names no base, as the ones built before this did, is treated as running the index's version.

## Choosing which bases to offer

Two things decide which bases a lobby works with, and a base needs both:

- **The index describes it.** Its `index.toml` names one version in `archipelago_version` and can declare more in `[bases]`; see `apwm/README.md`.
- **The lobby offers it**, which is yours to set, because it only makes sense for a base you run workers for.

| Variable | Effect |
|---|---|
| `AP_BASES` | The bases to offer, separated by commas, for example `0.6.8,0.6.7`. Unset or empty offers the index's `archipelago_version` and nothing else, which is what a lobby did before it had this setting. A base listed here that the index doesn't describe yet is not offered until it does, with a warning at startup; it needs no restart when the index catches up. |
| `AP_DEFAULT_BASE` | The base of a new room, and of the options pages and the game API when no base is named. Unset or empty means the newest base on offer. It has to be one of `AP_BASES`. |

"Newest" is the highest version as semver orders them, whatever order `AP_BASES` lists them in. A pre-release is older than its release: `0.7.0-rc1` comes before `0.7.0`.

A value that isn't a version, or an `AP_DEFAULT_BASE` that isn't among the offered ones, stops the lobby at startup. If the index describes none of `AP_BASES`, the lobby starts, logs an error, and uses the index's own version wherever it needs a default. The log line `Loaded the index at commit ..., offering Archipelago 0.6.8 (default), 0.6.7` says what a process ended up with.

Each base on offer needs its own `yaml-checker`, `generator` and `option-generator`. A base that is offered without them looks fine and does nothing: its YAMLs stay unvalidated, its generations never start, and its options pages fail after waiting 30 seconds. In the compose example the three services at the top are for 0.6.7 and the three in the `ap-0.6.8` profile are for 0.6.8:

```
docker compose --profile ap-0.6.8 up -d
```

Not offering a base doesn't remove anything. A room that is on it keeps its pages and its YAMLs; with no worker for it, nothing new gets validated or generated there. `PRELOAD_OPTIONS_DEFS` preloads every base on offer.

## What a host sees

With one base on offer, nothing to choose appears anywhere. With more:

- **Making a room.** The form has an "Archipelago version" selector. It starts on the default base, or on the base of the template the room is made from if that one is still offered. Choosing another loads the form again, because the worlds of its Apworlds tab are those of one base.
- **A room's page** carries a chip in the upper right corner of its details, reading `AP 0.6.8`. It is green when the room is on the default base, gold when it is on an older one, and white when it is on a newer one. Its tooltip says the same in words.
- **Moving a room to another base** is done from its edit page, with the same selector and a "Change version" button. It is refused once the room has a generation. A room with no YAML moves at once. A room with YAMLs first shows what the move does, and moves when its owner confirms:
  - a warning for each world the target base doesn't have, whose YAMLs become unsupported, and for each world the room pins to a release the target base doesn't have, with the release that would be used instead;
  - a list of the worlds that simply get another release;
  - after the move, every YAML of the room is validated again by the new base's workers. The room's choice of worlds and releases is kept as it is and read against the new base.
- **Templates** can name a base, or leave it to the default when a room is made.
- **The worlds page and the options pages** have the selector too, and `/worlds` takes `?base=` like the options pages.
- **The API** gives a room's base as `ap_version`, in `/api/room/<id>` and in `/api/rooms`.

## Upgrading from a lobby without bases

The upgrade puts every existing room on 0.6.7, because that was the only version a lobby could be on when rooms got a base. A lobby whose index is on an older Archipelago has to move to 0.6.7 first, the usual way, with the version of the lobby from before this.

With `AP_BASES` unset, nothing else changes: the same workers get the same jobs, under the same keys in Valkey.

## Building a worker for a base

Each base has a pin file, `taskcluster/docker/ap-worker/bases/<base>.env`, holding the commits of Archipelago, the fuzzer and the linter that its image is built from. Archipelago comes from the [ionium fork](https://github.com/ionium-ap/Archipelago), which keeps one patched line per upstream release; [its own notes](https://github.com/ionium-ap/Archipelago/blob/ionium-0.6.8/docs/ionium%20fork.md) list what each line carries and why.

The Dockerfile takes the base as a build argument and defaults to `0.6.7`:

```
docker build --build-arg SRC=. --build-arg DOCKER_SRC=taskcluster/docker/ap-worker \
    --build-arg AP_BASE=0.6.8 -f taskcluster/docker/ap-worker/Dockerfile .
```

The build fails if the pinned Archipelago commit reports a different version than the file is named after.

- To move a base to a newer commit of the fork, change `BASE_COMMIT` in its pin file and note the fork tag beside it.
- To add a base, add its pin file, add the base to the `image-ap-worker` matrix in `.gitlab-ci.yml`, and move `NEWEST_BASE` there if it is the newest. The index's CI keeps its own list of bases and needs the new one too, see below.

CI pushes these `ap-worker` tags:

| Tag | Pushed from | Which build |
|---|---|---|
| `sha-<short sha>-<base>` | every branch | each base |
| `<base>` | `main` | each base |
| `<base>-dev` | `ionium-dev` | each base |
| `latest` | `main` | the newest base |
| `dev` | `ionium-dev` | the newest base |

`latest` and `dev` change Archipelago version whenever a base is added, so nothing that needs a given version should pull them. Unlike the other images, `ap-worker` has no `sha-<short sha>` tag without a base.

## Who builds on these images

- **The lobby's own deployments** pin `ap-lobby:sha-<short sha>` and, for each worker, `ap-worker:sha-<short sha>-<base>` of the same commit.
- **The index's CI** (the `Archipelago-index-ci` repository) builds one checker image per base, `ap-checker:<base>` on `ap-worker:<base>`, and fails its build if the two disagree on the Archipelago version. The jobs that run apworld code (`check`, `unit-tests`, `network-audit`, `fuzz`) run once per base, each on the versions that `apwm changes` reports as added on that base. The fuzzer and the linter in a checker image are the ones pinned in the base's pin file here.

The index's CI can't read its bases from the index, so it lists them itself: in the matrix of each of those jobs and its checker image build, and in `AP_BASES` in `index-validate.yml`. Its `diff` job warns when the index declares a base that has no lane. A base added here and to the index is not tested there until it is added to those lists.

## How the bases differ

A YAML can be valid on one base and not on another, and not only because the worlds changed.

| | 0.6.7 | 0.6.8 |
|---|---|---|
| A YAML's `requires:` block | Not checked. | Enforced. The YAML is rejected if `requires: version` is newer than 0.6.8, or if `requires: game: <Game>` asks for a newer world than the one the room uses. |
| A YAML's `quantity:` key | Ignored. | Rejected unless it is 1, because a room counts one player per YAML. |
| Core worlds | Includes Donkey Kong Country 3. | Donkey Kong Country 3 is gone; Gauntlet Legends is new. |
| Fork patches | Generation and hosting. | Generation only. |

The world version that `requires: game` is compared against comes from the apworld's own `archipelago.json`, not from the index. A core world with no manifest counts as 0.0.0, which is also what Archipelago's own templates assume for it.

Templates written by Archipelago always carry `requires: version` set to the version that wrote them, so a YAML from a newer Archipelago than the room's base is rejected on 0.6.8 and later.

# Running apdiff-viewer standalone

`apdiff-viewer` renders side-by-side diffs of `.apworld` zip contents for PR reviewers. It ships as a separate service with its own postgres and a host directory for the apworld blob store, so it deploys independently of the lobby. Source under [apdiff-viewer/](apdiff-viewer/).

Bring it up from a fresh clone:

```
cargo build --release --bin apdiff-viewer
cp target/release/apdiff-viewer taskcluster/docker/apdiff-viewer/build-result
cd apdiff-viewer
cp docker-compose.yml.example docker-compose.yml
# edit changeme placeholders; set APDIFF_PUBLIC_BASE_URL to the externally
# reachable URL — it must match what your PR-validation CI side will POST to
docker compose up -d
```

The diesel migration applies on first start. Smoke-test the POST path:

```
mkdir -p /tmp/sub/{apworlds,annotations}
cp some.apworld /tmp/sub/apworlds/some-0.1.0.apworld
echo '{"pr_number":1,"commit_sha":"deadbeef","added":[{"world_name":"some","version":"0.1.0"}]}' > /tmp/sub/manifest.json
echo '{"worlds":{"some":{"world_name":"Some","added_versions":["0.1.0"],"removed_versions":[],"checksums":{}}}}' > /tmp/sub/changes.json
tar -C /tmp/sub -cf /tmp/sub.tar .
curl -fsS -X POST -H "X-Api-Key: changeme" --data-binary @/tmp/sub.tar http://localhost:8001/api/submissions
```

The response is `{ "id": "...", "url": "..." }`. Open the URL in a browser to see the rendered diff.

### Bootstrap import

A separate single-artifact endpoint exists for one-shot seeding of the dedup table — used by an index-walker job that uploads the latest version of every apworld so future PR submissions have historical from-versions to point at. Same `APDIFF_API_KEY` auth as the submission endpoint; no manifest, no `submissions` rows are created — only `apworld_artifacts` is populated.

```
curl -fsS -X POST \
  -H "X-Api-Key: $APDIFF_API_KEY" \
  --data-binary @some-0.1.0.apworld \
  "http://localhost:8001/api/import?world=some&version=0.1.0"
```

Response: `{ "world": "some", "version": "0.1.0", "sha256": "...", "size_bytes": N, "stored": true }`. `stored: false` means the `(world, version, sha256)` tuple was already in the dedup table (re-upload is idempotent).

## Required secrets (apdiff-viewer)

| Variable | Where | Notes |
|---|---|---|
| `POSTGRES_PASSWORD` (apdiff stack) | `postgres` service env | Mirror in this stack's `DATABASE_URL`. Independent of the lobby's postgres. |
| `DATABASE_URL` | `apdiff-viewer` service env | `postgres://postgres:<password>@postgres:5432/apdiff` |
| `ROCKET_SECRET_KEY` | `apdiff-viewer` service env | Rocket release-mode startup check; same encoding rules as the lobby (44 base64 / 88 base64 / 64 hex chars). The service itself doesn't use session cookies, but the check fires anyway. |
| `APDIFF_API_KEY` | `apdiff-viewer` service env | Auth for `POST /api/submissions`. Share with the CI side that POSTs tarballs. |
| `FUZZ_API_KEY` | `apdiff-viewer` service env | Inherited from the legacy fuzz-results endpoints. Required at startup even if those routes go unused. |

## Other deployment-specific config (apdiff-viewer)

| Variable | Purpose |
|---|---|
| `APDIFF_STORAGE_ROOT` | Path inside the container for the blob store. Bind-mount it from a host dir. |
| `APDIFF_PUBLIC_BASE_URL` | The externally-reachable URL prefix. Used to build the `url` field of the POST response so CI can post it into PR comments. |
| `ROCKET_ADDRESS` | Set to `0.0.0.0` to expose the service from the container. |
| `TASKCLUSTER_ROOT_URL` (plus credentials) | Optional. Set only if you also want the legacy `GET /<task_id>` routes to serve from a Taskcluster instance. Leave unset for submission-only deployments — those routes will return an error at request time. |

Both `apdiff-viewer/docker-compose.yml` and the bind-mount dirs (`pgdata/`, `blobs/`) are gitignored once created, so future `git pull`s won't clobber your edits or data.

# Caveats

When working on the `ap-worker`, if you change the python dependencies, you
have to rerun `docker compose build` and restart everything.

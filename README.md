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

Several lobby processes can serve the same deployment, with one caveat about the index.

| State | Where it lives | With several processes |
|---|---|---|
| Rooms, YAMLs, validation results, generations | Postgres | Shared. |
| Job queues, sessions | Valkey | Shared. A worker's result is handled by whichever process the worker is connected to. |
| Option definitions for the options page | Valkey, for 24 hours | Shared. To drop them sooner, for example after fixing the options generator, delete the `options_def:*` keys. |
| Downloaded apworlds | `APWORLDS_PATH` | Shared if the directory is. |
| The parsed index | The memory of each process | **Not shared.** `/worlds/refresh` updates only the process that serves the request. Restart the others afterwards, or they keep validating against the old index. |

# Archipelago versions

The workers (`yaml-checker`, `generator`, `option-generator`) run Archipelago itself, and a worker image is built for exactly one Archipelago version, called its base. Every request a worker makes to the lobby's queues names the base it runs, and the lobby only hands a worker the jobs of that base.

A room is on one base, kept in `rooms.ap_version`. Everything about the room follows from it: the worlds and releases it can use are the ones the index gives that base, its YAMLs are validated by a worker of that base, and so is its generation. A job carries its base as `ap_version`, and a worker refuses a job that names another base than its own.

For now every room is on the `archipelago_version` of the index: rooms that existed before the column did were put on 0.6.7, new rooms get the index's version, and nothing in the interface changes a room's base yet. So a worker built for any other base connects, waits, and is never given a job. The options pages and the game API use the index's version as well. A worker that names no base, as the ones built before this did, is treated as running the index's version.

Each queue keeps its waiting jobs in one list per base. The index's version uses the keys the queue always had (`wq:<queue>:queue`), so jobs that are waiting or running across an upgrade or a rollback are not lost; any other base uses `wq:<queue>:partition:<base>:queue`.

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

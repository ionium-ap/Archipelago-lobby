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

| Variable | Effect when set |
|---|---|
| `SENTRY_DSN` | Enables Sentry error reporting. |
| `OTLP_ENDPOINT` | Enables OpenTelemetry / OTLP tracing. |
| `RUST_LOG` | Log filter, e.g. `info,ap_lobby=debug`. The compose example sets `debug`. |
| `SKIP_APWORLDS_UPDATE` | If set, skips fetching the apworld index on startup (useful for offline dev). |
| `PRELOAD_OPTIONS_DEFS` | If set, eagerly preloads option schemas into Redis at startup. |

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

## Required secrets (apdiff-viewer)

| Variable | Where | Notes |
|---|---|---|
| `POSTGRES_PASSWORD` (apdiff stack) | `postgres` service env | Mirror in this stack's `DATABASE_URL`. Independent of the lobby's postgres. |
| `DATABASE_URL` | `apdiff-viewer` service env | `postgres://postgres:<password>@postgres:5432/apdiff` |
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

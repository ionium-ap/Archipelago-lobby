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
| `ROCKET_SECRET_KEY` | `lobby` service env | Signs encrypted session cookies. Must be ≥ 32 bytes. |
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

# Caveats

When working on the `ap-worker`, if you change the python dependencies, you
have to rerun `docker compose build` and restart everything.

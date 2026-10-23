#!/usr/bin/env bash
# alc-cam-sd-ingest が外から素で届かないこと・本番の DB とカメラへの binding が本番にだけ在ることを wrangler.toml で検査する
# (ippoan/alc-lineworks-worker の同名の script の写し、元は ippoan/alc-vein-worker)。
#
# この Worker は呼び手を認証せず (auth-worker が付け直した X-Tenant-ID / X-User-Role / X-User-Developer を信頼する)、
# カメラの台帳 (宛先・資格情報) を読み書きし、VPC Network でカメラの網に届くので、到達経路は auth-worker からの
# Service Binding だけでなければならない:
#   - トップレベル (本番): workers_dev = false と preview_urls = false が明示されている
#   - どこにも (トップレベル・env 配下とも) route / routes が無い (custom_domain も routes の中に書く)
#   - どの env にも workers_dev = true / preview_urls = true が無い (トップレベルの true を env が継承するのも不可)
#   - どの vars にも ALLOW_INSECURE_DB / DATABASE_URL が無い (DATABASE_URL はローカルの .dev.vars にだけ置く)
#   - vpc_services はどこにも無い (カメラへは VPC Network、DB へは Hyperdrive)
#   - vpc_networks はトップレベル (本番) にだけ置く (ippoan/alc-cam#2)。env 配下には置かない
#     (宛先を絞らない binding なので、本番以外の版から拠点の網へ届かせない)
#   - hyperdrive はトップレベル (本番) にだけ置く (ippoan/rust-alc-api#723)。env 配下には置かない
#   - どの階層にも durable_objects と containers が無い (この worker は Durable Object も Container も持たない)
# 違えば exit 1。CI で毎回走らせる。陰性対照は scripts/check-exposure-test.sh。
#
#   bash scripts/check-exposure.sh [wrangler.toml]
set -euo pipefail

TOML="${1:-$(dirname "$0")/../wrangler.toml}"

python3 - "$TOML" <<'PY'
import sys
import tomllib

path = sys.argv[1]
with open(path, "rb") as f:
    cfg = tomllib.load(f)

errors = []

def err(msg):
    errors.append(msg)
    print(f"::error file={path}::{msg}")

# 本番 (トップレベル): 明示的に false
for key in ("workers_dev", "preview_urls"):
    if cfg.get(key) is not False:
        err(f"トップレベルに {key} = false がない (本番が外から直接届く)")

envs = cfg.get("env", {})
scopes = [("トップレベル", cfg)] + [(f"env.{n}", e) for n, e in envs.items()]

for scope, table in scopes:
    for key in ("route", "routes"):
        if key in table:
            err(f"{scope} に {key} がある (外から直接届く)")

# どの env も公開しない (workers_dev / preview_urls は env へ継承される)
for name, e in envs.items():
    for key in ("workers_dev", "preview_urls"):
        if e.get(key, cfg.get(key)) is True:
            err(f"env.{name} の {key} が true (どの env も workers.dev / preview URL を開けない)")

# DB の接続の経路を変える値は vars に置かない (ローカルの .dev.vars / --var にだけ)
for scope, table in scopes:
    for key in ("ALLOW_INSECURE_DB", "DATABASE_URL"):
        if key in table.get("vars", {}):
            err(f"{scope} の vars に {key} がある (ローカル専用)")

# VPC Service は使わない (カメラへは VPC Network、DB へは Hyperdrive)
for scope, table in scopes:
    if table.get("vpc_services"):
        err(f"{scope} に vpc_services がある (この worker は VPC Service を使わない)")

# 宛先を絞らない VPC Network と、本番の DB へ届く Hyperdrive は本番 (トップレベル) にだけ置く
for name, e in envs.items():
    for key in ("vpc_networks", "hyperdrive"):
        if e.get(key):
            err(f"env.{name} に {key} がある (本番の網・DB へ届く binding。トップレベルにだけ置く)")

# この worker は Durable Object も Container も持たない (どの階層にも置かない)
for scope, table in scopes:
    for key in ("durable_objects", "containers"):
        if key in table:
            err(f"{scope} に {key} がある (この worker は Durable Object も Container も持たない)")

if errors:
    sys.exit(1)
print(f"OK: {path} は本番が workers_dev / preview_urls = false・route 無しで、VPC Network と Hyperdrive は本番にだけ在る")
PY

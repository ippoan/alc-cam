#!/usr/bin/env bash
# check-exposure.sh の陰性対照 (ippoan/alc-lineworks-worker の同名の script の写し)。wrangler.toml を 1 か所ずつ崩したコピーで
# exit 1 になること、元のままなら exit 0 になることを確かめる。CI で check-exposure.sh の直後に走る。
# 行を挿す先は [build] の直前 (= トップレベル) と [env.local.vars] の直前 (= [env.local]) だけ。
set -euo pipefail
cd "$(dirname "$0")/.."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

expect() {
  local want="$1" label="$2" toml="$3"
  if bash scripts/check-exposure.sh "$toml" >"$tmp/out" 2>&1; then got=0; else got=1; fi
  if [ "$got" = "$want" ]; then
    echo "ok   ${label} (exit ${got})"
  else
    echo "FAIL ${label}: exit ${got}, want ${want}"; sed 's/^/     /' "$tmp/out"; fail=1
  fi
}

mutate() { # $1 = label, $2 = python の置換式 (s を書き換える)
  python3 - wrangler.toml "$tmp/w.toml" "$2" <<'PY'
import re
import sys
s = open(sys.argv[1]).read()
before = s
exec(sys.argv[3])
assert s != before, "mutation did not change wrangler.toml"
open(sys.argv[2], "w").write(s)
PY
  expect 1 "$1" "$tmp/w.toml"
}

expect 0 "wrangler.toml そのまま" wrangler.toml
mutate "local で workers_dev = true" \
  's = s.replace("name = \"alc-cam-sd-ingest-local\"\nworkers_dev = false", "name = \"alc-cam-sd-ingest-local\"\nworkers_dev = true", 1)'
mutate "local で preview_urls = true" \
  's = s.replace("preview_urls = false\n\n[env.local.vars]", "preview_urls = true\n\n[env.local.vars]", 1)'
mutate "別の env で workers_dev = true" \
  's += "\n[env.dev]\nname = \"cam-dev\"\nworkers_dev = true\n"'
mutate "別の env がトップレベルの true を継承" \
  's = re.sub(r"^workers_dev = false$", "workers_dev = true", s, count=1, flags=re.M) + "\n[env.other]\nname = \"x\"\n"'
mutate "トップレベルの workers_dev を true に" \
  's = re.sub(r"^workers_dev = false$", "workers_dev = true", s, count=1, flags=re.M)'
mutate "トップレベルの preview_urls を消す" \
  's = re.sub(r"^preview_urls = false\n", "", s, count=1, flags=re.M)'
mutate "本番に routes を足す" \
  's = s.replace("[build]", "routes = [{ pattern = \"cam.example.com\", custom_domain = true }]\n\n[build]", 1)'
mutate "local に route を足す" \
  's = s.replace("[env.local.vars]", "route = \"cam.example.com/*\"\n\n[env.local.vars]", 1)'
mutate "本番の vars に ALLOW_INSECURE_DB" \
  's = s.replace("MAX_CLIP_SECS = \"7200\"", "ALLOW_INSECURE_DB = \"1\"\nMAX_CLIP_SECS = \"7200\"", 1)'
mutate "local の vars に DATABASE_URL" \
  's = s.replace("[env.local.vars]\n", "[env.local.vars]\nDATABASE_URL = \"postgresql://x\"\n", 1)'
mutate "本番に vpc_networks (宛先を絞らない binding) を足す" \
  's = s.replace("[build]", "[[vpc_networks]]\nbinding = \"CAM_NET\"\nnetwork_id = \"cf1:network\"\nremote = true\n\n[build]", 1)'
mutate "local に vpc_networks を足す" \
  's = s.replace("[env.local.vars]", "[[env.local.vpc_networks]]\nbinding = \"CAM_NET\"\nnetwork_id = \"cf1:network\"\nremote = true\n\n[env.local.vars]", 1)'
mutate "local に vpc_services (拠点のカメラへ届く binding) を足す" \
  's = s.replace("[env.local.vars]", "[[env.local.vpc_services]]\nbinding = \"CAM_X\"\nservice_id = \"x\"\nremote = true\n\n[env.local.vars]", 1)'
mutate "local に hyperdrive (本番の DB へ届く binding) を足す" \
  's = s.replace("[env.local.vars]", "[[env.local.hyperdrive]]\nbinding = \"CAM_HYPERDRIVE\"\nid = \"x\"\n\n[env.local.vars]", 1)'
mutate "local に durable_objects を足す" \
  's = s.replace("[env.local.vars]", "[[env.local.durable_objects.bindings]]\nname = \"X\"\nclass_name = \"X\"\n\n[env.local.vars]", 1)'

exit "$fail"

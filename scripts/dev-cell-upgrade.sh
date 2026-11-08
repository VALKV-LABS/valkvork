#!/usr/bin/env bash
# Put this checkout's Valkvork binaries on a running dev cell, without
# building a golden image. For trying a fix during verification; releases
# still go through a tagged release and a new image.
#
#   scripts/dev-cell-upgrade.sh --cell <cell_id> --bucket <s3-bucket> [--region R] [--arch arm64|amd64] [--version X.Y.Z]
#   scripts/dev-cell-upgrade.sh --cell aws-us-west-1-valkvork --bucket my-dev-artifacts --region us-west-1 --version 0.3.0
#
# --version is what the binaries report in --version and the agents report to
# the controller (`valkvork node list --json`). It is stamped only for this
# build, like a release does; the Cargo.toml files are put back afterwards.
# Without it they report the committed crate version (0.1.0).
#
# What it does:
#   1. `make dist` for the cell's architecture (static musl binaries, in Docker).
#   2. Uploads them as one tarball to s3://<bucket>/valkvork-dev/ and presigns a
#      download link valid for 15 minutes. The nodes need no S3 permissions.
#   3. Finds the cell's instances by their Cell and Orchestrator=valkvork tags,
#      controller node first, and runs an SSM command on each: download, check
#      the SHA-256, swap the binaries in atomically, restart the units the
#      node runs. Running workloads keep running: the agent re-adopts them.
#   4. Prints each node's result and `valkvork-agent --version`.
#
# The next `valkv-valkvork-bootstrap` run (every boot) re-renders configuration
# but keeps these binaries; rebuilding or replacing an instance from the image
# brings back the released ones.
#
# Needs: aws CLI v2 with credentials for the cell's account, Docker (buildx),
# and an S3 bucket in any region you can write to.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CELL=""
BUCKET=""
REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-}}"
ARCH="arm64"
VERSION=""

usage() { sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
die() { echo "dev-cell-upgrade: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --cell) CELL="$2"; shift 2 ;;
    --bucket) BUCKET="$2"; shift 2 ;;
    --region) REGION="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --version) VERSION="${2#v}"; shift 2 ;;
    -h | --help) usage ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done
[ -n "$CELL" ] || die "--cell is required (the environment's cell_id)"
[ -n "$BUCKET" ] || die "--bucket is required"
[ -n "$REGION" ] || die "--region is required (or set AWS_REGION)"
case "$ARCH" in arm64 | amd64) ;; *) die "--arch must be arm64 or amd64" ;; esac
[ -z "$VERSION" ] || [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version must be X.Y.Z, not $VERSION"
command -v aws >/dev/null || die "aws CLI not found"

work="$(mktemp -d)"
# Put back the crate versions if --version stamped them, even on failure.
restore_versions() {
  [ -d "$work/manifests" ] || return 0
  (cd "$work/manifests" && find . -type f) | while read -r f; do
    cp "$work/manifests/$f" "$ROOT/$f"
  done
}
trap 'restore_versions; rm -r "$work"' EXIT

# ── 1. Build ─────────────────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
  mkdir -p "$work/manifests"
  (cd "$ROOT" && for f in Cargo.lock crates/*/Cargo.toml; do
    mkdir -p "$work/manifests/$(dirname "$f")"
    cp "$f" "$work/manifests/$f"
  done)
  "$ROOT/scripts/set-version.sh" "$VERSION" >/dev/null
fi
echo "==> Building linux/$ARCH binaries"
make -C "$ROOT" dist PLATFORM="linux/$ARCH" >/dev/null
restore_versions
for bin in valkvork-controller valkvork-agent valkvork; do
  [ -x "$ROOT/dist/$bin" ] || die "dist/$bin missing after make dist"
done
version="${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/valkvork-agent/Cargo.toml" | head -1)}"
rev="$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo -dirty)"
tarball="$work/valkvork-$rev-linux-$ARCH.tar.gz"
tar -C "$ROOT/dist" -czf "$tarball" valkvork-controller valkvork-agent valkvork
sha="$(shasum -a 256 "$tarball" | cut -d' ' -f1)"
echo "    $rev ($version), sha256 $sha"

# ── 2. Stage ─────────────────────────────────────────────────────────────────
key="valkvork-dev/$(basename "$tarball")"
echo "==> Uploading to s3://$BUCKET/$key"
aws s3 cp --only-show-errors "$tarball" "s3://$BUCKET/$key"
url="$(aws s3 presign "s3://$BUCKET/$key" --expires-in 900)"

# ── 3. Install on each node ─────────────────────────────────────────────────
# ValkvorkRoles is "controller+agent" or "agent"; the controller node sorts first.
nodes=()
while read -r id; do
  [ -n "$id" ] && nodes+=("$id")
done < <(aws ec2 describe-instances --region "$REGION" \
  --filters "Name=tag:Cell,Values=$CELL" "Name=tag:Orchestrator,Values=valkvork" \
  "Name=instance-state-name,Values=running" \
  --query 'Reservations[].Instances[].[Tags[?Key==`ValkvorkRoles`]|[0].Value, InstanceId]' \
  --output text | sort -r | awk '{print $2}')
[ "${#nodes[@]}" -gt 0 ] || die "no running instances tagged Cell=$CELL, Orchestrator=valkvork in $REGION"
echo "==> ${#nodes[@]} node(s): ${nodes[*]}"

# Runs on the node as root, under bash (AWS-RunShellScript itself uses sh).
# The units are restarted only if they are enabled on that node: the bootstrap
# enables the roles the node runs.
remote="$(cat <<REMOTE
cat > /root/valkvork-dev-upgrade.sh <<'VALKVORK_DEV'
set -euo pipefail
tmp=\$(mktemp -d); trap 'rm -r "\$tmp"' EXIT
curl -fsSL --retry 3 -o "\$tmp/v.tgz" '$url'
echo "$sha  \$tmp/v.tgz" | sha256sum -c --status || { echo "checksum mismatch"; exit 1; }
tar -C "\$tmp" -xzf "\$tmp/v.tgz"
for b in valkvork-controller valkvork-agent valkvork; do
  install -m 0755 "\$tmp/\$b" "/usr/local/bin/\$b.new"
  mv -f "/usr/local/bin/\$b.new" "/usr/local/bin/\$b"
done
for unit in valkvork-controller valkvork-agent; do
  if systemctl is-enabled --quiet "\$unit.service"; then
    systemctl restart "\$unit.service"
    sleep 3
    systemctl is-active --quiet "\$unit.service" || { journalctl -u "\$unit" -n 20 --no-pager; exit 1; }
    echo "restarted \$unit"
  fi
done
/usr/local/bin/valkvork-agent --version
VALKVORK_DEV
bash /root/valkvork-dev-upgrade.sh
REMOTE
)"
params="$(python3 -c 'import json,sys; print(json.dumps({"commands": sys.stdin.read().splitlines()}))' <<<"$remote")"

failed=0
for id in "${nodes[@]}"; do
  echo "==> $id"
  cmd="$(aws ssm send-command --region "$REGION" --instance-ids "$id" \
    --document-name AWS-RunShellScript --comment "valkvork dev upgrade $rev" \
    --parameters "$params" --query Command.CommandId --output text)"
  # Wait for this node before the next: the controller comes back before agents restart.
  aws ssm wait command-executed --region "$REGION" --command-id "$cmd" --instance-id "$id" || true
  status="$(aws ssm get-command-invocation --region "$REGION" --command-id "$cmd" --instance-id "$id" \
    --query Status --output text)"
  aws ssm get-command-invocation --region "$REGION" --command-id "$cmd" --instance-id "$id" \
    --query '[StandardOutputContent, StandardErrorContent]' --output text | sed 's/^/    /'
  echo "    $status"
  [ "$status" = "Success" ] || failed=1
done

aws s3 rm --only-show-errors "s3://$BUCKET/$key"
[ "$failed" -eq 0 ] || die "one or more nodes failed; see above"
echo "==> Done: $rev on ${#nodes[@]} node(s). Check with: valkvork node list"

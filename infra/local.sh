#!/bin/sh
# Starts DynamoDB Local in memory on port 8000 and creates the service tables.
# `infra/local.sh stop` stops it. Needs Java and the AWS CLI.
set -eu

url=https://d1ni2b6xgvw0s0.cloudfront.net/v2.x/dynamodb_local_latest.tar.gz
dir=${XDG_CACHE_HOME:-$HOME/.cache}/loremaster/dynamodb-local
pidfile=$dir/pid
endpoint=http://localhost:8000

# DynamoDB Local accepts any credentials; these keep the CLI off real AWS profiles.
export AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=us-east-2
unset AWS_PROFILE

running() { [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; }

if [ "${1:-}" = stop ]; then
  if running; then kill "$(cat "$pidfile")"; fi
  rm -f "$pidfile"
  exit 0
fi

if [ ! -f "$dir/DynamoDBLocal.jar" ]; then
  mkdir -p "$dir"
  curl -fsSL "$url" -o "$dir/download.tar.gz"
  expected=$(curl -fsSL "$url.sha256" | cut -d' ' -f1)
  actual=$(shasum -a 256 "$dir/download.tar.gz" | cut -d' ' -f1)
  if [ "$expected" != "$actual" ]; then
    echo "DynamoDB Local checksum mismatch" >&2
    rm -f "$dir/download.tar.gz"
    exit 1
  fi
  tar -xzf "$dir/download.tar.gz" -C "$dir"
  rm "$dir/download.tar.gz"
fi

if ! running; then
  (cd "$dir" && exec java -Djava.library.path=DynamoDBLocal_lib -jar DynamoDBLocal.jar \
    -inMemory -sharedDb -port 8000 >"$dir/log" 2>&1) &
  echo $! >"$pidfile"
  until aws dynamodb list-tables --endpoint-url "$endpoint" >/dev/null 2>&1; do
    running || { cat "$dir/log" >&2; exit 1; }
    sleep 0.2
  done
fi

create() {
  table=$1
  shift
  aws dynamodb describe-table --endpoint-url "$endpoint" --table-name "$table" >/dev/null 2>&1 \
    || aws dynamodb create-table --endpoint-url "$endpoint" --table-name "$table" \
      --billing-mode PAY_PER_REQUEST "$@" >/dev/null
}
create users \
  --attribute-definitions AttributeName=user_id,AttributeType=S \
  --key-schema AttributeName=user_id,KeyType=HASH
create devices \
  --attribute-definitions AttributeName=token_hash,AttributeType=S AttributeName=user_id,AttributeType=S \
  --key-schema AttributeName=token_hash,KeyType=HASH \
  --global-secondary-indexes 'IndexName=by_user,KeySchema=[{AttributeName=user_id,KeyType=HASH}],Projection={ProjectionType=ALL}'

echo "DynamoDB Local on $endpoint with tables users and devices"

# Run the service against it, from crates/loremaster-service:
#   cargo lambda watch --env-var USERS_TABLE=users,DEVICES_TABLE=devices,\
#   GITHUB_API_URL=https://api.github.com,DYNAMODB_ENDPOINT=http://localhost:8000,\
#   AWS_ACCESS_KEY_ID=local,AWS_SECRET_ACCESS_KEY=local,AWS_REGION=us-east-2
# Then call http://localhost:9000/lambda-url/loremaster-service/v1/devices

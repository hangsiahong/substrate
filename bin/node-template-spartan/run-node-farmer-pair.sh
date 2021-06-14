#!/usr/bin/env bash
set -e

if [ $# -eq 0 ]; then
    echo -e "Usage:\n  $0 <instance_id>\nWhere <instance_id> should be unique for each call, for example:\n  $0 first"
    exit 1
fi

cd $(dirname ${BASH_SOURCE[0]})

export BOOTSTRAP_CLIENT_IP=$(docker inspect -f "{{.NetworkSettings.Networks.spartan.IPAddress}}" node-template-spartan)
export INSTANCE_ID="$1"
export COMPOSE_PROJECT_NAME="spartan-$INSTANCE_ID"

stop() {
  docker-compose down -t 3 || /bin/true
  docker volume rm spartan-farmer-$INSTANCE_ID
}

trap 'stop' SIGINT

docker-compose pull

docker volume create spartan-farmer-$INSTANCE_ID
docker run --rm -it \
  --name spartan-farmer-$INSTANCE_ID \
  --mount source=spartan-farmer-$INSTANCE_ID,target=/var/spartan \
  subspacelabs/spartan-farmer plot 256000 spartan

docker-compose up
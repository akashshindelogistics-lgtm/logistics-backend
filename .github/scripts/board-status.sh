#!/usr/bin/env bash
# Set an issue's Status on the owner's project board (project #1 by default).
# Usage: board-status.sh <issue-number> <status name, e.g. "In Progress">
# GH_TOKEN must be a token with the `project` scope: the workflow's own
# GITHUB_TOKEN can't edit a project owned by a user account.
set -euo pipefail
[ -n "${GH_TOKEN:-}" ] || { echo "PROJECT_TOKEN secret not set; board not updated"; exit 0; }

issue=$1 status=$2
owner=$GITHUB_REPOSITORY_OWNER project=${PROJECT_NUMBER:-1}

# Adding an issue that's already on the board returns its existing item.
item=$(gh project item-add "$project" --owner "$owner" \
  --url "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/issues/$issue" --format json --jq .id)
project_id=$(gh project view "$project" --owner "$owner" --format json --jq .id)
read -r field option < <(gh project field-list "$project" --owner "$owner" --format json \
  --jq ".fields[] | select(.name == \"Status\") | .id + \" \" + (.options[] | select(.name == \"$status\") | .id)") || true
[ -n "${option:-}" ] || { echo "no Status option named '$status'"; exit 1; }

gh project item-edit --id "$item" --project-id "$project_id" --field-id "$field" --single-select-option-id "$option"
echo "issue #$issue -> $status"
